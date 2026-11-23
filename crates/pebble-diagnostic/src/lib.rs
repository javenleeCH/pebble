//! pebble-diagnostic：Span、诊断分类与错误码。
//!
//! 错误码与严重级别是稳定契约（docs/draft/spec/08-diagnostics.md）。
//! 渲染负责源码级标注（行/列 + 提示），Span 贯穿 parser → semantics → HIR。

use std::fmt;

/// 源码区间（字节偏移，UTF-8）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// 诊断严重级别。MVP 仅产生 Error；Warning/Note 为预留通道。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        })
    }
}

/// 编译期错误码（稳定契约，不得复用/改义）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    // 词法
    LexIllegalChar,
    LexUnclosedString,
    LexBadEscape,
    LexBadNumber,
    // 语法
    ParUnexpected,
    ParOpAfterTerminal,
    ParExtendNotRecord,
    ParIfMalformed,
    ParAggNotAggCall,
    ParSelectItemUnnamed,
    ParJoinOnMalformed,
    // 语义
    SemUnknownField,
    SemTypeMismatch,
    SemOpUnsupported,
    SemAliasDuplicate,
    SemOpPosition,
    SemJoinRightNotSource,
    SemIfBranchMismatch,
    SemNullMisuse,
    SemLambdaRemoved,
    SemPredicateNotBool,
    SemAmbiguousField,
    SemOutOfScope,
    SemSiblingBinding,
    SemDuplicateName,
    SemLiteralOutOfRange,
    SemConstDomain,
    SemAggMisuse,
    // Schema
    SchNoSchema,
    SchJoinKeyMismatch,
    SchDuplicateOutput,
    SchGroupKeyRename,
    SchLockMismatch,
    SchAmbiguousBoundary,
    // 执行（RUN；编译期不产生，供 driver/运行时使用）
    RunNonFiniteInput,
    RunSchemaValidation,
    RunIoFailure,
    RunResourceExhausted,
}

impl Code {
    pub fn as_str(&self) -> &'static str {
        match self {
            Code::LexIllegalChar => "LEX-001",
            Code::LexUnclosedString => "LEX-002",
            Code::LexBadEscape => "LEX-003",
            Code::LexBadNumber => "LEX-004",
            Code::ParUnexpected => "PAR-001",
            Code::ParOpAfterTerminal => "PAR-002",
            Code::ParExtendNotRecord => "PAR-003",
            Code::ParIfMalformed => "PAR-004",
            Code::ParAggNotAggCall => "PAR-005",
            Code::ParSelectItemUnnamed => "PAR-006",
            Code::ParJoinOnMalformed => "PAR-007",
            Code::SemUnknownField => "SEM-001",
            Code::SemTypeMismatch => "SEM-002",
            Code::SemOpUnsupported => "SEM-003",
            Code::SemAliasDuplicate => "SEM-004",
            Code::SemOpPosition => "SEM-005",
            Code::SemJoinRightNotSource => "SEM-006",
            Code::SemIfBranchMismatch => "SEM-007",
            Code::SemNullMisuse => "SEM-008",
            Code::SemLambdaRemoved => "SEM-009",
            Code::SemPredicateNotBool => "SEM-010",
            Code::SemAmbiguousField => "SEM-011",
            Code::SemOutOfScope => "SEM-012",
            Code::SemSiblingBinding => "SEM-013",
            Code::SemDuplicateName => "SEM-014",
            Code::SemLiteralOutOfRange => "SEM-015",
            Code::SemConstDomain => "SEM-016",
            Code::SemAggMisuse => "SEM-017",
            Code::SchNoSchema => "SCH-001",
            Code::SchJoinKeyMismatch => "SCH-002",
            Code::SchDuplicateOutput => "SCH-003",
            Code::SchGroupKeyRename => "SCH-004",
            Code::SchLockMismatch => "SCH-005",
            Code::SchAmbiguousBoundary => "SCH-006",
            Code::RunNonFiniteInput => "RUN-001",
            Code::RunSchemaValidation => "RUN-002",
            Code::RunIoFailure => "RUN-003",
            Code::RunResourceExhausted => "RUN-004",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条诊断：错误码 + 消息 + 源码位置 + 可选提示。
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: Code,
    pub message: String,
    pub span: Option<Span>,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn error(code: Code, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: Some(span),
            hint: None,
        }
    }

