# pebble-cranelift

职责：保留为后续原生/受控执行后端 crate，可先行开展小范围研究原型。目标路径为 typed HIR → Pebble Physical Plan → CLIF/JIT/AOT artifact；Physical Plan 与原生执行语义必须可审计，不得绕过 canonical HIR 证据链。

路线：不是 M0/M1 产品首发后端；当前产品后端为 DuckDB SQL，Polars 后续接入。Cranelift 与 MLIR 同属 M4 原生路线，是否产品化由真实负载、可信性和受控执行需求决定。

状态：crate 保留，暂不进入 MVP 实现与默认交付。研究代码不得提前引入 Arrow 全栈、自建 storage 或第二套未审计语言语义。
