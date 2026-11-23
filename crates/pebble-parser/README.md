# pebble-parser

职责：词法/语法解析（Chumsky），输出仅含语法结构与 Span 的 AST。当前语法统一为无 Lambda 的 `filter(expr)`、`extend(record)`、`select(expr as name)`、`join(..., on = expr)`、分组/全局 `agg` 和 `if condition { then } else { else }`；不再生成 `map` 或 Lambda 节点。

状态：目录骨架，暂不写实现。文法与 AST 分别见 `grammar/pebble.ebnf`、`docs/draft/design/ast.md`；AST 必须经 semantics/HIR 后才能进入任何后端。
