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

> M0 的实现以单个大提交 `3bccd59` 收口（教训：教程驱动应当「每章一 commit」，从 v0.1.1 起严格执行）。

## 后续章节规划（实现一章、写一章）

- 第 8 章：解析器错误恢复（多错误一次报告，补齐 08-diagnostics §8.6）
- 第 9 章：`having` 与受限 `cast`（v0.1.1）
- 第 10 章：M2 证据签名与独立 verifier
- 第 11 章：M3 Polars 第二后端与语义差集测试
- 第 12 章：M4 Cranelift 原生后端初探（表达式 JIT）

## 阅读环境

```powershell
git clone https://github.com/javenleeCH/pebble.git
cd pebble
cargo test          # 全绿即可开始
```

运行 DuckDB 端到端需要 `tools/duckdb.exe`（Windows）或 PATH 中的 `duckdb`；缺失时相关测试自动跳过，不影响跟做前 6 章。
