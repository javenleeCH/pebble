# Pebble 从 0 到 1：亲手造一门语言（M0 系列教程）

> 本系列仿照 LLVM「Writing a Toy Compiler（Kaleidoscope）」的写法：**每个章节对应一个可运行、可测试的代码状态**；读者可以 checkout 到任意章节的 commit，跟着做、跟着跑、跟着改。
>
> 本项目的第一目标是「完整体验一门语言从立项、实现到落地」，语言本身的意义与价值是其次。因此——**实现改动必须与教程同步演进**。

## 开发纪律（对本仓库具有约束力）

1. **教程即代码的说明书**：任何实现改动，同一个 commit 内要么更新对应章节，要么在章节标注「TODO 待补」；教程滞后实现不得超过一章。
2. **一章 = 一个可运行状态**：每章结束时的代码必须 `cargo test` 全绿；读者 checkout 到该章的 commit 就能复现本章内容。
3. **commit 规范**：`<type>(ch<N>-<crate>): 摘要`，正文写「这一步教什么 / 为什么」。例：
   - `feat(ch07-driver): 证据链四幕——build/run/verify/lock`
   - `docs(ch04-semantics): 补写 NN 收缩的可靠性证明小节`
4. **每章必含五段**：本章目标 → 为什么这样设计（含被否决的备选）→ 代码走读 → 跑起来 → 练习（带提示，不给完整答案）。
5. **不写「正确废话」**：教程引用的代码必须与仓库当前实现一致（可加节选标记），过期即视为 bug。

## 章节地图

| 章 | 主题 | 对应 crate / 关键文件 | commit |
|---|---|---|---|
| [第 0 章](ch00-立项.md) | 立项：从「我想造语言」到 rationale 与五条红线 | `rationale.md`、两轮评审 | `3bccd59`（补写） |
| [第 1 章](ch01-词法.md) | 词法分析：token 模型与错误码 | `pebble-parser/src/lexer.rs` | `3bccd59` |
| [第 2 章](ch02-解析与AST.md) | 递归下降解析器与 AST | `pebble-parser/src/{ast,parser}.rs` | `3bccd59` |
| [第 3 章](ch03-诊断契约.md) | 诊断系统：错误码是稳定契约 | `pebble-diagnostic/src/lib.rs` | `3bccd59` |
| [第 4 章](ch04-类型与语义.md) | 类型系统与 P0 语义（null/Kleene/checked-null/流敏收缩） | `pebble-semantics/src/lib.rs` | `3bccd59` |
| [第 5 章](ch05-HIR与哈希.md) | HIR：字段身份、canonical 序列化与内容寻址 | `pebble-hir/src/{lib,canonical,lock}.rs` | `3bccd59` |
| [第 6 章](ch06-DuckDB语义守卫.md) | 后端 codegen：不继承 SQL 默认行为 | `pebble-duckdb/src/lib.rs` | `3bccd59` |
| [第 7 章](ch07-证据链.md) | 落地：build/run/verify 证据链闭环 | `pebble-driver/src/*` | `3bccd59` |
| [第 8 章](ch08-解析器错误恢复.md) | 解析器错误恢复：同步点、级联错误、阶段门控 | `pebble-parser/src/{lexer,parser}.rs`、`pebble-diagnostic/src/lib.rs` | 见 git log |
| [第 9 章](ch09-语义层错误恢复.md) | 语义层错误恢复：毒化，让坏消息静默传播 | `pebble-semantics/src/lib.rs` | 见 git log |
| [第 10 章](ch10-codegen融合.md) | codegen 融合：不是每个算子都要一层子查询（工件 v2） | `pebble-duckdb/src/lib.rs`、`pebble-hir/src/manifest.rs` | 见 git log |
| [第 11 章](ch11-having与cast.md) | having 与受限 cast：给语言加一个特性的完整路径（v0.1.1） | `pebble-{parser,hir,semantics,duckdb,latex}`、`tests/differential.rs` | 见 git log |
| [第 12 章](ch12-签名与verifier.md) | M2 证据签名与独立 verifier：堵住自洽伪造与既当运动员又当裁判 | `pebble-hir/src/{sign,canonical}.rs`、`crates/pebble-verifier/` | 见 git log |
| [附录 A](appendix-ci-发布门禁.md) | CI 与发布门禁：把纪律变成机器 | `.github/workflows/*`、`docs_gate.rs` | 见 git log |

> M0 的实现以单个大提交 `3bccd59` 收口（教训：教程驱动应当「每章一 commit」）。从第 8 章起严格执行：一章一个 commit，读者 checkout 到该章的提交就能复现本章内容。

## 后续章节规划（实现一章、写一章）

- 第 13 章：M3 宿主 SDK 与资源配额
- 第 14 章：M4 MLIR 原生后端初探（主路线；Cranelift 研究对照）

> 排序说明：codegen 融合（第 10 章）原本排在 `having/cast` 之后，提前是因为它修的是已发布后端的一个真实限制，
> 而且它会改变全部 canonical SQL——越早做，需要重新审批的东西越少。
>
> 待定的设计决定：`lower` / `upper` 的 Unicode 全映射差集（第 11 章 §11）——改规范还是改实现。

> 路线调整（2026-11-25）：原计划第 8 章「补齐 §8.6」。实现时发现语义阶段的多错误报告依赖一项独立的技术（毒化类型），一堂课讲一个核心想法对新手更好，故单列为第 9 章，其后章节顺延一位。

## 阅读环境

```powershell
git clone https://github.com/javenleeCH/pebble.git
cd pebble
cargo test          # 全绿即可开始
```

运行 DuckDB 端到端需要 `tools/duckdb.exe`（Windows）或 PATH 中的 `duckdb`；缺失时相关测试自动跳过，不影响跟做前 6 章。
