# Pebble 设计定稿（目标 / 边界 / 功能 / 语法）

> 状态：**v3 定稿**（2026-11-23，第二轮专家评审后修订）。取代 v2 及旧草稿中的冲突表述。
> 评审依据：`docs/review-2026-11-23.md`、`docs/review-r2-2026-11-23.md`。
> 当前仍是设计定稿，**原型尚未开始，不得把目标描述为已验证能力**。
> 原则：每个保留特性必须有唯一语义；「可信」必须拆成可验证性质、明确假设与信任基。

---

## 0. 定位（收窄后的世界级主张）

> **Pebble 是面向离线批结构化数据的可信变换语言与嵌入式编译器 SDK：把源码、Schema 快照、规范化逻辑计划、生成的执行工件和运行回执连接成可独立核对的证据链。**

Pebble 不与 DuckDB 比速度、不与 Polars 比算子数量，也不宣称哈希或 LaTeX 能证明业务结果正确。其差异化是：比任意 SQL/宿主脚本更小的表达与攻击面、唯一且可测试的数据语义、可复核的逻辑计划，以及未来可受控嵌入的宿主边界。

一句话理论方向（目标而非既成事实）：**最小的一阶、带依赖细化、具有可判定等价片段的有限多重集查询语言。**

---

## 1. 目标与保证矩阵

### 1.1 用户与产品形态

- **首要用户**：需要在数据平台中接受并执行受限用户变换的低代码/数据产品厂商。
- **溢价用户**：需要可审计、可复现数据管线的受监管团队。
- **产品形态**：编译器库 + Schema/策略校验 + 可插拔多后端适配器（DuckDB 先行）+ 受控宿主 SDK + 证据工件 + 独立 verifier。
- **不是**：代理服务、通用查询引擎、容器/微虚拟机的替代品。

### 1.2 保证矩阵（禁止混用「终止 / 安全 / 完整性 / 正确性」）

| 性质 | MVP 能保证什么 | 信任基 / 边界 |
|---|---|---|
| 语法与类型安全 | 保留语法有唯一静态类型；字段/作用域/Schema 错误编译期拒绝 | 编译器实现需测试与审计 |
| 终止性 | 无用户无界控制流；迭代由有限集合算子完成 | **不等于有界时间/内存，不等于沙箱** |
| 计划确定性 | 相同源码、Schema lock、语义与编译器版本产生相同 canonical HIR/hash | 规范序列化，禁止时间戳/HashMap 顺序进入摘要 |
| 审批完整性 | HIR、Schema、生成 SQL、后端版本等可绑定到 manifest；篡改可检测 | 真实性还需签名与可信分发，MVP 不自动提供 |
| 执行请求一致性 | receipt 可核对本次提交的执行工件与已审 manifest | DuckDB 可自行重写逻辑/物理计划 |
| 数学可读性 | 同一 HIR 可导出关系代数/LaTeX 视图 | **LaTeX 不是执行正确性证明** |
| 结果正确性 | 不证明业务逻辑「算对了」；仅以语义一致性测试降低编译/后端差集 | 编译器与指定版本 DuckDB 在 TCB 内 |
| 安全嵌入 | MVP 只缩小不可信代码面，不承诺进程内硬隔离 | 取消、OOM、配额、故障隔离与恶意负载测试未完成 |

对外措辞固定为：

> **「审的 HIR、Schema、生成 SQL与本次执行请求可密码学绑定；执行语义由指定版本的 DuckDB 承担。」**

不得宣称「已审 HIR 就是实际执行的物理计划」或「跨 DuckDB 版本结果必然一致」。

---

## 2. 边界与红线

### 2.1 适用 / 不适用

- **适合**：离线批过滤、派生、投影、关联、分组聚合；需静态校验、计划审计和确定性证据的变换。
- **不适合**：实时无限流、通用算法/递归、任意 IO/网络、异构文本抽取、业务状态机、复杂半结构与窗口分析（后置）。

### 2.2 五条红线

1. **总性红线**：无用户递归/循环/无界迭代；数据源必须有限。终止性不冒充资源安全。
2. **一阶纯函数红线**：无一等/高阶函数；MVP 无任意 UDF。未来仅考虑带契约与预算的受信函数，任意代码放在 Pebble 外隔离执行。
3. **线性数据流红线**：单入口单出口；`if` 只能值选择，不能选择流水线结构。
4. **受控效应红线**：变换核心无用户可见任意副作用；输入输出只能经显式宿主能力边界，`Table` 是输入能力，`write/print` 是受控终端效应。
5. **语义封闭红线**：表达式取值和算子行为由规范唯一确定；禁止「由实现定义」。

