//! pebble-diagnostic：Span、诊断分类与错误码。
//!
//! 错误码与严重级别是稳定契约（docs/draft/spec/08-diagnostics.md）。
//! 渲染负责源码级标注（行/列 + 提示），Span 贯穿 parser → semantics → HIR。

use std::fmt;

/// 源码区间：UTF-8 **字节偏移**的半开区间 `[start, end)`，两端都落在字符边界上。
///
/// 单位是契约的一部分：词法层按什么单位写、渲染层就必须按什么单位读（第 8 章）。
/// Span 只服务诊断与源码映射，不进入 canonical HIR hash。
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
    /// ch13：静态成本模型——最坏情况输出行数超过配额（M3 资源预算）。
    SemBudgetExceeded,
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
            Code::SemBudgetExceeded => "SEM-018",
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Default, Debug)]
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

    /// Error 数量按错误码前缀分类；只列出现过的类别，顺序固定为 LEX → PAR → SEM → SCH → RUN
    ///（即编译流水线的阶段顺序，与发现顺序无关）。
    pub fn count_by_category(&self) -> Vec<(&'static str, usize)> {
        CATEGORIES
            .iter()
            .filter_map(|category| {
                let n = self
                    .diags
                    .iter()
                    .filter(|d| d.severity == Severity::Error && category_of(&d.code) == *category)
                    .count();
                (n > 0).then_some((*category, n))
            })
            .collect()
    }

    /// 诊断的整理（第 8 章「最后一公里」）：
    /// 按源码位置排序（同一位置保持发现顺序；无源码位置的 RUN 诊断排在最后），
    /// 再去掉完全重复的条目。诊断是「发现顺序」的，人读的是「源码顺序」。
    pub fn normalize(&mut self) {
        self.diags.sort_by_key(|d| {
            d.span
                .map(|s| (s.start, s.end))
                .unwrap_or((usize::MAX, usize::MAX))
        });
        self.diags.dedup();
    }
}

const CATEGORIES: &[&str] = &["LEX", "PAR", "SEM", "SCH", "RUN"];

fn category_of(code: &Code) -> &'static str {
    let text = code.as_str();
    match text.split_once('-') {
        Some((prefix, _)) => prefix,
        None => text,
    }
}

/// 一次最多显示多少条诊断。没人读得完一万条；超出部分只在汇总行里计数。
pub const MAX_RENDERED: usize = 100;

/// 渲染全部诊断，并在末尾附一行汇总：`编译中止：共 N 个错误（LEX 1，PAR 2）`。
///
/// 诊断先按源码位置整理，最多显示 `MAX_RENDERED` 条（汇总行里的 N 始终是真实总数）。
/// 汇总行只统计 Error；一条 Error 都没有时不输出汇总（调用方也就不该中止编译）。
pub fn render_all(source: &str, filename: &str, bag: &DiagBag) -> String {
    let mut ordered = bag.clone();
    ordered.normalize();
    let index = LineIndex::new(source);
    let mut out = String::new();
    for d in ordered.diags.iter().take(MAX_RENDERED) {
        out.push_str(&render_with(&index, source, filename, d));
    }
    let hidden = ordered.diags.len().saturating_sub(MAX_RENDERED);
    if hidden > 0 {
        out.push_str(&format!("……另有 {hidden} 条未显示。\n"));
    }
    let categories = ordered.count_by_category();
    if !categories.is_empty() {
        let total: usize = categories.iter().map(|(_, n)| n).sum();
        let detail: Vec<String> = categories
            .iter()
            .map(|(c, n)| format!("{} {}", c, n))
            .collect();
        out.push_str(&format!(
            "编译中止：共 {} 个错误（{}）\n",
            total,
            detail.join("，")
        ));
    }
    out
}

/// 每一行起始位置（字节偏移）的有序表。
///
/// 渲染一条诊断要知道「偏移在第几行」。逐条从头扫描源码，代价是「诊断数 × 文件大小」：
/// 一行里一万个非法字符，就要扫上亿次。有了这张表，每次定位是一次二分查找。
#[derive(Debug)]
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { starts }
    }

    /// 偏移 `offset` 所在的行（0 开始）：最后一个起点不晚于它的行。
    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset) - 1
    }
}

