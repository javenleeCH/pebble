//! 递归下降解析器：按 grammar/pebble.ebnf Draft v0.2。
//! 关键文法点：
//! - `extend(...)` 参数必须是记录字面量（PAR-003）。
//! - `join(..., on = expr,)`：`on` 是固定绑定名，缺 `=` 报 PAR-007。
//! - `selectItem = expr [as ident]`；无 `as` 的复合表达式报 PAR-006。
//! - `ifExpr = if expr { expr } [else if expr { expr }...] else { expr }`，脱糖为单条件嵌套 If。
//! - select/sort/group_by/agg/join/记录字面量统一允许尾逗号。
//!
//! 错误恢复（第 8 章；08-diagnostics §8.6）：
//! - 解析函数仍返回 `Option<T>`，但 `None` 的含义是「**这一段已经坏了**」：
//!   错误已经报告（或由词法层报告过），且解析位置已经同步到最近的同步点。
//! - `None` 一路向上传播，只有两处会接住它：流水线循环（丢弃整个算子，继续下一个）
//!   和列表循环 `parse_list`（丢弃当前元素，继续下一个）。
//! - 词法层留下的洞是 `Tok::Error`：解析器遇到它绝不重复报错。
//! - 带伤的树以 `PartialAst` 交还，而不是 `Program`——类型上就进不了语义层。

use crate::ast::*;
use crate::lexer::{Tok, Token};
use pebble_diagnostic::{Code, DiagBag, Span};

const OPERATORS: &[&str] = &[
    "filter", "extend", "select", "distinct", "sort", "take", "join", "group_by", "agg",
];
const TERMINALS: &[&str] = &["write", "collect", "print"];
const AGG_FNS: &[&str] = &["count", "sum", "min", "max", "avg"];

/// 表达式允许的最大嵌套层数。递归下降解析器靠调用栈记状态：输入嵌套多深，栈就有多深。
/// 不设上限的话，一段深嵌套的输入就能把栈撑爆——进程以栈溢出崩溃，而不是给出诊断。
const MAX_NESTING: usize = 64;
/// 单个表达式允许的原子（字面量、字段、调用、if……）个数上限。
/// 一条很长的 `a + b + c + …` 不嵌套却会造出一棵很深的树，释放在语义层遍历这棵树时同样递归。
const MAX_ATOMS: usize = 256;

/// 解析失败时交还的部分 AST：只含**完整解析出来**的算子，可能缺源、缺算子、缺终端。
///
/// 它刻意不是 `Program`：语义层的入口 `compile()` 只接受 `&Program`，
/// 所以带伤的树在类型上就进不了语义层（08-diagnostics §8.6 第 4 条）。
///
/// 里面的 `Program` 装在 `Box` 里：`Result` 的大小取决于较大的那个变体，
/// 一整棵树按值塞进 `Err` 会让每次 `parse` 调用都背着几百个字节。
#[derive(Clone, Debug)]
pub struct PartialAst {
    program: Box<Program>,
}

impl PartialAst {
    /// 完整解析出来的变换算子（坏的算子已被丢弃）。
    pub fn ops(&self) -> &[Op] {
        &self.program.pipeline.ops
    }

    /// 完整解析出来的终端（若有）。
    pub fn terminal(&self) -> Option<&TerminalOp> {
        self.program.pipeline.terminal.as_ref()
    }

    /// 数据源。源本身解析失败时，这里是空路径的占位 `Table`。
    pub fn source(&self) -> &SourceOp {
        &self.program.pipeline.source
    }
}

/// 解析入口：词法 + 语法。
///
/// - `Ok(program)`：没有任何错误，AST 完整，可以进入语义层；
/// - `Err(partial)`：存在 LEX/PAR 错误（或字面量越界的 SEM-015/016），
///   诊断已全部收集在 `diags` 里；`partial` 是恢复出来的部分 AST，只供诊断与工具使用。
pub fn parse(source: &str, diags: &mut DiagBag) -> Result<Program, PartialAst> {
    let toks = crate::lexer::lex(source, diags);
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        brackets: Vec::new(),
        debris: false,
        nesting: 0,
        atoms: 0,
        par002_reported: false,
        diags,
    };
    let program = p.parse_program();
    if p.diags.has_error() {
        Err(PartialAst {
            program: Box::new(program),
        })
    } else {
        Ok(program)
    }
}

/// 流水线循环里「一个以 `.` 开头的元素」的解析结果。
enum Element {
    /// 一个完整的变换算子。
    Op(Op),
    /// 终端算子。
    Terminal(TerminalOp),
    /// 这一段已坏：错误已报告。调用方负责跳到下一个算子。
    Broken,
}

/// 一个 `…(` 结构接受什么参数。决定「右括号位置上遇到了别的东西」时，哪句提示不说谎。
#[derive(Clone, Copy)]
enum Arity {
    /// 写成 `()`，括号里什么都不接受（`collect`、`distinct`……）。
    Empty,
    /// 恰好一个参数（`filter`、`extend`、`take`、`write`、聚合调用）。
    One,
    /// 右表与 `on = 条件`（`join`）。
    Join,
}

pub struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    /// 未闭合的开括号栈（`(` / `{`）。栈高 = 当前 token 的括号深度；流水线层深度为 0。
    brackets: Vec<Tok>,
    /// **残骸模式**：此前的恢复越过了未闭合的括号（见 `sync_pipeline`）。
    /// 从那以后，看起来像算子的东西可能是表达式残骸（例如方法调用 `o.x.abs(`）：
    /// 只相信已知的算子名，并且每个元素都先试探着解析，站得住脚才算数。
    debris: bool,
    /// 当前表达式递归下降的深度与已读入的原子个数（见 `MAX_NESTING` / `MAX_ATOMS`）。
    nesting: usize,
    atoms: usize,
    /// PAR-002 是否已经报过（终端之后接的算子可能不止一个，但这条只报一次）。
    par002_reported: bool,
    diags: &'a mut DiagBag,
}