---

## 3. 功能与交付路线

### 3.1 M0：杀手级 Demo——审批后不可偷换

用固定 MemoryTable 数据走通：

```pebble
MemoryTable("orders") as o
  .filter(o.status == "paid" && o.amount != null)
  .group_by(o.customer_id)
  .agg(
    order_count = count(),
    revenue = sum(o.amount),
  )
  .filter(revenue >= 10000)
  .select(customer_id, order_count, revenue)
  .sort(revenue desc)
  .take(20)
  .collect()
```

四幕验收：

1. `pebble build`：输出 canonical HIR hash、DuckDB SQL、最小单向 LaTeX/代数视图。
2. `pebble run --receipt run.json`：指定 DuckDB 执行，记录 manifest、实际 SQL hash、引擎版本和输入校验结果。
3. `pebble verify`：核对已审工件与本次执行请求一致。
4. 反例：只改格式/注释，hash 不变；改 `10000→9999` 或篡改 receipt，verify 失败；字段拼错则编译期拒绝且不生成计划。

### 3.2 MVP v0.1：2–4 周可验证最小核

**保留**

- 流水线：`MemoryTable`（`Table` 稳定则保留）→ `filter` → `extend` → `group_by + agg` / 全局 `agg` → `select + as` → `sort + take` → `collect`。
- 聚合：`count/sum/min/max/avg`。
- 严格字符串函数：`lower/upper/trim/length/contains`；`length` 明确定义为 Unicode 标量值数量还是字素簇，MVP 只能选一种并测试。
- 全部保留语法的 P0/P1 语义、编译期诊断、canonical HIR/hash、Schema lock、manifest、DuckDB SQL codegen、receipt、verify。
- 空表、全 null、除零、溢出、Unicode、重复编译、语义扰动与回执篡改测试。

**延期**

- v0.1.1：~~`having`、少量有明确转换矩阵的 `cast`~~（已完成，教程第 11 章）、更多字符串函数、join/distinct/write/print 硬化、完整 Schema 版本仓库。
- 更晚：通用 `cast`、全部时间函数、`coalesce/nullif/ifnull`、`substr`、`split`（必须与数组一起）、LaTeX 往返、完整资源配额/沙箱。MLIR 原生后端按 §3.3、§6.4 路线持续推进。
- **路线修订（2026-11-28）**：Polars 第二外部后端**取消**——原生后端直上 MLIR；跨后端语义差集测试矩阵改由「DuckDB 后端 vs 差分参考实现」承担，并在 MLIR 后端落地时扩展为「DuckDB vs MLIR」对拍。

原则：**砍功能，不砍保留功能的语义。**

### 3.3 后续里程碑

- **M2**：证据工件签名（ed25519）与独立 verifier；canonical HIR 反序列化 roundtrip；LaTeX 往返仅在语义覆盖完整后开展。
- **M3**：宿主 SDK、取消/配额/输出限制、恶意查询测试；Arrow 仅作为必要的宿主交换格式引入。
- **M4：MLIR 原生执行后端（主路线）**：`HIR → Pebble Physical Plan → native BackendArtifact`（工件哈希对象为 canonical 物理计划文本，非机器码）；先完成表达式 JIT 与受控执行原型，再按真实负载和可信性需求决定是否建设完整原生执行器；届时引入列式内核与 storage。**Cranelift 降为研究对照**（非产品路线），保留 crate 供基准对比。
- **M5**：2–3 个设计伙伴共创，以证据链或安全嵌入的单一必杀指标验收。

---

## 4. 语法与作用域

### 4.1 已定语法

1. 保留 `.` 方法链，不换管道符。
2. `map` 改名 `extend`，是扩展算子 ε；`select` 是投影 π。
3. `extend` 唯一接受记录字面量：`extend({ a = expr, b = expr, })`；记录字面量不得出现在其他表达式位置。
4. 绑定统一 `=`：`join(..., on = expr)`；`on` 是 join 固定绑定名，不是任意命名参数。
5. `select(expr as name, ...)` 支持派生命名；无 `as` 项只能是字段引用，复合表达式必须命名。
6. 多行列表和记录统一允许尾逗号。
7. 废除 Lambda `|u|`：单源可裸字段，多源用源别名限定。
8. 修复 `ifExpr`：文法必须是 `if expr { expr } else { expr }`，不得把条件放进 `{}`。