/// 源码级渲染：定位行/列并标注出错区间（纯文本，无 ANSI；行为确定性优先）。
pub fn render(source: &str, filename: &str, d: &Diagnostic) -> String {
    render_with(&LineIndex::new(source), source, filename, d)
}

fn render_with(index: &LineIndex, source: &str, filename: &str, d: &Diagnostic) -> String {
    let mut out = String::new();
    let head = format!("{}[{}]: {}", d.severity, d.code, d.message);
    out.push_str(&head);
    out.push('\n');
    if let Some(span) = d.span {
        let (line_no, col_no, line_text, carets) = locate(index, source, span);
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
///
/// 渲染器是诊断的最后一道防线：无论传进来的 Span 多离谱（越界、落在多字节字符中间、
/// 跨行），这里都只做夹取，**绝不 panic**——报错的代码自己崩溃，用户就什么都看不到了。
fn locate(index: &LineIndex, source: &str, span: Span) -> (usize, usize, String, String) {
    let start = clamp_to_boundary(source, span.start);
    let line_no = index.line_of(start);
    let line_start = index.starts[line_no];
    let rest = &source[line_start..];
    let raw_line = rest.split('\n').next().unwrap_or("");
    let line_text = raw_line.trim_end_matches('\r');
    // 跨多行的 Span 只标注它在第一行上的那一段，否则 `^` 会冲出一整行
    let line_end = line_start + raw_line.len();
    let end = clamp_to_boundary(source, span.end).min(line_end).max(start);
    let col_no = source[line_start..start].chars().count() + 1;
    let width = source[start..end].chars().count().max(1);
    let carets = "^".repeat(width);
    (line_no + 1, col_no, line_text.to_string(), carets)
}

/// 把偏移夹取到 `[0, len]` 内最近的字符边界上（向左退）。
fn clamp_to_boundary(source: &str, mut offset: usize) -> usize {
    offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
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

    /// 行号表（`LineIndex`）要认得 CRLF，也认得多字节字符：行号按 `\n` 数，列号按字符数。
    #[test]
    fn line_numbers_survive_crlf_and_multibyte_text() {
        let src = "// 中文注释\r\nlet a = 1;\r\n\r\nlet b = \"汉字\";\r\n";
        let start = src.find("let b").expect("needle");
        let d = Diagnostic::error(Code::ParUnexpected, "x", Span::new(start, start + 5));
        let text = render(src, "crlf.pebble", &d);
        // 第 4 行（第 3 行是空行），第 1 列
        assert!(text.contains("crlf.pebble:4:1"), "{text}");
    }

    /// 诊断很多时：显示有条数上限，但汇总行里的数字是真的；
    /// 被显示出来的那些仍然按源码顺序、位置准确。
    #[test]
    fn many_diagnostics_are_capped_but_counted_exactly() {
        // 每行一个非法字符：2 000 行、2 000 条诊断
        let line = "  .filter(o.a ^ 1)\n";
        let hat = line.find('^').expect("needle");
        let src = line.repeat(2000);
        let mut bag = DiagBag::new();
        for k in 0..2000 {
            let at = k * line.len() + hat;
            bag.error(Code::LexIllegalChar, "非法字符", Span::new(at, at + 1));
        }
        let text = render_all(&src, "big.pebble", &bag);
        assert_eq!(text.matches("error[").count(), MAX_RENDERED, "{text}");
        assert!(text.contains("……另有 1900 条未显示。"), "{text}");
        assert!(
            text.ends_with("编译中止：共 2000 个错误（LEX 2000）\n"),
            "{text}"
        );
        // 显示的是前 100 条：第 100 条在第 100 行，位置准确
        assert!(
            text.contains(&format!("big.pebble:100:{}", hat + 1)),
            "{text}"
        );
        assert!(!text.contains("big.pebble:101:"), "{text}");
    }

    /// 源码里有中文时，按**字节偏移**取 Span 并渲染，行列必须落在原文位置上（第 8 章修的真实 bug）。
    #[test]
    fn render_locates_non_ascii_source_correctly() {
        let src = "// 中文注释：每个汉字 3 字节\nTable(\"a.csv\") as o\n  .filter(o.typo > 1)\n";
        let start = src.find("o.typo").expect("needle");
        let d = Diagnostic::error(
            Code::SemUnknownField,
            "字段不存在",
            Span::new(start, start + 6),
        );
        let text = render(src, "demo.pebble", &d);
        // 第 3 行；`  .filter(` 占 10 个字符，所以 `o.typo` 从第 11 列开始
        assert!(text.contains("demo.pebble:3:11"), "{text}");
        assert!(
            text.contains(&format!("  │ {}^^^^^^ SEM-001", " ".repeat(10))),
            "{text}"
        );
    }

    /// 渲染器是最后一道防线：再离谱的 Span 也只能夹取，不能 panic。
    #[test]
    fn render_never_panics_on_bad_spans() {
        let src = "//中文\n x\n";
        // 字节 3、6 落在多字节字符中间；越界、start > end、空串
        for span in [
            Span::new(3, 6),
            Span::new(6, 10),
            Span::new(9999, 10000),
            Span::new(8, 2),
            Span::new(0, 0),
        ] {
            let d = Diagnostic::error(Code::ParUnexpected, "x", span);
            let text = render(src, "demo.pebble", &d);
            assert!(text.contains("PAR-001"), "{text}");
        }
        let empty = render(
            "",
            "empty.pebble",
            &Diagnostic::error(Code::ParUnexpected, "x", Span::new(0, 0)),
        );
        assert!(empty.contains("PAR-001"));
    }

    /// 跨行的 Span 只在第一行打 `^`，不能冲出一整行。
    #[test]
    fn multi_line_span_marks_only_its_first_line() {
        let src = ".filter(o.a > 1\n  .collect()\n";
        let d = Diagnostic::error(Code::ParUnexpected, "x", Span::new(0, src.len()));
        let text = render(src, "demo.pebble", &d);
        let caret_line = text.lines().find(|l| l.contains('^')).expect("caret line");
        // 只有第一行的 15 个字符被标注；第二行的 `.collect()` 不应有 `^`
        assert_eq!(
            caret_line,
            format!("  │ {} PAR-001", "^".repeat(15)),
            "{text}"
        );
        assert_eq!(text.matches('^').count(), 15, "{text}");
    }

    #[test]
    fn normalize_sorts_by_position_and_drops_duplicates() {
        let mut bag = DiagBag::new();
        bag.error(Code::ParUnexpected, "later", Span::new(20, 21));
        bag.error(Code::LexIllegalChar, "earlier", Span::new(5, 6));
        bag.error(Code::LexIllegalChar, "earlier", Span::new(5, 6)); // 完全重复
        bag.push(Diagnostic::run(Code::RunIoFailure, "no span"));
        bag.normalize();
        let codes: Vec<&str> = bag.diags.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, ["LEX-001", "PAR-001", "RUN-003"]);
    }

    #[test]
    fn render_all_appends_summary_in_stage_order() {
        let src = "a b c d e\n";
        let mut bag = DiagBag::new();
        // 故意乱序塞入：PAR 在前、LEX 在后
        bag.error(Code::ParUnexpected, "p1", Span::new(2, 3));
        bag.error(Code::ParUnexpected, "p2", Span::new(4, 5));
        bag.error(Code::LexIllegalChar, "l1", Span::new(0, 1));
        let text = render_all(src, "demo.pebble", &bag);
        assert_eq!(text.matches("error[").count(), 3);
        let first = text.find("LEX-001").unwrap() < text.find("PAR-001").unwrap();
        assert!(first, "渲染应按源码顺序：\n{text}");
        assert!(
            text.ends_with("编译中止：共 3 个错误（LEX 1，PAR 2）\n"),
            "{text}"
        );

        // 没有 Error 就没有汇总行
        let quiet = render_all(src, "demo.pebble", &DiagBag::new());
        assert!(quiet.is_empty());
    }
}
