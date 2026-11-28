# pebble-driver

职责：编排 `parse → semantics → typed HIR → canonical HIR/hash → BackendArtifact`，并负责 `build/run/verify` 的 Schema lock、Execution Manifest、ExecutionReceipt 和输入校验闭环。审计视图与执行工件必须消费同一 canonical HIR。

路线：M0/M1 只启用 DuckDB SQL 执行适配器；架构保留 M4 MLIR 原生后端扩展点（Cranelift 为研究对照），不在 MVP 同时实现或宣称跨后端等价。

状态：已实现（M0：`build` / `run` / `verify` / `lock` 与证据链）。M0/M1 不自建 Physical Plan、Arrow 全栈或 storage；教程见 `docs/tutorial/ch07-证据链.md`。