impl Parser<'_> {
    // ---------- 读 token ----------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }

    /// 刚刚消费掉的 token 的 Span（算子名、右括号等「上一个 token」）。
    fn last_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }

    /// 当前 token 的括号深度：0 = 流水线层。
    fn depth(&self) -> usize {
        self.brackets.len()
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            match t.tok {
                Tok::LParen | Tok::LBrace => self.brackets.push(t.tok.clone()),
                Tok::RParen | Tok::RBrace => {
                    // 右括号关掉与它配对的最内层开括号；中间没闭合的层一并关掉
                    let opener = if t.tok == Tok::RParen {
                        Tok::LParen
                    } else {
                        Tok::LBrace
                    };
                    if let Some(k) = self.brackets.iter().rposition(|b| *b == opener) {
                        self.brackets.truncate(k);
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        t
    }

    // ---------- 同步点：文法的围栏 ----------
    //
    // 恢复的全部依据只有一句话：**出错后不再试图理解残骸，直接跳到下一道围栏**。
    // Pebble 的围栏有三道，由细到粗：
    //   1. 列表里的 `,`          —— 丢掉一个元素，保住同一个算子里的其他元素
    //   2. 当前层的右括号        —— 丢掉一个算子 / 一次调用 / 一个括号表达式
    //   3. 下一个算子的起点 `.`  —— 丢掉整段残骸，回到流水线层

    /// 当前 token 是否是「可信的下一个算子起点」：后接 `标识符 (` 的 `.`，任何深度上都算。
    ///
    /// 合法的表达式里不存在 `.名字(` 这种写法（`.` 只用于 `别名.字段`），
    /// 所以在括号里看到它，说明前面的括号没闭合就开了下一个算子。
    /// 同步时**只**相信这种形状：`o.a` 里的 `.` 只是残骸的一部分。
    fn at_operator_start(&self) -> bool {
        *self.peek() == Tok::Dot && self.is_op_call_ahead()
    }

    /// 紧跟在当前 token 之后的算子名（若有）。
    fn op_name_ahead(&self) -> Option<&str> {
        match self.toks.get(self.pos + 1) {
            Some(Token {
                tok: Tok::Ident(name),
                ..
            }) => Some(name),
            _ => None,
        }
    }

    /// `peek()` 之后是否紧跟 `标识符 (`。
    fn is_op_call_ahead(&self) -> bool {
        self.op_name_ahead().is_some()
            && matches!(
                self.toks.get(self.pos + 2),
                Some(Token {
                    tok: Tok::LParen,
                    ..
                })
            )
    }

    /// `peek()` 之后是否紧跟「已知算子名 + `(`」。
    fn is_known_op_ahead(&self) -> bool {
        self.op_name_ahead()
            .is_some_and(|name| OPERATORS.contains(&name) || TERMINALS.contains(&name))
            && matches!(
                self.toks.get(self.pos + 2),
                Some(Token {
                    tok: Tok::LParen,
                    ..
                })
            )
    }

    /// 同步到深度不超过 `base` 的分隔点：`,`、当前层（或更外层）的右括号、
    /// 下一个算子的起点、文件结尾。之间的一切**静默丢弃**——
    /// 它们是错误的残骸，不是新的错误。
    ///
    /// 比 `base` 更深的括号整组吞掉；`base` 层的右括号不消费，留给它的拥有者收尾。
    fn sync_to(&mut self, base: usize) {
        loop {
            if *self.peek() == Tok::Eof {
                self.brackets.clear();
                break;
            }
            if self.at_operator_start() {
                break;
            }
            let is_separator =
                *self.peek() == Tok::Comma || matches!(self.peek(), Tok::RParen | Tok::RBrace);
            if is_separator && self.depth() <= base {
                break;
            }
            self.advance();
        }
    }

    /// 流水线层的恢复：丢弃直到下一个算子的可信起点（或文件结尾）。
    ///
    /// 停下时清空括号栈——被越过的括号就地视作闭合，我们回到了流水线层。
    /// 如果停下时括号栈非空，说明是在未闭合的括号里遇到了下一个算子：
    /// 从此进入**残骸模式**（`debris`），因为那段没闭合的表达式后面还可能有别的东西
    /// 看起来像算子——比如第二个方法调用 `o.y.abs(`。残骸模式一旦进入就保持到文件结尾。
    fn sync_pipeline(&mut self) {
        loop {
            if *self.peek() == Tok::Eof {
                break;
            }
            let trusted = self.at_operator_start() && (!self.debris || self.is_known_op_ahead());
            if trusted {
                break;
            }
            self.advance();
        }
        if !self.brackets.is_empty() {
            self.debris = true;
        }
        self.brackets.clear();
    }

    /// 一个元素解析完之后，接下来必须像「流水线里的下一个东西」：
    /// 文件结尾、下一个算子的起点，或者若干个迟到的右括号之后的这两者。
    /// 否则这个「元素」其实是表达式残骸（例如 `.rows.select() > 1` 里的 `.select(`）。
    fn ends_like_pipeline_element(&self) -> bool {
        let token_at = |k: usize| self.toks.get(self.pos + k).map(|t| &t.tok);
        let mut k = 0usize;
        while matches!(token_at(k), Some(Tok::RParen | Tok::RBrace | Tok::Error)) {
            k += 1;
        }
        match token_at(k) {
            None | Some(Tok::Eof) => true,
            Some(Tok::Dot) => {
                matches!(token_at(k + 1), Some(Tok::Ident(_)))
                    && matches!(token_at(k + 2), Some(Tok::LParen))
            }
            _ => false,
        }
    }

    // ---------- 报告错误：报告 + 同步，然后返回 None ----------
    //
    // 三条规则：
    //   1. 错误只在这里报告（加上几处 `diags.error`），别处一律 `return None`；
    //   2. 报告完立刻同步——之后 `None` 一路上抛时，位置已经落在同步点上；
    //   3. 词法层的洞（`Tok::Error`）不走这里：它已经被报过了。

    /// 在给定 Span 上报告错误，并从**当前位置**同步到当前层的分隔点。
    fn fail_spanned(
        &mut self,
        code: Code,
        message: impl Into<String>,
        span: Span,
        hint: Option<&str>,
    ) {
        let mut d = pebble_diagnostic::Diagnostic::error(code, message, span);
        if let Some(h) = hint {
            d = d.with_hint(h);
        }
        self.diags.push(d);
        let base = self.depth();
        self.sync_to(base);
    }

    /// 在当前 token 上报告错误并同步。调用方紧接着返回 `None`。
    fn fail(&mut self, code: Code, message: impl Into<String>) {
        let span = self.peek_span();
        self.fail_spanned(code, message, span, None);
    }

    /// 同 `fail`，但附带修复提示。
    fn fail_hint(&mut self, code: Code, message: impl Into<String>, hint: &str) {
        let span = self.peek_span();
        self.fail_spanned(code, message, span, Some(hint));
    }

    /// 在给定 Span 上报告错误——错误位置在已经读过的 token 上时用。
    fn fail_at(&mut self, code: Code, message: impl Into<String>, span: Span) {
        self.fail_spanned(code, message, span, None);
    }

    /// 当前位置若是词法层留下的洞（`Tok::Error`）：越过它并同步，返回 true。
    /// 词法层已经报过错，这里绝不重复报。
    fn eat_hole(&mut self) -> bool {
        if *self.peek() != Tok::Error {
            return false;
        }
        self.advance();
        let base = self.depth();
        self.sync_to(base);
        true
    }

    fn expect_punct(&mut self, t: &Tok, what: &str) -> bool {
        if self.peek() == t {
            self.advance();
            return true;
        }
        if self.eat_hole() {
            return false;
        }
        let got = describe(self.peek());
        self.fail(Code::ParUnexpected, format!("期望 {what}，遇到 {got}"));
        false
    }

    /// 期望某个 `(` 的右括号。`opened` 是左括号所在结构的原文（如 `.filter(`），
    /// 写进消息里，读者一眼就知道是哪个 `(` 没闭合。
    ///
    /// 提示是在替用户做判断，所以只说站得住的话：只有遇到文件结尾、下一个算子、
    /// 或者一个不配对的右括号时，「缺少右括号」才是可信的推断。
    /// 遇到标识符、字面量、`=` 这类 token，说明参数在它之前就已经完整了——
    /// 这时提「缺少 `)`」就是实话的反面。
    fn expect_close(&mut self, opened: &str, arity: Arity) -> bool {
        use Arity::*;
        if *self.peek() == Tok::RParen {
            self.advance();
            return true;
        }
        if self.eat_hole() {
            return false;
        }
        let got = describe(self.peek());
        let (message, hint) = match (arity, self.peek()) {
            (_, Tok::Comma) => (format!("`{opened}` 的参数到此已完整，不需要 {got}"), None),
            (_, Tok::Eof | Tok::Dot | Tok::RParen | Tok::RBrace) => (
                format!("期望 `)` 结束 `{opened}`，遇到 {got}"),
                Some("检查这个括号里的内容是否缺少右括号 `)`"),
            ),
            (Empty, _) => (
                format!("`{opened}` 不接受参数，却遇到了 {got}"),
                Some("这个算子写成 `()` 即可：删掉括号里的内容"),
            ),
            (Join, _) => (
                format!("期望 `)` 结束 `{opened}`，却先遇到了 {got}"),
                Some("join 只接受右表与 `on = 条件` 两个参数"),
            ),
            (One, _) => (
                format!("期望 `)` 结束 `{opened}`，却先遇到了 {got}"),
                Some("这个算子只接受一个参数：是多写了一个，还是把 `==` 写成了 `=`？"),
            ),
        };
        match hint {
            Some(h) => self.fail_hint(Code::ParUnexpected, message, h),
            None => self.fail(Code::ParUnexpected, message),
        }
        false
    }

    fn expect_ident(&mut self, what: &str) -> Option<String> {
        if let Tok::Ident(name) = self.peek().clone() {
            self.advance();
            return Some(name);
        }
        if self.eat_hole() {
            return None;
        }
        let got = describe(self.peek());
        // `what` 以反引号或 ASCII 字母开头时补一个空格：「期望 `on`」；中文则直接相连：「期望算子名」
        let separator =
            if what.starts_with(|c: char| c == '`' || c.is_ascii_alphabetic() || c == '_') {
                " "
            } else {
                ""
            };
        self.fail(
            Code::ParUnexpected,
            format!("期望{separator}{what}，遇到 {got}"),
        );
        None
    }

    fn accept_punct(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    fn accept_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Tok::Ident(n) if n == kw) {
            self.advance();
            return true;
        }
        false
    }

    // ---------- 列表循环：列表层的恢复点 ----------

    /// 列表的通用骨架（select 项、sort 键、group_by 键、agg 绑定、记录绑定、调用实参）：
    /// `elem { , elem } [,]`，允许尾逗号，允许空列表（是否允许由调用方决定）。
    ///
    /// 某个元素坏了（`elem` 返回 `None`）时：
    /// - 解析位置仍在列表自己的括号内 → 丢掉这个元素，从下一个 `,` 继续；
    /// - 列表的括号已经被更外层的恢复越过 → 整个列表算坏，返回 `None` 继续上抛。
    ///
    /// 返回 `(元素, 是否有过坏元素)`。坏列表收尾时不再报「期望右括号」——
    /// 那只是第一个错误的后果。
    fn parse_list<T>(
        &mut self,
        closing: &Tok,
        owner: &str,
        mut elem: impl FnMut(&mut Self) -> Option<T>,
    ) -> Option<(Vec<T>, bool)> {
        let my_depth = self.depth();
        let mut out = Vec::new();
        let mut damaged = false;
        if self.peek() == closing {
            self.advance();
            return Some((out, false));
        }
        loop {
            let item_start_span = self.peek_span();
            let item_diags = self.diags.diags.len();
            match elem(self) {
                Some(e) => out.push(e),
                None => {
                    // 这条诊断报在元素的第一个 token 上 = 元素还没读进任何东西就失败了
                    let failed_on_first_token = self
                        .diags
                        .diags
                        .get(item_diags)
                        .is_some_and(|d| d.span == Some(item_start_span));
                    if damaged && failed_on_first_token {
                        // 紧跟在坏元素后面、一个 token 都没读进去就失败的元素，多半是前者被逗号截断的残骸
                        //（`x = if a >, 0 { 1 }`：`0 { 1 }` 其实还是那个 if 的一部分）。连它的诊断一起丢弃
                        self.diags.diags.truncate(item_diags);
                    }
                    // 先吞掉内层没闭合的右括号，看能否回到本列表所在的层
                    while self.depth() > my_depth
                        && matches!(self.peek(), Tok::RParen | Tok::RBrace)
                    {
                        self.advance();
                    }
                    // 回不来：本列表的括号已被更外层的恢复越过（或已到文件结尾）
                    if self.depth() != my_depth {
                        return None;
                    }
                    // 回来了：这一项连同它后面残留的 `as x` 之类一起丢掉，停到 `,` 或右括号
                    damaged = true;
                    let base = self.depth();
                    self.sync_to(base);
                }
            }
            if self.accept_punct(&Tok::Comma) {
                if self.peek() == closing {
                    break; // 尾逗号
                }
                continue;
            }
            break;
        }
        if damaged {
            // 已经坏过的列表：右括号在就吃掉，不在也不再多报一次错
            if self.peek() == closing {
                self.advance();
            } else {
                return None;
            }
        } else {
            let what = format!("`,` 或 {}（结束 `{owner}` 的参数列表）", describe(closing));
            if !self.expect_punct(closing, &what) {
                return None;
            }
        }
        Some((out, damaged))
    }

    /// select/sort/group_by/agg 至少要有一项；失败时报在右括号上。
    fn require_nonempty(&mut self, message: &str) -> Option<()> {
        if *self.peek() == Tok::RParen {
            self.fail(Code::ParUnexpected, message.to_string());
            return None;
        }
        Some(())
    }

    // ---------- program / pipeline ----------

    fn parse_program(&mut self) -> Program {
        let pipeline = self.parse_pipeline();
        // 词法洞由词法层报过。走到这里还剩的洞都在流水线层，成不了程序：越过即可
        while *self.peek() == Tok::Error {
            self.advance();
        }
        Program { pipeline }
    }

    fn parse_pipeline(&mut self) -> Pipeline {
        let source_start = self.peek_span();
        let source = match self.parse_source("流水线必须") {
            Some(s) => s,
            // 源解析失败（已报告、已同步）：用空路径占位，继续解析后面的算子
            None => SourceOp::Table {
                path: String::new(),
                alias: None,
                span: source_start,
            },
        };
        let start = source.span();
        let mut ops: Vec<Op> = Vec::new();
        let mut end = start;
        let mut terminal: Option<TerminalOp> = None;

        // 流水线循环 = 流水线层的恢复点：坏掉的元素整个丢弃，绝不中断整条流水线。
        // 深度 0 上的任何 `.` 都只能是下一个算子的起点。
        loop {
            if *self.peek() == Tok::Error {
                self.advance();
                continue;
            }
            if *self.peek() == Tok::Eof {
                break;
            }
            if *self.peek() != Tok::Dot || self.depth() != 0 {
                // 流水线层冒出了一个不该有的 token。报一次，然后跳到下一个算子**继续**——
                // 后面的算子里可能还有独立的错误，不能因为一个多余的 `)` 就都不看了
                let got = describe(self.peek());
                let span = self.peek_span();
                self.diags.error(
                    Code::ParUnexpected,
                    format!("流水线在这里中断：多出了一个 {got}"),
                    span,
                );
                self.sync_pipeline();
                continue;
            }
            // 残骸模式下的元素是试探性的：它产生的诊断先记着，等确认它真的像一个算子再说
            let mark = self.diags.diags.len();
            let after_terminal = terminal.is_some();
            let element = self.parse_element(after_terminal);
            if self.debris && !self.ends_like_pipeline_element() {
                // 这个「算子」其实是没闭合的表达式的一部分：连同它的诊断一起丢弃
                self.diags.diags.truncate(mark);
                self.sync_pipeline();
                continue;
            }
            match element {
                Element::Op(o) => {
                    // 终端之后的算子照常解析（为了找出里面独立的错误），但不进 AST
                    if !after_terminal {
                        end = o.span();
                        ops.push(o);
                    }
                }
                Element::Terminal(t) => {
                    if terminal.is_none() {
                        end = t.span();
                        terminal = Some(t);
                    }
                }
                Element::Broken => self.sync_pipeline(),
            }
            if self.debris {
                // 吞掉迟到的右括号等残骸，停在下一个算子前
                self.sync_pipeline();
            }
        }
        Pipeline {
            source,
            ops,
            terminal,
            span: start.merge(end),
        }
    }

    /// 解析一个以 `.` 开头的元素（调用前 `.` 尚未消费）。
    /// 返回 `Broken` 时错误已经报告，由调用方跳到下一个算子。
    fn parse_element(&mut self, after_terminal: bool) -> Element {
        let dot_span = self.peek_span();
        self.advance(); // `.`
        let name_span = self.peek_span();
        let op_name = match self.expect_ident("算子名") {
            Some(n) => n,
            None => return Element::Broken,
        };
        let is_terminal = TERMINALS.contains(&op_name.as_str());
        let is_operator = OPERATORS.contains(&op_name.as_str());
        if !is_terminal && !is_operator {
            self.diags.error(
                Code::ParUnexpected,
                format!("未知算子 `{op_name}`"),
                name_span,
            );
            return Element::Broken;
        }
        if after_terminal && !self.par002_reported {
            // PAR-002：终端之后又接了算子。只报一次；后面的算子照常解析——它们可能是独立的笔误
            self.diags.error(
                Code::ParOpAfterTerminal,
                "终端算子之后不得再接续任何算子",
                dot_span.merge(name_span),
            );
            self.par002_reported = true;
        }
        if is_terminal {
            return match self.parse_terminal(&op_name) {
                Some(t) => Element::Terminal(t),
                None => Element::Broken,
            };
        }
        match self.parse_operator(&op_name) {
            Some(o) => Element::Op(o),
            None => Element::Broken,
        }
    }

    /// 解析数据源。`must` 是句子的主语，用来说清楚「谁必须以 Table 或 MemoryTable 开头」：
    /// 流水线的第一个 token 是它，`join(` 之后的右表也是它——
    /// 把后者报成「流水线必须……」就是在说假话。
    fn parse_source(&mut self, must: &str) -> Option<SourceOp> {
        let kw_span = self.peek_span();
        // 源只有一个：坏了就跳到下一个算子（源层的恢复点）
        let kw = match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                name
            }
            other => {
                if !self.eat_hole() {
                    let got = describe(&other);
                    self.fail(
                        Code::ParUnexpected,
                        format!("{must}以 Table 或 MemoryTable 开头，遇到 {got}"),
                    );
                }
                self.sync_pipeline();
                return None;
            }
        };
        if kw != "Table" && kw != "MemoryTable" {
            self.diags.error(
                Code::ParUnexpected,
                format!("{must}以 Table 或 MemoryTable 开头，遇到 `{kw}`"),
                kw_span,
            );
            self.sync_pipeline();
            return None;
        }
        if !self.expect_punct(&Tok::LParen, "`(`") {
            self.sync_pipeline();
            return None;
        }
        // sourceArg = stringLiteral | ident
        let arg = match self.peek().clone() {
            Tok::Str(s) => {
                self.advance();
                s
            }
            Tok::Ident(n) => {
                self.advance();
                n
            }
            other => {
                if !self.eat_hole() {
                    let got = describe(&other);
                    self.fail(
                        Code::ParUnexpected,
                        format!("期望数据源路径或名称，遇到 {got}"),
                    );
                }
                self.sync_pipeline();
                return None;
            }
        };
        if !self.expect_punct(&Tok::RParen, "`)`") {
            self.sync_pipeline();
            return None;
        }
        // 词法洞不算内容（词法层已经报过）：`Table("a.csv") ^ as o` 仍然带着别名
        while *self.peek() == Tok::Error {
            self.advance();
        }
        // [as ident]
        let mut alias = None;
        if self.accept_kw("as") {
            match self.expect_ident("源别名") {
                Some(a) => alias = Some(a),
                None => {
                    self.sync_pipeline();
                    return None;
                }
            }
        }
        let span = kw_span.merge(self.last_span());
        if kw == "Table" {
            Some(SourceOp::Table {
                path: arg,
                alias,
                span,
            })
        } else {
            Some(SourceOp::MemoryTable {
                name: arg,
                alias,
                span,
            })
        }
    }

    fn parse_terminal(&mut self, name: &str) -> Option<TerminalOp> {
        let start = self.last_span();
        if !self.expect_punct(&Tok::LParen, "`(`") {
            return None;
        }
        let terminal = match name {
            "write" => {
                let path = match self.peek().clone() {
                    Tok::Str(s) => {
                        self.advance();
                        s
                    }
                    other => {
                        if !self.eat_hole() {
                            self.fail(
                                Code::ParUnexpected,
                                format!("write 的参数必须是字符串路径，遇到 {}", describe(&other)),
                            );
                        }
                        return None;
                    }
                };
                TerminalOp::Write { path, span: start }
            }
            "collect" => TerminalOp::Collect { span: start },
            "print" => TerminalOp::Print { span: start },
            _ => return None,
        };
        let arity = if name == "write" {
            Arity::One
        } else {
            Arity::Empty
        };
        if !self.expect_close(&format!(".{name}("), arity) {
            return None;
        }
        Some(terminal)
    }

    fn parse_operator(&mut self, name: &str) -> Option<Op> {
        let start = self.last_span(); // 算子名
        if !self.expect_punct(&Tok::LParen, "`(`") {
            // 连左括号都没有：这一段无法再按算子解析，直接跳到下一个算子
            self.sync_pipeline();
            return None;
        }
        let opened = format!(".{name}(");
        let op = match name {
            "filter" => {
                let pred = self.parse_expr()?;
                if !self.expect_close(&opened, Arity::One) {
                    return None;
                }
                Op::Filter {
                    pred,
                    span: start.merge(self.last_span()),
                }
            }
            "extend" => {
                // PAR-003：参数必须是记录字面量
                if *self.peek() != Tok::LBrace {
                    if !self.eat_hole() {
                        let got = describe(self.peek());
                        self.fail(
                            Code::ParExtendNotRecord,
                            format!("extend 的参数必须是记录字面量 `{{ ... }}`，遇到 {got}"),
                        );
                    }
                    return None;
                }
                let record = self.parse_record_literal()?;
                if !self.expect_close(&opened, Arity::One) {
                    return None;
                }
                Op::Extend {
                    record,
                    span: start.merge(self.last_span()),
                }
            }
            "select" => {
                self.require_nonempty("select 至少需要一个输出项")?;
                let (items, damaged) = self.parse_list(&Tok::RParen, "select", |p| {
                    let item_start = p.peek_span();
                    let expr = p.parse_expr()?;
                    let mut alias = None;
                    if p.accept_kw("as") {
                        alias = Some(p.expect_ident("派生列名")?);
                    }
                    // PAR-006：无 as 时必须是字段引用
                    if alias.is_none() && !is_field_ref(&expr) {
                        p.fail_at(
                            Code::ParSelectItemUnnamed,
                            "select 的复合表达式必须用 `as name` 命名；无 as 的项只能是字段引用",
                            expr.span(),
                        );
                        return None;
                    }
                    let span = item_start.merge(p.last_span());
                    Some(SelectItem { expr, alias, span })
                })?;
                if damaged {
                    return None;
                }
                Op::Select {
                    items,
                    span: start.merge(self.last_span()),
                }
            }
            "distinct" => {
                if !self.expect_close(&opened, Arity::Empty) {
                    return None;
                }
                Op::Distinct {
                    span: start.merge(self.last_span()),
                }
            }
            "sort" => {
                self.require_nonempty("sort 至少需要一个排序键")?;
                let (keys, damaged) = self.parse_list(&Tok::RParen, "sort", |p| {
                    let key_start = p.peek_span();
                    let expr = p.parse_expr()?;
                    let mut descending = false;
                    if matches!(p.peek(), Tok::Ident(dir) if dir == "asc" || dir == "desc") {
                        descending = matches!(p.peek(), Tok::Ident(dir) if dir == "desc");
                        p.advance();
                    }
                    Some(SortKey {
                        expr,
                        descending,
                        span: key_start.merge(p.last_span()),
                    })
                })?;
                if damaged {
                    return None;
                }
                Op::Sort {
                    keys,
                    span: start.merge(self.last_span()),
                }
            }
            "take" => {
                let n = match self.peek().clone() {
                    Tok::Int(v) => {
                        self.advance();
                        // 编译期可确定的越界（02-lexical：超出 i64 范围报错）
                        if v > i64::MAX as i128 || v < i64::MIN as i128 {
                            self.fail_at(
                                Code::SemConstDomain,
                                format!("take 的参数 `{v}` 超出 i64 范围"),
                                self.last_span(),
                            );
                            return None;
                        }
                        v as i64
                    }
                    other => {
                        if !self.eat_hole() {
                            self.fail(
                                Code::ParUnexpected,
                                format!(
                                    "take 的参数必须是非负整数字面量，遇到 {}",
                                    describe(&other)
                                ),
                            );
                        }
                        return None;
                    }
                };
                if !self.expect_close(&opened, Arity::One) {
                    return None;
                }
                Op::Take {
                    n,
                    span: start.merge(self.last_span()),
                }
            }
            "join" => {
                let right = self.parse_source("join 的右表必须")?;
                if !self.accept_punct(&Tok::Comma) {
                    if !self.eat_hole() {
                        let got = describe(self.peek());
                        self.fail(
                            Code::ParUnexpected,
                            format!("join 需要 `,` 分隔右表与 on 条件，遇到 {got}"),
                        );
                    }
                    return None;
                }
                // on = expr；缺 `=` 报 PAR-007
                let on_span = self.peek_span();
                let on_kw = self.expect_ident("`on`")?;
                if on_kw != "on" {
                    self.fail_at(
                        Code::ParJoinOnMalformed,
                        format!("join 的第二个参数必须是固定绑定 `on = expr`，遇到 `{on_kw}`"),
                        on_span,
                    );
                    return None;
                }
                if !self.accept_punct(&Tok::Assign) {
                    if !self.eat_hole() {
                        let got = describe(self.peek());
                        self.fail(
                            Code::ParJoinOnMalformed,
                            format!("`on` 之后必须是 `=`（固定绑定名），遇到 {got}"),
                        );
                    }
                    return None;
                }
                let on = self.parse_expr()?;
                self.accept_punct(&Tok::Comma);
                if !self.expect_close(&opened, Arity::Join) {
                    return None;
                }
                Op::Join {
                    right,
                    on,
                    span: start.merge(self.last_span()),
                }
            }
            "group_by" => {
                self.require_nonempty("group_by 至少需要一个分组键")?;
                let (keys, damaged) = self.parse_list(&Tok::RParen, "group_by", |p| {
                    let key_start = p.peek_span();
                    let expr = p.parse_expr()?;
                    let mut alias = None;
                    if p.accept_kw("as") {
                        alias = Some(p.expect_ident("分组键输出名")?);
                    }
                    Some(GroupKey {
                        expr,
                        alias,
                        span: key_start.merge(p.last_span()),
                    })
                })?;
                if damaged {
                    return None;
                }
                Op::GroupBy {
                    keys,
                    span: start.merge(self.last_span()),
                }
            }
            "agg" => {
                self.require_nonempty("agg 至少需要一个绑定")?;
                let (bindings, damaged) = self.parse_list(&Tok::RParen, "agg", |p| {
                    let b_start = p.peek_span();
                    let name = p.expect_ident("聚合绑定名")?;
                    if !p.accept_punct(&Tok::Assign) {
                        if !p.eat_hole() {
                            let got = describe(p.peek());
                            p.fail(
                                Code::ParUnexpected,
                                format!("聚合绑定 `{name} = aggCall(...)`，遇到 {got}"),
                            );
                        }
                        return None;
                    }
                    let call = p.parse_agg_call()?;
                    Some(AggBinding {
                        name,
                        call,
                        span: b_start.merge(p.last_span()),
                    })
                })?;
                if damaged {
                    return None;
                }
                Op::Agg {
                    bindings,
                    span: start.merge(self.last_span()),
                }
            }
            _ => return None,
        };
        Some(op)
    }

    fn parse_agg_call(&mut self) -> Option<AggCall> {
        let start = self.peek_span();
        // PAR-005：绑定右侧必须是直接聚合调用
        let fn_name = match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                name
            }
            other => {
                if !self.eat_hole() {
                    self.fail(
                        Code::ParAggNotAggCall,
                        format!(
                            "agg 绑定右侧必须是聚合调用（count/sum/min/max/avg），遇到 {}",
                            describe(&other)
                        ),
                    );
                }
                return None;
            }
        };
        if !AGG_FNS.contains(&fn_name.as_str()) {
            self.fail_at(
                Code::ParAggNotAggCall,
                format!("agg 绑定右侧必须是聚合调用（count/sum/min/max/avg），遇到 `{fn_name}`"),
                start,
            );
            return None;
        }
        if !self.expect_punct(&Tok::LParen, "`(`") {
            return None;
        }
        let mut arg = None;
        if *self.peek() != Tok::RParen {
            arg = Some(self.parse_expr()?);
        }
        if !self.expect_close(&format!("{fn_name}("), Arity::One) {
            return None;
        }
        let func = match fn_name.as_str() {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => AggFunc::Avg,
        };
        Some(AggCall {
            func,
            arg,
            span: start.merge(self.last_span()),
        })
    }

    fn parse_record_literal(&mut self) -> Option<RecordLiteral> {
        let start = self.peek_span();
        if !self.expect_punct(&Tok::LBrace, "`{`") {
            return None;
        }
        let (bindings, damaged) = self.parse_list(&Tok::RBrace, "extend", |p| {
            let b_start = p.peek_span();
            let name = p.expect_ident("记录绑定名")?;
            if !p.accept_punct(&Tok::Assign) {
                if !p.eat_hole() {
                    let got = describe(p.peek());
                    p.fail(
                        Code::ParUnexpected,
                        format!("记录绑定 `{name} = 表达式`，遇到 {got}"),
                    );
                }
                return None;
            }
            let value = p.parse_expr()?;
            Some(Binding {
                name,
                value,
                span: b_start.merge(p.last_span()),
            })
        })?;
        if damaged {
            return None;
        }
        Some(RecordLiteral {
            bindings,
            span: start.merge(self.last_span()),
        })
    }

    // ---------- 表达式（优先级自低到高）----------
    //
    // 表达式层没有恢复点：任何一层拿到 `None` 都原样上抛（`?`）。
    // 接住它的，是拥有「这道括号」的那一层——列表循环或流水线循环。
    //
    // 但「上抛」和「深入」都是递归：递归下降解析器靠调用栈记状态，输入嵌套多深，栈就有多深。
    // 所以凡是递归调用自己的地方都经过 `descend`，超过 `MAX_NESTING` 层就报错而不是撑爆栈。

    /// 深入一层表达式递归。超过上限时报 PAR-001 并返回 `None`（由调用方用 `?` 接住上抛）。
    fn descend(&mut self, f: impl FnOnce(&mut Self) -> Option<Expr>) -> Option<Expr> {
        self.nesting += 1;
        let result = if self.nesting > MAX_NESTING {
            self.fail(
                Code::ParUnexpected,
                format!("表达式嵌套过深（最多 {MAX_NESTING} 层）"),
            );
            None
        } else {
            f(self)
        };
        self.nesting -= 1;
        result
    }

    fn parse_expr(&mut self) -> Option<Expr> {
        if self.nesting == 0 {
            // 最外层表达式的起点：原子计数从这里重新开始
            self.atoms = 0;
        }
        self.parse_or()
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut left = self.parse_and()?;
        while *self.peek() == Tok::OrOr {
            self.advance();
            let right = self.parse_and()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Some(left)
    }

    fn parse_and(&mut self) -> Option<Expr> {
        let mut left = self.parse_not()?;
        while *self.peek() == Tok::AndAnd {
            self.advance();
            let right = self.parse_not()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Some(left)
    }

    fn parse_not(&mut self) -> Option<Expr> {
        if *self.peek() == Tok::Not {
            let start = self.peek_span();
            self.advance();
            // `!` 右结合：`!!!x` 是一层套一层的递归，所以经过 `descend`
            let expr = self.descend(|p| p.parse_not())?;
            let span = start.merge(expr.span());
            Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
                span,
            })
        } else {
            self.parse_cmp()
        }
    }

    fn parse_cmp(&mut self) -> Option<Expr> {
        let left = self.parse_add()?;
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Neq => Some(BinOp::Neq),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Gte => Some(BinOp::Gte),
            Tok::Lte => Some(BinOp::Lte),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.parse_add()?;
            let span = left.span().merge(right.span());
            return Some(Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            });
        }
        Some(left)
    }

    fn parse_add(&mut self) -> Option<Expr> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_mul()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Some(left)
    }

    fn parse_mul(&mut self) -> Option<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Some(left)
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        if *self.peek() == Tok::Minus {
            let start = self.peek_span();
            self.advance();
            // 一元 `-` 右结合，同 `!`
            let expr = self.descend(|p| p.parse_unary())?;
            let span = start.merge(expr.span());
            return Some(Expr::Unary {
                op: UnaryOp::Negate,
                expr: Box::new(expr),
                span,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Option<Expr> {
        let mut expr = self.parse_primary()?;
        while *self.peek() == Tok::Dot {
            if self.at_operator_start() {
                // 表达式里不可能出现 `.名字(`。两种常见成因：上一个算子少了 `)`；
                // 或者按别的语言的习惯写了方法调用（`o.x.abs(`）
                let name = self.op_name_ahead().unwrap_or("…");
                self.fail_hint(
                    Code::ParUnexpected,
                    format!("表达式内部出现了 `.{name}(`"),
                    "表达式里的 `.` 只能用于 `别名.字段`，不能调用方法；\
                     如果这其实是下一个算子，说明前面的 `(` 缺少 `)`",
                );
                return None;
            }
            self.advance();
            let name = self.expect_ident("字段名")?;
            // 最多两级：alias.name 或 name；更多级报 PAR-001
            match expr {
                Expr::Field {
                    alias: None,
                    name: base,
                    span,
                } => {
                    let new_span = span.merge(self.last_span());
                    expr = Expr::Field {
                        alias: Some(base),
                        name,
                        span: new_span,
                    };
                }
                other => {
                    // `.` 和名字都已读入：错误要报在整段引用（如 `a.b.c`）上
                    let span = other.span().merge(self.last_span());
                    self.fail_spanned(
                        Code::ParUnexpected,
                        "`.` 只支持 `别名.字段` 两级引用",
                        span,
                        Some("引用最多两级，例如 `o.amount`；不支持 `a.b.c` 或方法调用"),
                    );
                    return None;
                }
            }
        }
        Some(expr)
    }

    /// 表达式的原子：字面量、字段、括号、调用、if。所有递归下降的「深入」都经过这里，
    /// 所以在这里既计层数（`descend`）也计原子个数。
    fn parse_primary(&mut self) -> Option<Expr> {
        self.descend(|p| p.parse_primary_inner())
    }

    fn parse_primary_inner(&mut self) -> Option<Expr> {
        self.atoms += 1;
        if self.atoms > MAX_ATOMS {
            self.fail(
                Code::ParUnexpected,
                format!("表达式过长（单个表达式最多 {MAX_ATOMS} 个原子）"),
            );
            return None;
        }
        let span = self.peek_span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.advance();
                // 编译期可确定的整数字面量越界（02-lexical：超出 i64 范围报错）
                if v > i64::MAX as i128 || v < i64::MIN as i128 {
                    self.fail_at(
                        Code::SemConstDomain,
                        format!("整数字字面量 `{v}` 超出 i64 范围"),
                        span,
                    );
                    return None;
                }
                Some(Expr::Literal(Literal::Int(v as i64), span))
            }
            Tok::Float(v) => {
                self.advance();
                // MVP f64 值域只含有限值（05-type-system）
                if !v.is_finite() {
                    self.fail_at(
                        Code::SemLiteralOutOfRange,
                        "浮点字面量超出 MVP 有限 f64 值域",
                        span,
                    );
                    return None;
                }
                Some(Expr::Literal(Literal::Float(v), span))
            }
            Tok::Str(s) => {
                self.advance();
                Some(Expr::Literal(Literal::Str(s), span))
            }
            Tok::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                if !self.expect_punct(&Tok::RParen, "`)`") {
                    return None;
                }
                Some(inner)
            }
            Tok::Ident(name) => match name.as_str() {
                "true" | "false" => {
                    self.advance();
                    Some(Expr::Literal(Literal::Bool(name == "true"), span))
                }
                "null" => {
                    self.advance();
                    Some(Expr::Literal(Literal::Null, span))
                }
                "if" => self.parse_if(),
                _ => {
                    self.advance();
                    // 函数调用：ident "(" ... ")"
                    if *self.peek() == Tok::LParen {
                        self.advance();
                        let owner = format!("{name}(");
                        let (args, damaged) =
                            self.parse_list(&Tok::RParen, &owner, |p| p.parse_expr())?;
                        if damaged {
                            return None;
                        }
                        return Some(Expr::Call {
                            name,
                            args,
                            span: span.merge(self.last_span()),
                        });
                    }
                    Some(Expr::Field {
                        alias: None,
                        name,
                        span,
                    })
                }
            },
            Tok::Error => {
                // 词法层留下的洞：词法层已报错，这里只负责越过并同步
                self.eat_hole();
                None
            }
            other => {
                if other == Tok::LBrace {
                    // PAR-003：记录字面量只允许出现在 extend 参数位。
                    // 不过走到这里的 `{` 多半根本不是记录字面量，而是 if 的分支——
                    // 条件里少了操作数，`{ 1 } else { … }` 就被当成了一个操作数
                    self.fail_hint(
                        Code::ParExtendNotRecord,
                        "这里需要一个表达式，却遇到了 `{`（记录字面量只能作为 extend 的参数）",
                        "如果这是 if 的分支：`{` 之前必须是一个写完整的表达式，请检查条件是否漏了操作数",
                    );
                } else {
                    let got = describe(&other);
                    self.fail(Code::ParUnexpected, format!("期望表达式，遇到 {got}"));
                }
                None
            }
        }
    }

    /// `if c { a } [else if c2 { b }] ... else { z }` → 嵌套单条件 If。
    /// 调用前 `if` 关键字已由 parse_primary 分派但未消费；这里消费它。
    fn parse_if(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        self.advance(); // 消费 "if"
        self.parse_if_body(start)
    }

    /// 解析 if 的条件/分支部分（`if` 关键字已消费）。else-if 链脱糖为嵌套 If。
    fn parse_if_body(&mut self, start: Span) -> Option<Expr> {
        if *self.peek() == Tok::LBrace {
            // 常见笔误：把条件写进了 `{ }`。具体的错误要抢在通用的之前——
            // 否则它会一路掉进 parse_primary，被报成「记录字面量只能给 extend 用」
            self.fail_hint(
                Code::ParIfMalformed,
                "if 的条件不能放进 `{ }`",
                "写法：`if 条件 { 值 } else { 值 }`——条件写在第一个 `{` 之前",
            );
            return None;
        }
        let condition = self.parse_expr()?;
        if !self.expect_punct(&Tok::LBrace, "`{`（then 分支）") {
            return None;
        }
        let then = self.parse_expr()?;
        if !self.expect_punct(&Tok::RBrace, "`}`（then 分支结束）") {
            return None;
        }
        if !self.accept_kw("else") {
            // PAR-004：缺 else
            self.fail_hint(
                Code::ParIfMalformed,
                "if 表达式必须以 `else { ... }` 结束",
                "`if` 是表达式：两个分支都要有值，请补上 `else { ... }`",
            );
            return None;
        }
        let else_ = if self.accept_kw("if") {
            // `else if ...` → 嵌套 if（其 `if` 关键字已消费）。长长的一串 else-if 也是递归
            Box::new(self.descend(|p| p.parse_if_body(start))?)
        } else {
            if !self.expect_punct(&Tok::LBrace, "`{`（else 分支）") {
                return None;
            }
            let body = self.parse_expr()?;
            if !self.expect_punct(&Tok::RBrace, "`}`（else 分支结束）") {
                return None;
            }
            Box::new(body)
        };
        let span = start.merge(else_.span());
        Some(Expr::If {
            condition: Box::new(condition),
            then: Box::new(then),
            else_,
            span,
        })
    }
}

