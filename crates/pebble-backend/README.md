# pebble-backend

职责：后端中立编译接口，消费 typed canonical HIR/CompiledUnit，输出 `BackendArtifact`；禁止 AST 直连后端，也禁止后端回写 HIR。执行适配器提交同一工件并生成 `ExecutionReceipt`，编译与执行不得隐式合并。

路线：架构支持多后端，M0/M1 只实现 DuckDB SQL；Polars 后续以独立 artifact/codegen 接入并做能力与语义差集检查。外部引擎的优化计划不属于 Pebble Physical Plan。M4 原生受控执行路线可新增 `HIR → Pebble Physical Plan → native BackendArtifact`。

状态：目录骨架，暂不写实现。M0/M1 不自建 Physical Plan、Arrow 全栈或 storage。
