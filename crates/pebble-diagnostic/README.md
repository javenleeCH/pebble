# pebble-diagnostic

职责：统一错误诊断（Ariadne），维护错误码、源码片段、Span 与字段/来源身份的可读映射。解析恢复产生的含错误程序不得继续 codegen；Schema lock、后端能力不匹配和证据链校验失败也使用明确诊断。

状态：目录骨架，暂不写实现。错误码表草稿见 `docs/draft/spec/08-diagnostics.md`。