    pub fn run(code: Code, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: None,
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// 诊断收集器。任一 Error 存在时禁止 lowering / codegen。
#[derive(Default, Debug)]
pub struct DiagBag {
    pub diags: Vec<Diagnostic>,
}

impl DiagBag {
    pub fn new() -> Self {
        DiagBag { diags: Vec::new() }
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    pub fn error(&mut self, code: Code, message: impl Into<String>, span: Span) {
        self.push(Diagnostic::error(code, message, span));
    }

    pub fn error_hint(
        &mut self,
        code: Code,
        message: impl Into<String>,
        span: Span,
        hint: impl Into<String>,
    ) {
        self.push(Diagnostic::error(code, message, span).with_hint(hint));
    }

    pub fn has_error(&self) -> bool {
        self.diags.iter().any(|d| d.severity == Severity::Error)
    }
}

/// 源码级渲染：定位行/列并标注出错区间（纯文本，无 ANSI；行为确定性优先）。
pub fn render(source: &str, filename: &str, d: &Diagnostic) -> String {
    let mut out = String::new();
    let head = format!("{}[{}]: {}", d.severity, d.code, d.message);
    out.push_str(&head);
    out.push('\n');
    if let Some(span) = d.span {
        let (line_no, col_no, line_text, carets) = locate(source, span);
        out.push_str(&format!("  ┌─ {}:{}:{}\n", filename, line_no, col_no));
        out.push_str("  │\n");
        out.push_str(&format!("{} │ {}\n", line_no, line_text));
        out.push_str(&format!(
            "  │ {}{} {}\n",
            " ".repeat(col_no.saturating_sub(1)),
            carets,
            d.code.as_str()
        ));
        out.push_str("  │\n");
    } else {
        out.push_str("  ┌─ (无源码位置)\n");
    }
    if let Some(hint) = &d.hint {
        out.push_str(&format!("  提示: {}\n", hint));
    }
    out
}

/// 返回（1-based 行号、列号、该行内容、^ 标注串）。
fn locate(source: &str, span: Span) -> (usize, usize, String, String) {
    let start = span.start.min(source.len());
    let end = span.end.min(source.len()).max(start);
    let mut line_no = 1usize;
    let mut line_start = 0usize;
    for (i, ch) in source.char_indices() {
        if i >= start {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = i + 1;
        }
    }
    let rest = &source[line_start..];
    let line_text = rest.split('\n').next().unwrap_or("").trim_end_matches('\r');
    let col_no = source[line_start..start].chars().count() + 1;
    let width = source[start..end].chars().count().max(1);
    let carets = "^".repeat(width);
    (line_no, col_no, line_text.to_string(), carets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_locates_line_and_caret() {
        let src = "Table(\"a.csv\")\n  .filter(u.age >= 18)\n";
        // 第 2 行 "  .filter(u.age >= 18)"：u.age 从第 13 列开始，占 3 个字符
        let d = Diagnostic::error(
            Code::SemUnknownField,
            "字段 `agge` 不存在于当前解析域",
            Span::new(27, 30),
        )
        .with_hint("当前可用字段: age, name");
        let text = render(src, "demo.pebble", &d);
        assert!(text.contains("SEM-001"));
        assert!(text.contains("demo.pebble:2:13"));
        assert!(text.contains("当前可用字段"));
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(Code::SemAmbiguousField.as_str(), "SEM-011");
        assert_eq!(Code::SchAmbiguousBoundary.as_str(), "SCH-006");
        assert_eq!(Code::RunSchemaValidation.as_str(), "RUN-002");
    }
}