### 4.2 静态解析域

- 源别名与输出字段名属于不同命名空间；裸字段只有当前可见 Schema 中名称唯一时才能解析，否则报歧义，禁止默认取左表。
- join 保留字段来源身份；进入不允许重名的输出边界前必须用 `select as` 消除重名。
- `extend` 的所有 RHS 同时对输入行求值，兄弟绑定互不可见；覆盖后旧值不可再按原名访问。
- `select` 后可见 Schema 恰好等于输出字段；被裁字段不得再经源别名访问。
- `agg` 内部：聚合参数引用组内输入行，分组键是组级值；同一 agg 的输出绑定互不可见。
- `agg` 后：只能引用分组键与聚合输出，不得再引用 `o.amount` 等输入行字段。
- MVP 的 `group_by` 键只接受字段引用；`group_by(u.first)` 默认输出 `first`，去限定后重名必须显式经前置派生/投影消除或拒绝，禁止自动改名。表达式分组键及 `as` 命名后置。
- 全局 agg 中，聚合函数之外不得引用输入行字段。

### 4.3 文法同步要求

新版 EBNF 至少包含：`extend(recordLiteral)`、`filter(expr)`、`selectItem = expr [as ident]`、`join(..., on = expr)`、全局 agg、`having(expr)` 与 `cast(expr as T)`（v0.1.1）、统一尾逗号及正确 ifExpr。旧 `|` 运算符和 Lambda 小节删除。

---

## 5. P0/P1 语义规范（必须完整落入正式 Spec）

### 5.1 null、比较与谓词

- 除判空测试和逻辑连接词外，普通算术/比较是严格运算：任一输入 null，结果 null；普通比较静态类型为 `?bool`。
- `x == null` / `x != null` 是独立判空形式，恒返回非空 `bool`，等价 `is_null(x)` / `!is_null(x)`；不得按普通相等解释。
- 强 Kleene 三值逻辑：`!null=null`；`false && x=false`、`true && x=x`；`true || x=true`、`false || x=x`；其余含空情况按真值表。`&&`/`||` 是严格 null 传播的显式例外。
- `filter`、`having`、join `on`、`if` 条件及聚合接受 `bool/?bool`；只有 `true` 成立，`false/null` 均不成立。if 只选择条件为 true 的分支。

### 5.2 值同一性、join、分组与排序

- join 仅保留 on=true 的行对，普通等值 join 不匹配 null 键。
- `group_by` 与 `distinct` 使用独立「值同一性」：null 与自身同一，非空值按类型规则比较；不得复用普通 `==`。
- `sort` 所有键统一 `nulls last`，不随 asc/desc 反转；asc/desc 只反转非空值顺序。
- f64 必须另行定义 NaN、±∞、±0.0 的比较、确定性排序全序和 group/distinct 值同一性；未定义前不得让后端自行决定。

### 5.3 流敏非空收缩

对谓词做「求值为 true 时的必要非空条件」分析：

- 识别 `f != null`、`!(f == null)`、`!is_null(f)`，先消除括号并解析为稳定字段身份。
- 合取的非空保证取并集；析取取交集；if 仅当所有可能产生 true 的路径均保证非空时传播。
- 分析可靠但不完备；无法证明就保持 `?T`，不追求布尔公式完全等价。
- 收缩只作用于当前 filter（及 having）的输出 Schema，不跨 extend/select/join/agg 自动逆向传播。

### 5.4 部分函数：checked-null，不饱和、不抛用户异常

- 整数溢出、`abs(i64::MIN)`、`-i64::MIN`、`i64::MIN / -1`、`/0`、`%0` 均返回 null。
- 所有可能失败的运算，其静态返回类型必须包含可空性；禁止返回类型为 `i64` 而运行时临时产生 null。
- 禁止饱和值静默代替精确结果；编译期可确定的非法常量直接报编译错误。
- 用户表达式定义域失败统一以 null 表示并体现在类型中；IO、OOM、取消、配额耗尽是执行状态，不得静默转换为字段 null。

### 5.5 聚合签名与空输入

