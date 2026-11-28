# pebble-mlir

职责：保留为后续原生/受控执行后端 crate，可研究 typed HIR → Pebble Physical Plan → 标准稳定 MLIR Dialect 的 lowering。优先复用稳定上游设施，不以自定义 Dialect 掩盖未定义语义。

路线：MLIR 是 M4 原生后端**主路线**（2026-11-28 修订，Polars 取消）。当前产品后端为 DuckDB SQL；MLIR 后端按真实负载与可信性需求推进产品化，届时引入 Pebble Physical Plan，工件哈希对 canonical 物理计划文本计算（非机器码），并接入同一 canonical HIR、manifest/receipt 证据链。

状态：crate 保留，暂不进入 MVP 实现与默认交付。研究阶段不提前建设完整 Physical Plan、Arrow 全栈或 storage。
