# Pebble

[![CI](https://github.com/javenleeCH/pebble/actions/workflows/ci.yml/badge.svg)](https://github.com/javenleeCH/pebble/actions/workflows/ci.yml)

面向离线批结构化数据的**可信变换 DSL 与嵌入式编译器 SDK**：唯一静态语义、Schema 快照、规范化 HIR、DuckDB SQL 执行工件和运行回执组成可核对的证据链。

> **M0 最小可验证核已跑通**（`cargo test` 全绿，端到端在 DuckDB 1.4.x 验证）：build → run → verify 四幕证据链，
> 语义变更使旧审批失效、注释变更不影响哈希、数据篡改（TOCTOU）与非有限 f64 输入均被拒绝。
> 运行 `cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks` 开始。
>
> **第 8 章（解析器错误恢复）已完成**：一次编译报出全部词法/语法错误（按源码位置排序，末尾附汇总行），
> 并修复了中文注释导致的诊断定位漂移/崩溃。试试 [tests/fixtures/diagnostics/multi_error.pebble](tests/fixtures/diagnostics/multi_error.pebble)，教程见 [`docs/tutorial/ch08-解析器错误恢复.md`](docs/tutorial/ch08-解析器错误恢复.md)。
>
> **第 9 章（语义层错误恢复）已完成**：语义错误同样一次报完；定义失败的名字被毒化，它的后果不再被报成新错误。
> 试试 [tests/fixtures/diagnostics/multi_semantic_error.pebble](tests/fixtures/diagnostics/multi_semantic_error.pebble)，教程见 [`docs/tutorial/ch09-语义层错误恢复.md`](docs/tutorial/ch09-语义层错误恢复.md)。
>
> **第 10 章（codegen 融合）已完成**：DuckDB 后端不再给每个算子套一层子查询，连续的 `filter` / `extend` / `select` / `sort` 并进同一层 `SELECT`（工件版本 v2）。
> 两万个连续 `filter` 的流水线此前会让引擎崩溃，现在正常运行。教程见 [`docs/tutorial/ch10-codegen融合.md`](docs/tutorial/ch10-codegen融合.md)。
>
> **第 11 章（having 与受限 cast）已完成（v0.1.1）**：`.agg(…).having(…)` 把「对分组结果过滤」写进源码，并发射为 SQL 的 `HAVING`；
> `cast(expr as i64|f64|string)` 提供四种有明确取值规则的显式转换。
> 取值规则有参考实现，并在真实 DuckDB 上逐格对拍。教程见 [`docs/tutorial/ch11-having与cast.md`](docs/tutorial/ch11-having与cast.md)。

## 文档导航

| 产物 | 位置 |
|---|---|
| **从 0 到 1 系列教程（推荐入口，教程驱动开发）** | [`docs/tutorial/README.md`](docs/tutorial/README.md) |
| 定位与取舍 | [`rationale.md`](rationale.md) |
| **v3 设计定稿（目标/边界/功能/语法/语义/技术路线）** | [`docs/design-decisions.md`](docs/design-decisions.md) |
| 第一轮专家评审 | [`docs/review-2026-11-23.md`](docs/review-2026-11-23.md) |
| 第二轮专家复审 | [`docs/review-r2-2026-11-23.md`](docs/review-r2-2026-11-23.md) |
| M0 demo（证据链样例） | [`examples/00_m0_revenue.pebble`](examples/00_m0_revenue.pebble) |
| 威胁模型 | [`docs/threat-model.md`](docs/threat-model.md) |
| 样例与预期输出 | [`examples/`](examples/) |
| EBNF 文法草案（Draft v0.2） | [`grammar/pebble.ebnf`](grammar/pebble.ebnf) |
| 规范/内部设计（与定稿同步） | [`docs/draft/`](docs/draft/) |
| 测试与验收 | [`tests/README.md`](tests/README.md) |

## 快速上手

```text
# 0. 审批密钥（一次性；seed 即私钥，保密）
cargo run -p pebble-driver -- keygen --out approver.json

# 1. Schema lock（schema-as-code；样例的 lock 已在 tests/locks 备好）
cargo run -p pebble-driver -- lock table tests/data/orders.csv order_id:i64 status:string amount:i64? score:f64 customer_id:string --out tests/locks/orders_csv.lock.json

# 2. build：canonical HIR/hash + DuckDB SQL + manifest
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks

# 3. sign（M2）：审批签名——顺序固定 build → sign → run，回执绑定的才是已审版本
cargo run -p pebble-driver -- sign examples/target/pebble/00_m0_revenue/manifest.json --key approver.json

# 4. run：TOCTOU 数据哈希核对 + f64 有限值预检 + 提交已审工件 → receipt
cargo run -p pebble-driver -- run examples/target/pebble/00_m0_revenue

# 5. verify：APPROVED PLAN == EXECUTED PLAN（--pubkey 同时核验签名）
cargo run -p pebble-driver -- verify examples/target/pebble/00_m0_revenue/manifest.json examples/target/pebble/00_m0_revenue/receipt.json --pubkey approver.json

# 6. 独立核验（审计员视角：不加载编译器，只凭工件 + 外部公钥）
cargo run -p pebble-verifier -- examples/target/pebble/00_m0_revenue --pubkey approver.json

# 7.（可选）资源配额：最坏情况输出超过 N 行即拒绝构建（ch13）
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks --quota-rows 1000000

# 8.（可选）执行超时：到期杀引擎 → RUN-004（ch13）
cargo run -p pebble-driver -- run examples/target/pebble/00_m0_revenue --timeout-secs 30
```

## 五条红线

1. 总性：无用户无界控制流；终止不等于资源安全。
2. 一阶纯函数：无一等/高阶函数；MVP 无任意 UDF。
3. 线性数据流：单入口单出口；if 只能值选择。
4. 受控效应：输入输出经显式能力边界。
5. 语义封闭：禁止「由实现定义」。

## MVP 技术路线

Rust 2024 · 手写递归下降解析（原型实现选择；Chumsky 为路线草案）· 自定义源码级诊断渲染（Ariadne 为路线草案）· canonical typed HIR · Schema lock · **可插拔多后端 BackendArtifact** · Execution Manifest / Receipt / Verify · 最小 LaTeX 可读视图。

后端路线：**DuckDB SQL（M0 已实现）→ MLIR 原生执行后端（M4 主路线）**；Cranelift 保留为研究对照（非产品路线）。路线修订（2026-11-28）：Polars 后端取消——原生后端直上 MLIR。

M0 明确不做：另写解释器、自建 Physical Plan、Arrow 全栈、自研 storage；时间函数/coalesce/split、数值转字符串与通用 `cast` 仍后置（`having` 与受限 `cast` 已在 v0.1.1 完成）。

工件版本：DuckDB 后端的 SQL 形态为 v2（第 10 章）。v1 给每个算子套一层子查询，实测连续约 300 个算子以上 DuckDB 就无法执行；v2 把连续的行级算子并进同一层 `SELECT`，这个限制已消除。

已知语义差集（如实记录，见第 11 章）：`lower` / `upper` 在三处特殊码点上与规范的「Unicode 全映射」不一致——DuckDB 实现的是简单映射（`lower("İ")`、`upper("ß")`、词尾 `σ`）。规范与实现该改哪一边，尚待决定。