- `group_by + agg` 是单个聚合节点，输入是完整分组关系，输出为「分组键 + 聚合字段」。
- 分组聚合：空输入产生零组、零行；不存在人为创造的空分组。
- 全局聚合：任意输入恰好一行；空输入形成空全局组。
- `count()`/`count(x)` 恒 `i64`，空全局组均为 0；`count(x)` 只计非空值。
- `avg` 无非空输入时 null，可能无值时类型 `?f64`；`min/max` 同理。
- `sum` 空集合默认 null；checked 整数 sum 可能溢出，因此结果可空。
- 可空性由「全局组是否可能为空 × 参数是否可能全 null × 是否可能溢出」决定，不按函数名一刀切。
- 可空聚合结果可直接比较、运算和 sort；需要非空时由用户显式判空/未来 coalesce，不提供隐式或异常式强制解包。

### 5.6 HIR 与 Schema 一致性

- 删除 `FieldSchema.nullable`，可空性只由 `Type::Opt` 表达。
- 明确 extend 覆盖落位、select 输出顺序、join 字段身份、group 键命名；禁止隐藏自动改名。
- `sort/take` 是代数外表示层和顺序哨兵；自由重排只适用于无顺序依赖的代数片段。
- 采用多重集关系代数；`extend`=扩展投影 ε，`select`=π。可判定等价与半环 provenance 只对明确限定片段主张，不外推到完整 MVP。

---

## 6. 技术路线与证据链

### 6.1 分阶段多后端架构

**统一语义边界始终是 canonical typed HIR；每个后端生成独立、可审计的 BackendArtifact。**

首发路径：

```text
Pebble source
  → typed canonical HIR (Logical)
  → BackendArtifact::DuckDbSql
  → specified DuckDB version executes
  → ExecutionReceipt
```

分阶段扩展：

1. **M0/M1：DuckDB SQL 首发后端**，先闭环证据链与语义一致性，不另写解释器。
2. **M4：MLIR 原生后端（主路线）**，引入 Pebble Physical Plan；`artifact_hash` 对 canonical 物理计划文本（MLIR 的输入）计算，不对机器码（Cranelift/MLIR 输出非 bit 稳定）。原生后端可直接控制物理执行，因此审计边界与外部引擎后端分别定义。
3. **Cranelift：研究对照**（2026-11-28 降级），保留 crate 用于 JIT 基准对比，非产品路线。

M0/M1 不做：多后端同时首发、另写解释器、Arrow 全栈、自研 parquet/storage。外部引擎观测到的 EXPLAIN 只能作为 receipt 附件，不冒充 Pebble 决策层。MLIR 原型在 M4 前可独立构建与跑基准，不阻塞首发后端。

### 6.2 Schema lock 工作流

```text
pebble schema lock
  读取数据源元数据 → 生成 schema.lock（字段顺序/类型/nullability/映射/规范版本/hash）

pebble build/run --locked
  编译只信任 lock；运行前验证实际 Schema 精确一致；不一致即失败
```

路径只是定位符，不是版本。MVP 限单文件、无 glob、无隐式 widening/union_by_name。需要结果复现时必须绑定不可变 dataset version/内容身份，并防 TOCTOU；MemoryTable 同样需要宿主注册的不可变 Schema。

### 6.3 Execution Manifest / Receipt

```text
manifest_hash = H(
  canonical_HIR + schema_lock_hash + dataset_identity
  + semantics_version + canonical_SQL
  + backend_name/version + engine_config + terminal_options
)
```

receipt 记录 manifest hash、实际提交 SQL hash、引擎版本、输入校验与执行状态。MVP 保证完整性与可核对性；签名和可信分发后续补齐。

### 6.4 Crate 取舍

保留 parser、semantics、hir、diagnostic、driver、可插拔 backend 接口、最小 latex；新增 `pebble-duckdb`，Schema lock 先为模块。`pebble-cranelift` / `pebble-mlir` 保留在长期 workspace 路线与独立研究构建中，M0/M1 不进入默认交付依赖；M4 逐步接入 Physical Plan、原生 BackendArtifact 和性能/语义基准。

---

## 7. 下一步执行顺序

1. 写威胁模型与恶意查询测试集，明确 Pebble 与宿主各自防什么。
2. 按 §5 重写类型/语义/算子规范，并以第二轮反例做规范验收测试。**规范文本已同步，测试待实现。**
3. 同步 EBNF、AST/HIR 和 6 个样例，删除 Lambda、map、on:、错误 ifExpr 等旧债。**本轮已完成文档同步。**
4. 实现 canonical HIR/hash + Schema lock + DuckDB codegen + manifest/receipt/verify。
5. 用 M0 四幕 Demo 验收：重复编译一致、语义变更审批失效、篡改可发现、保留语法边界行为唯一。
6. 在单后端证据链闭环前，不扩展函数库、多后端或自研执行器。

