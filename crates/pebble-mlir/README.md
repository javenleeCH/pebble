# pebble-mlir

职责：保留为后续原生/受控执行后端 crate，可研究 typed HIR → Pebble Physical Plan → 标准稳定 MLIR Dialect 的 lowering。优先复用稳定上游设施，不以自定义 Dialect 掩盖未定义语义。

路线：不是 M0/M1 产品首发后端；当前产品后端为 DuckDB SQL，Polars 后续接入。MLIR 与 Cranelift 同属 M4 原生路线，只有在真实负载或可信性需求证明必要时才产品化，并须接入同一 canonical HIR、manifest/receipt 证据链。

状态：crate 保留，暂不进入 MVP 实现与默认交付。研究阶段不提前建设完整 Physical Plan、Arrow 全栈或 storage。
