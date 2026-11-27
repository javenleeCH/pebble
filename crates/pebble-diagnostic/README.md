# pebble-diagnostic

职责：统一错误诊断——错误码、严重级别、Span 与源码级渲染。实现为自研的轻量渲染器（Ariadne 属路线草案）。解析恢复产生的含错误程序不得继续 lowering 或 codegen；Schema lock、后端能力不匹配和证据链校验失败也使用明确诊断。

Span 记录 UTF-8 字节偏移。多条诊断经 `normalize` 按源码位置排序、去重，由 `render_all` 一次性输出并附汇总行（第 8 章）；行号靠 `LineIndex` 二分查找，一次最多显示 100 条，汇总行给出真实总数。渲染器对任意 Span 都只做夹取、绝不 panic。

错误码表见 `docs/draft/spec/08-diagnostics.md`；教程见 `docs/tutorial/ch03-诊断契约.md`、`docs/tutorial/ch08-解析器错误恢复.md`。