### 7.1 执行状态（2026-11-23）

**M0 最小可验证核已实现并验收通过**（`cargo test` 全绿；端到端在 DuckDB 1.4.1 验证）：

- `pebble-diagnostic`：Span/错误码（LEX-001~004、PAR-001~007、SEM-001~017、SCH-001~006、RUN-001~004）与源码级渲染。
- `pebble-parser`：手写递归下降（原型实现选择；Chumsky 属路线草案），全部 P0 文法（extend/select as/on =/尾逗号/if 修正）。
- `pebble-hir`：typed HIR + 稳定 FieldId/SourceId + canonical 序列化/SHA-256 + Schema lock + BackendArtifact/Manifest/Receipt/verify。
- `pebble-semantics`：解析域、严格 null 传播、判空 lowering、Kleene、NN(p) 收缩、checked-null 与 SEM-016 常量定义域、聚合签名、历史作用域区分 SEM-001/012；多错误报告与毒化（第 9 章）。
- `pebble-duckdb`：canonical SQL（threads=1）、IS NOT NULL、HUGEINT checked 守卫、f64 isfinite/+0.0 规范化、round ties-to-even、NULLS LAST、group/distinct 值同一性。
- `pebble-latex`：最小单向 σ/ε/π/γ/τ/topN 审计视图。
- `pebble-driver`：`build` / `run` / `verify` / `lock` 四个子命令与四幕验收（含 TOCTOU 数据哈希、RUN-001 f64 预检、RUN-003 工件缺失）。
- `pebble-parser` + `pebble-diagnostic`（第 8 章）：解析器错误恢复——同步点 + 括号栈，多错误一次报告且一个根因只报一次（残骸模式下试探解析，恢复本身不制造诊断）；词法错误占位 token；`parse() -> Result<Program, PartialAst>` 以类型执行「带伤的 AST 不进语义层」；提示只说与所遇 token 相容的话；表达式嵌套（64 层）与原子数（256 个）上限，避免栈溢出；诊断按源码位置排序并附汇总行（行号二分查找，最多显示 100 条）；Span 统一为 UTF-8 字节偏移（修复中文注释导致的定位漂移/崩溃）。

四幕验收结果：重复编译三哈希一致；语义变更（10000→9999）旧审批失效；注释变更哈希不变；数据篡改（RUN-002）与非有限输入（RUN-001）均被拒绝；SEM-011 歧义诊断带候选提示。

工件版本 v2（2026-11-27，教程第 10 章）：v1 的 DuckDB 后端为每个算子生成一层嵌套子查询，实测连续约 300 个算子以上引擎无法执行（350–450 层时 DuckDB 自身栈溢出，500 层以上解析器报 `Max expression depth limit of 1000`）。v2 把连续的行级算子并进同一层 SELECT：`filter` → `WHERE`（AND 合成平衡树）、`extend` / `select` → 选择列表、`sort` → `ORDER BY`；`distinct` / `take` / 聚合是屏障，`join` 开新层但吸收其后的行级算子。版本标在 SQL 文件头（`artifact v2`）与工件哈希的域分隔前缀上，v1 与 v2 的工件哈希不可能相同，旧审批不会被误用。验证：44 个程序在新旧实现、同一 DuckDB 上输出逐字相同；单个算子的流水线 SQL 正文与 v1 逐字相同；2 000 个 `filter` 的端到端与独立计算一致。

语义阶段（第 9 章）一次报出全部独立的错误；定义失败的名字被毒化，其后果不再重复报告。

