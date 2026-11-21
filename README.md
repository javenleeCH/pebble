# Pebble

面向离线批结构化数据的链式流式变换 DSL 及编译器：线性链式语法、天然数据流 DAG、静态 Schema 校验、非图灵完备。

> 当前处于 **阶段 1–3（定位 → 样例 → 文法草案）**，原型实现尚未开始。正式 Spec 待原型跑通后固化。

## 文档导航

| 阶段 | 产物 | 位置 |
|---|---|---|
| 1 | 定位与取舍一页纸（rationale） | [`rationale.md`](rationale.md) |
| 2 | 样例代码 + 预期输出 | [`examples/`](examples/) |
| 3 | EBNF 文法草案 | [`grammar/pebble.ebnf`](grammar/pebble.ebnf) |
| 4 | 编译器 crate 骨架（占位） | [`crates/`](crates/) |
| — | 草稿设计笔记（未定稿） | [`docs/draft/`](docs/draft/) |

## 设计红线

1. 不引入通用循环 / 递归；迭代只由内置集合算子隐式完成。
2. 不开放一等函数；Lambda 仅为算子表达式模板。
3. 不放开顶层控制流；`if` 只能是表达式。
4. 不引入可变全局状态；除终端算子外全部纯变换。
5. 不支持无限流；数据源必须是有限集合。

## 技术路线（详见计划书）

Rust 2024 Edition · Chumsky 解析 · Ariadne 诊断 · HIR 为后端唯一边界 · 后端：Cranelift（起步）→ MLIR（演进）· LaTeX 公式导出。
