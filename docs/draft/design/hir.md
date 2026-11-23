# HIR 设计草案

> 对应 crate：`pebble-hir`。HIR 是前端语义层与所有后端之间的**唯一 typed Logical 边界**：DuckDB SQL、后续 Polars、LaTeX 及未来原生受控执行器统一从 HIR 输入，禁止 AST 直连后端。M0/M1 只有 DuckDB SQL 产品实现，但接口不得把 DuckDB SQL 或外部引擎的 Physical Plan 塞回 HIR。

## 1. 设计目标

- **后端无关**：不包含 SQL 方言、Polars 表达式、CLIF/MLIR、调用约定或内存布局。
- **Schema 附着**：每个节点携带输出 Schema；可空性只由 `Type::Opt` 表达。
- **稳定字段身份**：字段引用不依赖裸名字或源别名，join 重名、投影和覆盖后仍能区分来源。
- **DAG 结构**：节点与输入边表达数据依赖；`sort/take` 显式保留顺序约束。
- **规范化与内容寻址**：所有语义 pass 完成后生成 canonical HIR；格式、注释和诊断位置不影响 hash。
- **证据链边界**：同一 canonical HIR 同时驱动执行工件生成和审计视图；manifest/receipt 绑定实际执行请求，不冒充外部引擎等价性证明。

## 2. 类型、Schema 与字段身份

```rust
pub enum Type {
    I64,
    F64,
    Bool,
    String,
    Opt(Box<Type>),
}

pub type SourceId = u32;
pub type FieldId = u32;

pub struct FieldSchema {
    /// 单次 canonical HIR 内稳定且唯一的语义身份。
    pub id: FieldId,
    /// 对外显示/绑定名；同一节点是否允许重名由算子和输出边界规则决定。
    pub name: String,
    pub ty: Type,
    pub origin: FieldOrigin,
}

pub enum FieldOrigin {
    Source { source: SourceId, source_field: String },
    Derived { node: NodeId, binding: String },
}

pub struct Schema {
    /// 顺序即用户可见输出列顺序。
    pub fields: Vec<FieldSchema>,
}
```

不存在 `FieldSchema.nullable`。例如 `?i64` 只写为 `Type::Opt(Box::new(Type::I64))`，禁止两套可空标记互相失真。

字段身份规则：

1. 源字段获得稳定 `FieldId` 与 `FieldOrigin::Source`；源别名仅是 lowering 环境中的解析键，不进入字段身份。
2. `extend` 未覆盖字段保留身份；覆盖/新增绑定获得新身份，旧值在覆盖后不可再按原名访问。
3. `select as` 可改变显示名，不通过字符串名字合并两个来源不同的字段。
4. join 保留左右字段身份，即使输出显示名相同也不能混淆；进入不允许重名的边界前必须显式重命名或拒绝。
5. 分组键与聚合绑定按输出规则获得明确身份和名字，禁止隐藏自动改名。
6. `THirExpr::Field` 只保存解析后的 `FieldId`；名字和别名可从 Schema/Span 找回用于诊断，不参与表达式解析。

## 3. HIR 结构

```rust
pub type NodeId = u32;

pub struct Hir {
    pub nodes: Vec<HirNode>,
    /// 唯一终端节点；数据依赖边由终端反向可达全部有效节点。
    pub root: NodeId,
    pub semantics_version: String,
}

pub struct NodeBase {
    pub id: NodeId,
    pub inputs: Vec<NodeId>,
    pub output: Schema,
    /// 仅用于诊断；canonical 序列化时剔除。
    pub span: Span,
}

pub enum HirNode {
    Source(SourceNode),
    Transform(TransformNode),
    Aggregate(AggregateNode),
    Terminal(TerminalNode),
}
```

`root` 不再指向源节点。含 join 的 DAG 可以有多个源叶子，但只能有一个终端根；所有非源节点至少有一个输入。

## 4. 源节点

```rust
pub struct SourceNode {
    pub base: NodeBase,
    pub source: SourceId,
    pub kind: SourceKind,
}

pub enum SourceKind {
    Table {
        locator: String,
        schema_lock_hash: Hash,
    },
    MemoryTable {
        name: String,
        schema_lock_hash: Hash,
    },
}
```

Schema 来自已锁定的不可变快照，而非 lowering 时临时读取文件。路径/名称是定位符；数据版本或内容身份另外进入 Execution Manifest。运行时实际 Schema 与快照不一致必须失败，不允许隐式 widening 或 `union_by_name`。

## 5. 变换节点