fn is_field_ref(expr: &Expr) -> bool {
    matches!(expr, Expr::Field { .. })
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Ident(s) => format!("标识符 `{s}`"),
        Tok::Int(v) => format!("整数 `{v}`"),
        Tok::Float(v) => format!("浮点数 `{v:?}`"),
        Tok::Str(_) => "字符串字面量".to_string(),
        Tok::Error => "无法识别的字符".to_string(),
        Tok::LParen => "`(`".to_string(),
        Tok::RParen => "`)`".to_string(),
        Tok::LBrace => "`{`".to_string(),
        Tok::RBrace => "`}`".to_string(),
        Tok::Comma => "`,`".to_string(),
        Tok::Dot => "`.`".to_string(),
        Tok::Eq => "`==`".to_string(),
        Tok::Assign => "`=`".to_string(),
        Tok::Colon => "`:`".to_string(),
        Tok::Neq => "`!=`".to_string(),
        Tok::Gt => "`>`".to_string(),
        Tok::Lt => "`<`".to_string(),
        Tok::Gte => "`>=`".to_string(),
        Tok::Lte => "`<=`".to_string(),
        Tok::Plus => "`+`".to_string(),
        Tok::Minus => "`-`".to_string(),
        Tok::Star => "`*`".to_string(),
        Tok::Slash => "`/`".to_string(),
        Tok::Percent => "`%`".to_string(),
        Tok::AndAnd => "`&&`".to_string(),
        Tok::OrOr => "`||`".to_string(),
        Tok::Not => "`!`".to_string(),
        Tok::Eof => "文件结尾".to_string(),
    }
}
