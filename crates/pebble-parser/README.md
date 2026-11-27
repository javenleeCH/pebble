# pebble-parser

职责：词法 + 语法解析，输出只含语法结构与 Span 的 AST。实现为手写递归下降（Chumsky 属路线草案；诊断契约不因实现方式而改变）。当前语法统一为无 Lambda 的 `filter(expr)`、`extend(record)`、`select(expr as name)`、`join(..., on = expr)`、分组/全局 `agg` 和 `if condition { then } else { else }`；不再生成 `map` 或 Lambda 节点。

错误恢复（第 8 章）：出错后同步到最近的同步点（列表内的 `,`、当前层的右括号、下一个算子的起点），一次解析报告全部词法/语法错误，一个根因只报一次；恢复本身不制造诊断（残骸模式下试探解析）；词法层以 `Tok::Error` 为坏 token 占位。`parse()` 返回 `Result<Program, PartialAst>`——带伤的部分 AST 在类型上进不了语义层。提示只说与所遇 token 相容的话。

实现限制（不是语言规则，见 `docs/draft/spec/08-diagnostics.md` §8.6）：单个表达式嵌套不超过 64 层、原子不超过 256 个，超出以 PAR-001 报告而不是栈溢出。

文法与 AST 见 `grammar/pebble.ebnf`、`docs/draft/design/ast.md`；教程见 `docs/tutorial/ch02-解析与AST.md`、`docs/tutorial/ch08-解析器错误恢复.md`；恢复行为契约见 `tests/recovery.rs`。