```rust
pub struct TransformNode {
    pub base: NodeBase,
    pub kind: TransformKind,
}

pub enum TransformKind {
    Filter { pred: THirExpr },
    Extend { bindings: Vec<Binding<THirExpr>> },
    Select { projections: Vec<Projection> },
    Distinct,
    Sort { keys: Vec<SortKey<THirExpr>> },
    Take { n: i64 },
    /// inputs 必须依次为左、右输入；当前 MVP 右输入由源节点构成。
    Join { on: THirExpr },
}

pub struct Binding<T> {
    pub output: FieldId,
    pub name: String,
    pub value: T,
}

pub struct Projection {
    pub expr: THirExpr,
    pub output: FieldId,
    pub name: String,
}
```

- `Extend` 是扩展算子 ε，所有 RHS 同时对同一输入求值，兄弟绑定互不可见。
- `Select` 是投影 π，输出恰好等于 `projections`，顺序保持一致。
- `Filter` 只保留谓词为 `true` 的行，并进行当前节点输出 Schema 的流敏非空收缩。
- `Join` 只保留 `on=true` 的行对；普通等值 join 不匹配 null 键。
- `Sort` 所有键统一 `nulls last`；`Sort/Take` 是顺序哨兵，不允许外部优化假设被 Pebble 静默丢弃。

## 6. 聚合节点：分组与全局统一表示

```rust
pub struct AggregateNode {
    pub base: NodeBase,
    /// 空数组表示全局聚合，而不是缺失 AggregateNode。
    pub group_keys: Vec<GroupKey>,
    pub bindings: Vec<AggBinding>,
}

pub struct GroupKey {
    pub value: THirExpr,
    pub output: FieldId,
    pub name: String,
}

pub struct AggBinding {
    pub output: FieldId,
    pub name: String,
    pub call: AggCall,
}

pub struct AggCall {
    pub func: AggFunc,
    pub arg: Option<THirExpr>,
}

pub enum AggFunc { Count, Sum, Min, Max, Avg }
```

聚合语义约束：

1. `group_by + agg` lowering 为一个节点；输入始终是完整分组关系，不是只剩分组键的中间行。
2. 分组聚合空输入产生零行；全局聚合任意输入恰好一行，空输入形成空全局组。
3. `count()`/`count(x)` 恒为 `i64`，空全局组均为 0；`count(x)` 只计非空值。
4. `sum/min/max/avg` 的可空性由空组、全 null 和 checked 溢出共同决定，不能按函数名硬编码为非空。
5. 聚合参数引用组内输入行，分组键是组级值；同一节点的聚合输出互不可见。
6. 聚合后只暴露分组键与聚合输出。全局聚合的聚合函数之外不得引用输入行字段。
7. 聚合后的过滤使用普通 `Filter`；未来 `having` 若启用，也必须 lowering 到语义等价且可审计的结构。

## 7. 终端节点

```rust
pub struct TerminalNode {
    pub base: NodeBase,
    pub kind: TerminalKind,
}

pub enum TerminalKind {
    Write { path: String },
    Collect,
    Print,
}
```

终端是显式宿主能力/效应边界。终端选项同时进入 canonical HIR 和 manifest，不能在 codegen 后另加未审批的写入目标或输出配置。

## 8. 类型化表达式

```rust
pub enum THirExpr {
    Literal { value: Literal, ty: Type, span: Span },
    Field { field: FieldId, ty: Type, span: Span },
    Binary {
        op: BinOp,
        left: Box<THirExpr>,
        right: Box<THirExpr>,
        ty: Type,
        span: Span,
    },
    Unary { op: UnaryOp, expr: Box<THirExpr>, ty: Type, span: Span },
    If {
        condition: Box<THirExpr>,
        then: Box<THirExpr>,
        else_: Box<THirExpr>,
        ty: Type,
        span: Span,
    },
    Call { name: BuiltinFn, args: Vec<THirExpr>, ty: Type, span: Span },
}
```

HIR 中不存在 Lambda 形参、语法糖或未解析字段名。`if` 只有一个条件与两个分支；条件仅 `true` 选择 then，`false/null` 选择 else。

普通算术/比较严格传播 null，`x == null`/`x != null` 必须 lowering 为独立判空内置形式，不能留给后端解释。可能除零、溢出或定义域失败的表达式，其静态 `ty` 必须包含可空性。所有 Span 仅用于诊断，不进入 canonical hash。

## 9. AST → HIR lowering

| AST 构件 | HIR 结果 |
|---|---|
| `SourceOp` | `SourceNode`，绑定 Schema lock，分配 SourceId/FieldId |
| `filter(expr)` | `Filter`，解析字段身份并做输出非空收缩 |
| `extend(record)` | 单个 `Extend`，绑定同时求值 |
| `select(items)` | 单个 `Select`，保留项目顺序和显式派生命名 |
| `join(right, on = expr)` | 两个输入的 `Join`，保留左右来源身份 |
| `group_by + agg` | 一个非空 `group_keys` 的 `AggregateNode` |
| 裸 `agg` | `group_keys = []` 的全局 `AggregateNode` |
| `if c { a } else { b }` | 单条件 `THirExpr::If` |
| 终端 | 唯一 `TerminalNode`，并成为 `Hir.root` |

