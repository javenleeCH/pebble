# pebble-duckdb

Pebble 的 **M0/M1 首发产品后端**：把 canonical typed Logical HIR 编译为 `BackendArtifact::DuckDbSql`，交由指定版本 DuckDB 执行，并生成 ExecutionReceipt。

职责边界：

- 显式实现 Pebble 的 checked-null、有限 f64 输入校验、零规范化、Kleene 谓词、nulls last、group/distinct 值同一性及 join null 键规则，不得直接继承 SQL 默认行为。
- 固定 canonical SQL、后端版本、engine config 和 terminal options，参与 ExecutionManifest。
- 记录实际提交 SQL hash、输入校验、执行状态和可选 EXPLAIN 观测附件。
- 不宣称外部 DuckDB 的物理计划逐节点等于已审 HIR；DuckDB 属于执行信任基。

SQL 形态（artifact v2，第 10 章）：连续的行级算子并进同一层 SELECT——`filter` → `WHERE`（多个谓词合成平衡的 AND 树）、`extend` / `select` → 选择列表、`sort` → `ORDER BY`；只在 `distinct` / `take` / 聚合（屏障）与 `join` 处开新的一层，`join` 层吸收其后的行级算子。嵌套深度取决于必须分层的个数，而不是算子的个数。v1（每个算子一层子查询）在 DuckDB 上连续约 300 个算子即无法执行。

版本标识写在 SQL 文件头（`ARTIFACT_HEADER`）与工件哈希的域分隔前缀上；v1 与 v2 的工件哈希不可能相同。

验证手段：单个算子的流水线 SQL 正文与 v1 逐字相同（不变的部分被钉成测试）；新旧实现在同一 DuckDB 上的差分测试；端到端与独立计算的对照；变异验证。教程见 `docs/tutorial/ch10-codegen融合.md`。

第 11 章：紧跟聚合的 `filter`（即 `having`）并进聚合层的 `HAVING`——聚合屏障唯一的例外；HAVING 里的聚合输出引用替换成表达式，不依赖方言的别名引用。`cast` 的四种转换各带守卫，先把引擎的行为（更宽的字符串写法、进位取整、越界报错）收敛到规范再取值；`trim` 显式给出 Unicode White_Space 的全部 25 个码点（DuckDB 默认的 `trim` 不去制表符）。

已知语义差集（未修）：`lower` / `upper` 在三处特殊码点上是 Unicode 简单映射，而规范要求全映射。

后续多后端路线：Polars 在 M3 作为独立 BackendArtifact 接入；Cranelift/MLIR 在 M4 走 Pebble Physical Plan 与 native BackendArtifact，不经过本 crate。