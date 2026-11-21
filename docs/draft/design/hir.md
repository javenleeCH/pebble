# HIR 设计草案

> 对应 crate：`pebble-hir`。HIR 是前端语义层与后端代码生成之间的**唯一边界**：所有后端（Cranelift / MLIR / LaTeX）统一从 HIR 输入，禁止 AST 直连后端。

## 1. 设计目标

- **后端无关**：不包含任何后端 IR 指令、调用约定或内存模型假设。
- **Schema 附着**：每个节点携带输出 Schema，类型检查结果在 lowering 阶段固化。
- **DAG 结构**：节点 + 输入边，保留流水线的依赖关系，为算子融合、下推、并行调度留空间。
- **可序列化**：阶段二提供 HIR 序列化（文本 / JSON），用于调试与跨后端一致性验证。

## 2. 类型与 Schema

```rust
pub enum Type {
    I64,
    F64,
    Bool,
    String,
    Opt(Box<Type>),
}

pub struct FieldSchema {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
}

pub struct Schema {
    pub fields: Vec<FieldSchema>,
}
```

## 3. HIR 结构

```rust
pub type NodeId = u32;

pub struct Hir {
    pub nodes: Vec<HirNode>,
    pub entry: NodeId,
}

pub enum HirNode {
    Source(SourceNode),
    Transform(TransformNode),
    Aggregate(AggregateNode),
    Terminal(TerminalNode),
}

pub struct NodeBase {
    pub id: NodeId,
    pub inputs: Vec<NodeId>,
    pub output: Schema,
    pub span: Span,
}
```

### 3.1 源节点

```rust
pub struct SourceNode {
    pub base: NodeBase,
    pub kind: SourceKind,
}

pub enum SourceKind {
    Table { path: String, alias: Option<String> },
    MemoryTable { name: String, alias: Option<String> },
}
```

### 3.2 变换节点

```rust
pub struct TransformNode {
    pub base: NodeBase,
    pub kind: TransformKind,
}

pub enum TransformKind {
    Filter { pred: THirExpr },
    Map { record: Vec<Binding<THirExpr>> },
    Select { fields: Vec<THirExpr> },
    Distinct,
    Sort { keys: Vec<SortKey<THirExpr>> },
    Take { n: i64 },
    Join { right: NodeId, on: THirExpr },
}
```

### 3.3 聚合节点

```rust
pub struct AggregateNode {
    pub base: NodeBase,
    pub group_keys: Vec<THirExpr>,
    pub bindings: Vec<AggBinding<THirExpr>>,
}
```

### 3.4 终端节点

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

## 4. 类型化表达式

HIR 中的表达式携带推导后的类型，后端无需再做类型推断：

```rust
pub enum THirExpr {
    Literal { value: Literal, ty: Type, span: Span },
    Field { alias: Option<String>, name: String, ty: Type, span: Span },
    Binary { op: BinOp, left: Box<THirExpr>, right: Box<THirExpr>, ty: Type, span: Span },
    Unary { op: UnaryOp, expr: Box<THirExpr>, ty: Type, span: Span },
    If { branches: Vec<(THirExpr, THirExpr)>, else_: Box<THirExpr>, ty: Type, span: Span },
    Call { name: String, args: Vec<THirExpr>, ty: Type, span: Span },
}
```

## 5. AST → HIR 映射

| AST 构件 | HIR 节点 / 表达式 | 说明 |
|---|---|---|
| `SourceOp` | `SourceNode` | Schema 来自数据源元信息 |
| `filter` | `TransformNode::Filter` | 谓词类型校验为 `bool` |
| `map` | `TransformNode::Map` | 记录字段按 05 章规则叠加覆盖 |
| `select` | `TransformNode::Select` | 输出 Schema 仅含列出字段 |
| `distinct` / `sort` / `take` | 对应 `TransformKind` | Schema 不变 |
| `join` | `TransformNode::Join` | 右表登记为独立 Source 节点，`right` 指向其 NodeId |
| `group_by` + `agg` | 合并为单个 `AggregateNode` | 分组键与聚合绑定同节点携带 |
| `write` / `collect` / `print` | `TerminalNode` | 流水线唯一出口 |
| `Expr` | `THirExpr` | 附加类型与 Span |

## 6. 不变量与校验

Lowering 完成后，HIR 必须满足：

1. **DAG 无环**：沿 `inputs` 可达，不存在回边。
2. **单入口**：`entry` 指向源节点；除源节点外每个节点至少有一个输入。
3. **Schema 闭合**：每个表达式引用的字段都存在于其输入节点 Schema；节点输出 Schema 与算子规则一致。
4. **终端唯一**：只有一个终端节点，且无后继。
5. **类型闭合**：所有 `THirExpr.ty` 与运算符 / 函数签名兼容；可选类型按 05 章规则标注。
6. **无语法残留**：HIR 中不存在 Lambda 形参绑定、语法糖包装；Lambda 已展开为逐行表达式树。

## 7. 与后端的接口边界

- 后端 Trait（`pebble-backend`）输入为 `&Hir`，不接受 AST。
- 后端可读取节点 Schema 与类型化表达式，但不得回写 HIR；优化如需重构 DAG，在 HIR 层以 pass 重建。
- 阶段一 Cranelift 后端直接遍历 HIR 生成 CLIF；阶段三 MLIR / LaTeX 后端遵循同一接口。
