# pebble-cranelift

职责：保留为后续原生/受控执行后端 crate，可先行开展小范围研究原型。目标路径为 typed HIR → Pebble Physical Plan → CLIF/JIT/AOT artifact；Physical Plan 与原生执行语义必须可审计，不得绕过 canonical HIR 证据链。

路线：Cranelift 为**研究对照**（2026-11-28 修订），非产品路线；当前产品后端为 DuckDB SQL，M4 原生主路线为 MLIR。本 crate 保留用于 JIT 基准对比与实验。

状态：crate 保留，暂不进入 MVP 实现与默认交付。研究代码不得提前引入 Arrow 全栈、自建 storage 或第二套未审计语言语义。
