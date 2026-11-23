# pebble-duckdb

Pebble 的 **M0/M1 首发产品后端**：把 canonical typed Logical HIR 编译为 `BackendArtifact::DuckDbSql`，交由指定版本 DuckDB 执行，并生成 ExecutionReceipt。

职责边界：

- 显式实现 Pebble 的 checked-null、有限 f64 输入校验、零规范化、Kleene 谓词、nulls last、group/distinct 值同一性及 join null 键规则，不得直接继承 SQL 默认行为。
- 固定 canonical SQL、后端版本、engine config 和 terminal options，参与 ExecutionManifest。
- 记录实际提交 SQL hash、输入校验、执行状态和可选 EXPLAIN 观测附件。
- 不宣称外部 DuckDB 的物理计划逐节点等于已审 HIR；DuckDB 属于执行信任基。

后续多后端路线：Polars 在 M3 作为独立 BackendArtifact 接入；Cranelift/MLIR 在 M4 走 Pebble Physical Plan 与 native BackendArtifact，不经过本 crate。