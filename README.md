# Pebble

[![CI](https://github.com/javenleeCH/pebble/actions/workflows/ci.yml/badge.svg)](https://github.com/javenleeCH/pebble/actions/workflows/ci.yml)

面向离线批结构化数据的**可信变换 DSL 与嵌入式编译器 SDK**：唯一静态语义、Schema 快照、规范化 HIR、DuckDB SQL 执行工件和运行回执组成可核对的证据链。

> **M0 最小可验证核已跑通**（`cargo test` 全绿，端到端在 DuckDB 1.4.x 验证）：build → run → verify 四幕证据链，
> 语义变更使旧审批失效、注释变更不影响哈希、数据篡改（TOCTOU）与非有限 f64 输入均被拒绝。
> 运行 `cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks` 开始。

## 文档导航

| 产物 | 位置 |
|---|---|
| **从 0 到 1 系列教程（推荐入口，教程驱动开发）** | [`docs/tutorial/README.md`](docs/tutorial/README.md) |
| 定位与取舍 | [`rationale.md`](rationale.md) |
| **v3 设计定稿（目标/边界/功能/语法/语义/技术路线）** | [`docs/design-decisions.md`](docs/design-decisions.md) |
| 第一轮专家评审 | [`docs/review-2026-11-23.md`](docs/review-2026-11-23.md) |
| 第二轮专家复审 | [`docs/review-r2-2026-11-23.md`](docs/review-r2-2026-11-23.md) |
| M0 demo（证据链样例） | [`examples/00_m0_revenue.pebble`](examples/00_m0_revenue.pebble) |
| 样例与预期输出 | [`examples/`](examples/) |
| EBNF 文法草案（Draft v0.2） | [`grammar/pebble.ebnf`](grammar/pebble.ebnf) |
| 规范/内部设计（与定稿同步） | [`docs/draft/`](docs/draft/) |
| 测试与验收 | [`tests/README.md`](tests/README.md) |

## 快速上手

```text
# 1. Schema lock（schema-as-code）
cargo run -p pebble-driver -- lock table tests/data/orders.csv order_id:i64 status:string amount:i64? score:f64 customer_id:string --out tests/locks/orders_csv.lock.json

# 2. build：canonical HIR/hash + DuckDB SQL + manifest
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks

# 3. run：TOCTOU 数据哈希核对 + f64 有限值预检 + 提交工件 → receipt
cargo run -p pebble-driver -- run examples/target/pebble/00_m0_revenue

# 4. verify：APPROVED PLAN == EXECUTED PLAN
cargo run -p pebble-driver -- verify examples/target/pebble/00_m0_revenue/manifest.json examples/target/pebble/00_m0_revenue/receipt.json
```

## 五条红线

1. 总性：无用户无界控制流；终止不等于资源安全。
2. 一阶纯函数：无一等/高阶函数；MVP 无任意 UDF。
3. 线性数据流：单入口单出口；if 只能值选择。
4. 受控效应：输入输出经显式能力边界。
5. 语义封闭：禁止「由实现定义」。

## MVP 技术路线

Rust 2024 · 手写递归下降解析（原型实现选择；Chumsky 为路线草案）· 自定义源码级诊断渲染（Ariadne 为路线草案）· canonical typed HIR · Schema lock · **可插拔多后端 BackendArtifact** · Execution Manifest / Receipt / Verify · 最小 LaTeX 可读视图。

后端路线：**DuckDB SQL（M0 已实现）→ Polars（M3）→ Cranelift / MLIR（M4 原生执行后端，保留并持续推进研究原型）**。

M0 明确不做：Polars/多后端、另写解释器、自建 Physical Plan、Arrow 全栈、自研 storage；having/cast/时间函数/coalesce/split 按计划后置。