v0.1.1（2026-11-27，教程第 11 章）：`having` 是语法糖，lowering 成紧跟聚合的 `filter`，HIR 哈希与 `.agg(…).filter(…)` 相同；紧跟聚合的 `filter` 在 SQL 里并进聚合层的 `HAVING`（聚合屏障唯一的例外；HAVING 中的聚合输出引用替换为表达式，不依赖方言的别名引用）。这改变了「聚合后接 filter」的程序生成的 SQL——工件哈希随之改变，HIR 哈希不变。v2 尚未随任何发布固化，所以没有再升版本号；发布之后再发生这种形态改变，必须升版本。
`cast` 只允许 `i64→f64`、`f64→i64`（取偶取整，越界为 null）、`string→i64`、`string→f64`（严格文法）；可能失败的转换结果一律可空。取值规则的参考实现在 `pebble_semantics::cast`，后端 SQL 用守卫把引擎行为收敛到规范，二者在真实 DuckDB 上逐格对拍。新 HIR 表达式 `Cast` 以新标签 `V` 编码，既有编码不变，旧程序哈希不变。
同批修复（都是「对的程序编译不过」或「语义差集」）：常量表达式里的内置函数调用被误报为 SEM-016；混合类型的常量除零（`1.5 / 0`）未被诊断；`trim` 沿用 DuckDB 默认行为、不去制表符。
已知语义差集（未修）：`lower` / `upper` 的三处结果与规范的 Unicode 全映射不一致（DuckDB 实现为简单映射）。消除差集需要在 SQL 里为一百余个特殊码点写守卫，或把规范改为简单映射——属于语言设计决定，待定。

下一步（按序，2026-11-28 修订）：~~M2~~（✅ ch12）→ ~~威胁模型与恶意查询测试集~~（✅ ch13）→ 更多语义边界测试（Kleene 反例矩阵、除零/溢出动态用例）→ v0.1.1 剩余项（字符串函数、join/distinct/write/print 硬化、Schema 版本仓库）与 `lower`/`upper` 差集的决定 → ~~M3 宿主 SDK 与资源配额~~（✅ ch13）→ M4 MLIR 原生后端（主路线；Cranelift 研究对照）→ M5 设计伙伴。Polars 第二后端已取消。

### 7.2 M2 执行状态（2026-11-28，教程第 12 章）

**证据签名与独立 verifier 已实现并验收**（verifier 攻防矩阵 8 测全绿）：

- `pebble-hir::sign`：ed25519 审批密钥（`pebble keygen`）、manifest 主体签名（`pebble sign`）、公钥验签；未签名 manifest 的字节与 M0/v0.1.1 完全一致（`skip_serializing_if` 空签名），旧回执核对不受影响。
- `canonical.rs` 反序列化 + roundtrip：verifier 可从 canonical 字节重算 `hir_hash`，不经过编译器主路径；含计数防御与尾随垃圾拒绝。
- 新 crate `pebble-verifier`：六项独立检查（签名/工件/HIR roundtrip/回执绑定/数据快照/结构不变量），失败可定位到单项；运行时依赖面仅 `pebble-hir` + JSON，不加载 parser/semantics/codegen。
- 顺序契约：build → sign → run → verify（签名改变 manifest_hash，回执绑定的必须是已审版本）。
- CI 第七道门禁：verifier 攻防矩阵（篡改 plan.sql/canonical.hir/manifest 主体/数据/公钥逐一必被相应检查项抓到）。
- 已知边界：私钥管理、签名前的 build 机欺骗、执行侧自证性（M4 议题）——见 ch12「诚实边界」。

### 7.3 M3 执行状态（2026-11-28，教程第 13 章）

**资源配额与威胁模型已实现并验收**（budget 9 测 + verifier 第 7 项检查全绿）：

- Schema lock 新增 `max_rows`（宿主行数上界承诺，进主体哈希）：`pebble lock --max-rows N`。
- 静态成本模型（`pebble-driver/src/budget.rs`）：从源 max_rows 推导最坏情况输出行数；join 是唯一乘法算子（saturating u128）；take/聚合收窄；全局聚合=1。预算与配额进 manifest，**审批者在签名前就能看到最坏情况**。
- 配额：`build --quota-rows N` 超限即拒绝（SEM-018，span 指向肇事 join）；缺 max_rows 的源 + 配额 → 同样拒绝（不静默降级）。
- 超时：`run --timeout-secs N`，到期杀引擎 → RUN-004（轮询 try_wait + kill）。
- 内存上限：`SET memory_limit = '512MB';` 进 canonical SQL 前言 + `engine_config.memory_limit_mb` 进 manifest——运行时资源上限是被审内容。
- OOM 分类：DuckDB 报 "Out of Memory"/"memory limit" → RUN-004；其余引擎错误 → RUN-003。
- 威胁模型：`docs/threat-model.md`——12 格攻防矩阵，每行至少一个自动化测试；「明确不防」列了四条，不吹。
- verifier 第七项检查：预算↔配额一致性（配额存在但预算缺失 = manifest 被篡改）。