## 10. HIR 不变量

1. DAG 无环；从唯一终端根可达全部有效节点，无未引用节点。
2. 源节点无输入；其他节点输入数符合算子签名；join 输入顺序固定为左、右。
3. 每个 `FieldId` 唯一，引用的字段必须存在于对应输入作用域；禁止按裸名字补救解析。
4. 节点输出 Schema 与算子规则一致，字段顺序稳定；可空性只由 `Type::Opt` 表示。
5. 表达式类型闭合，所有 checked-null 风险反映在静态类型中。
6. 无 Lambda、记录一等值、未展开语法糖、聚合越界或终端后算子。
7. `sort/take` 的相对顺序及所有可观察顺序依赖被保留。
8. 后端不得回写 HIR；Pebble 自有优化 pass 必须重建并重新校验 HIR，最终 codegen 与审计视图使用同一个 canonical 版本。

## 11. Canonical HIR 与内容寻址

`Hir` 是编译期表示，发布前必须生成独立的规范序列化字节 `canonical_HIR`：

- 节点按从源到终端的规范拓扑序编号；同等候选按结构键排序，不能依赖 `HashMap` 迭代或分配地址。
- Schema 字段顺序、FieldId/SourceId、表达式结合结构、显式类型、字面量位模式和终端选项均进入序列化。
- 剔除 Span、注释、空白、源别名拼写、时间戳、绝对临时路径和诊断缓存。
- 格式/注释变化应得到相同 hash；常量、类型、Schema、算子顺序或顺序哨兵变化必须得到不同 hash。
- 序列化格式和语义均版本化；浮点编码采用明确位模式，不能使用本地化字符串格式。

```text
hir_hash = SHA-256("pebble-canonical-hir-v1\0" + canonical_HIR)
```

哈希提供确定性与篡改检测，不单独提供真实性、签名或业务正确性证明。

## 12. BackendArtifact / Manifest / Receipt 边界

### 12.1 编译与执行分离

```rust
pub struct CompiledUnit {
    pub canonical_hir: Vec<u8>,
    pub hir_hash: Hash,
}

pub enum BackendArtifact {
    DuckDbSql { canonical_sql: String },
    // 后续新增 Polars 等变体；M0/M1 不实现。
}

pub struct ExecutionManifest {
    pub hir_hash: Hash,
    pub schema_lock_hashes: Vec<Hash>,
    pub dataset_identities: Vec<DatasetIdentity>,
    pub semantics_version: String,
    pub compiler_version: String,
    pub backend: BackendIdentity,
    pub artifact_hash: Hash,
    pub engine_config: CanonicalConfig,
    pub terminal_options: CanonicalTerminalOptions,
}

pub struct ExecutionReceipt {
    pub manifest_hash: Hash,
    pub submitted_artifact_hash: Hash,
    pub actual_engine: BackendIdentity,
    pub input_validation: Vec<InputValidation>,
    pub status: ExecutionStatus,
    /// 外部引擎 EXPLAIN 等只能作为可选观测附件。
    pub observed_engine_plan: Option<EnginePlanAttachment>,
}
```

后端编译器只负责 `CompiledUnit → BackendArtifact`；执行适配器负责提交同一工件并生成 receipt。driver/verifier 核对：

```text
canonical HIR
  + Schema locks + dataset identities
  + BackendArtifact
  + backend/config/terminal options
  = ExecutionManifest

已审 manifest
  vs. 本次实际提交工件与执行 receipt
```

MVP 保证的是证据链完整性和执行请求一致性。DuckDB/Polars 可自行重写逻辑与物理计划，属于指定版本后端的信任基；不得宣称已审 HIR 等于实际物理计划。

### 12.2 多后端与原生路线

- **M0/M1**：只实现 `BackendArtifact::DuckDbSql`，接口保持后端中立；不建设双后端一致性框架。
- **后续**：Polars 作为独立 BackendArtifact/codegen，必须显式声明语义差集和能力检查，不能默认与 DuckDB 等价。
- **M4 研究/原生受控执行后端**：Cranelift/MLIR 保留在路线图中；只有该路线才引入 Pebble 自有 Physical Plan、列式内核与必要 storage。届时路径为 `HIR → Pebble Physical Plan → native BackendArtifact`，Physical Plan 不反向污染 Logical HIR。
- **M0/M1 不做**：自建 Physical Plan、Arrow 全栈或自研 parquet/storage。Arrow 仅在后续宿主交换确有需要时引入。
