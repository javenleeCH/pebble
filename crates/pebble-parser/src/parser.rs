//! 递归下降解析器：按 grammar/pebble.ebnf Draft v0.2。
//! 关键文法点：
//! - `extend(...)` 参数必须是记录字面量（PAR-003）。
//! - `join(..., on = expr,)`：`on` 是固定绑定名，缺 `=` 报 PAR-007。
//! - `selectItem = expr [as ident]`；无 `as` 的复合表达式报 PAR-006。
//! - `ifExpr = if expr { expr } [else if expr { expr }...] else { expr }`，脱糖为单条件嵌套 If。
//! - select/sort/group_by/agg/join/记录字面量统一允许尾逗号。
//! - 遇到首个 PAR 错误即中止（MVP 简化：不做多错误恢复；词法错误先行收集）。

use crate::ast::*;
use crate::lexer::{Tok, Token};
use pebble_diagnostic::{Code, DiagBag, Span};

const OPERATORS: &[&str] = &[
    "filter", "extend", "select", "distinct", "sort", "take", "join", "group_by", "agg",
];
const TERMINALS: &[&str] = &["write", "collect", "print"];
const AGG_FNS: &[&str] = &["count", "sum", "min", "max", "avg"];

pub struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    diags: &'a mut DiagBag,
}

/// 解析入口：词法 + 语法。返回 None 表示存在编译期错误（诊断已收集）。
pub fn parse(source: &str, diags: &mut DiagBag) -> Option<Program> {
    let toks = crate::lexer::lex(source, diags);
    if diags.has_error() {
        return None;
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        diags,
    };
    let program = p.parse_program();
    if p.diags.has_error() {
        return None;
    }
    Some(program)
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn err_here(&mut self, code: Code, message: impl Into<String>) {
        let span = self.peek_span();
        self.diags.error(code, message, span);
    }

    fn expect_punct(&mut self, t: &Tok, what: &str) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            let got = describe(self.peek());
            self.err_here(
                Code::ParUnexpected,
                format!("期望 {}，遇到 {}", what, got),
            );
            false
        }
    }

    fn expect_ident(&mut self, what: &str) -> Option<String> {
        if let Tok::Ident(name) = self.peek().clone() {
            self.advance();
            Some(name)
        } else {
            let got = describe(self.peek());
            self.err_here(Code::ParUnexpected, format!("期望{}，遇到 {}", what, got));
            None
        }
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
        if let Tok::Ident(name) = self.peek() {
            if name == kw {
                self.advance();
                return true;
            }
        }
        false
    }

    // ---------- program / pipeline ----------

    fn parse_program(&mut self) -> Program {
        let pipeline = self.parse_pipeline();
        if !self.diags.has_error() && self.peek() != &Tok::Eof {
            let got = describe(self.peek());
            self.err_here(Code::ParUnexpected, format!("期望文件结尾，遇到 {}", got));
        }
        Program { pipeline }
    }

    fn parse_pipeline(&mut self) -> Pipeline {
        let source = self.parse_source();
        let start = source.span();
        let mut ops: Vec<Op> = Vec::new();
        let mut end = start;
        // 变换算子链
        while self.peek() == &Tok::Dot {
            let dot_span = self.peek_span();
            self.advance();
            let op_name = match self.expect_ident("算子名") {
                Some(n) => n,
                None => break,
            };
            if TERMINALS.contains(&op_name.as_str()) {
                // 终端算子：必须是最后一个
                let terminal = self.parse_terminal(&op_name);
                if let Some(t) = terminal {
                    end = t.span();
                    if self.peek() == &Tok::Dot {
                        // 终端之后继续接算子
                        self.advance();
                        let _ = self.expect_ident("算子名");
                        self.err_here(Code::ParOpAfterTerminal, "终端算子之后不得再接续任何算子");
                        return Pipeline {
                            source,
                            ops,
                            terminal: Some(t),
                            span: Span::new(start.start, end.end),
                        };
                    }
                    return Pipeline {
                        source,
                        ops,
                        terminal: Some(t),
                        span: start.merge(end),
                    };
                }
                break;
            }
            if !OPERATORS.contains(&op_name.as_str()) {
                self.diags.error(
                    Code::ParUnexpected,
                    format!("未知算子 `{}`", op_name),
                    dot_span,
                );
                break;
            }
            let op = self.parse_operator(&op_name);
            match op {
                Some(o) => {
                    end = o.span();
                    ops.push(o);
                }
                None => break,
            }
        }
        Pipeline {
            source,
            ops,
            terminal: None,
            span: start.merge(end),
        }
    }

    fn parse_source(&mut self) -> SourceOp {
        let kw_span = self.peek_span();
        let kw = match self.expect_ident("Table 或 MemoryTable") {
            Some(n) => n,
            None => {
                // 不可恢复：占位源，语义层会因缺失终止报错；此处已记录 PAR-001。
                return SourceOp::Table {
                    path: String::new(),
                    alias: None,
                    span: kw_span,
                };
            }
        };
        let span0 = kw_span;
        if kw != "Table" && kw != "MemoryTable" {
            self.diags.error(
                Code::ParUnexpected,
                format!("流水线必须以 Table 或 MemoryTable 开头，遇到 `{}`", kw),
                kw_span,
            );
            return SourceOp::Table {
                path: String::new(),
                alias: None,
                span: kw_span,
            };
        }
        if !self.expect_punct(&Tok::LParen, "`(`") {
            return SourceOp::Table {
                path: String::new(),
                alias: None,
                span: kw_span,
            };
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
                self.diags.error(
                    Code::ParUnexpected,
                    format!("期望数据源路径或名称，遇到 {}", describe(&other)),
                    self.peek_span(),
                );
                return SourceOp::Table {
                    path: String::new(),
                    alias: None,
                    span: kw_span,
                };
            }
        };
        self.expect_punct(&Tok::RParen, "`)`");
        // [as ident]
        let mut alias = None;
        let mut end = self.toks[self.pos.saturating_sub(1)].span;
        if self.accept_kw("as") {
            if let Some(a) = self.expect_ident("源别名") {
                alias = Some(a);
                end = self.toks[self.pos.saturating_sub(1)].span;
            }
        }
        let span = Span::new(span0.start, end.end);
        if kw == "Table" {
            SourceOp::Table {
                path: arg,
                alias,
                span,
            }
        } else {
            SourceOp::MemoryTable {
                name: arg,
                alias,
                span,
            }
        }
    }

    fn parse_terminal(&mut self, name: &str) -> Option<TerminalOp> {
        let start = self.toks[self.pos.saturating_sub(1)].span;
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
                        self.diags.error(
                            Code::ParUnexpected,
                            format!("write 的参数必须是字符串路径，遇到 {}", describe(&other)),
                            self.peek_span(),
                        );
                        return None;
                    }
                };
                TerminalOp::Write {
                    path,
                    span: start,
                }
            }
            "collect" => TerminalOp::Collect { span: start },
            "print" => TerminalOp::Print { span: start },
            _ => return None,
        };
        self.expect_punct(&Tok::RParen, "`)`");
        Some(terminal)
    }

    fn parse_operator(&mut self, name: &str) -> Option<Op> {
        let start = self.toks[self.pos.saturating_sub(1)].span;
        if !self.expect_punct(&Tok::LParen, "`(`") {
            return None;
        }
        let op = match name {
            "filter" => {
                let pred = self.parse_expr()?;
                Op::Filter {
                    pred,
                    span: start,
                }
            }
            "extend" => {
                // PAR-003：参数必须是记录字面量
                if self.peek() != &Tok::LBrace {
                    let got = describe(self.peek());
                    self.err_here(
                        Code::ParExtendNotRecord,
                        format!("extend 的参数必须是记录字面量 `{{ ... }}`，遇到 {}", got),
                    );
                    return None;
                }
                let record = self.parse_record_literal()?;
                Op::Extend {
                    record,
                    span: start,
                }
            }
            "select" => {
                let mut items = Vec::new();
                loop {
                    if self.peek() == &Tok::RParen {
                        break;
                    }
                    let item_start = self.peek_span();
                    let expr = self.parse_expr()?;
                    let mut alias = None;
                    if self.accept_kw("as") {
                        alias = self.expect_ident("派生列名");
                    }
                    // PAR-006：无 as 时必须是字段引用
                    if alias.is_none() && !is_field_ref(&expr) {
                        self.diags.error(
                            Code::ParSelectItemUnnamed,
                            "select 的复合表达式必须用 `as name` 命名；无 as 的项只能是字段引用",
                            expr.span(),
                        );
                        return None;
                    }
                    let span = item_start.merge(self.toks[self.pos.saturating_sub(1)].span);
                    items.push(SelectItem {
                        expr,
                        alias,
                        span,
                    });
                    if self.accept_punct(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
                if items.is_empty() {
                    self.err_here(Code::ParUnexpected, "select 至少需要一个输出项");
                    return None;
                }
                self.accept_punct(&Tok::Comma);
                Op::Select {
                    items,
                    span: start,
                }
            }
            "distinct" => Op::Distinct { span: start },
            "sort" => {
                let mut keys = Vec::new();
                loop {
                    if self.peek() == &Tok::RParen {
                        break;
                    }
                    let key_start = self.peek_span();
                    let expr = self.parse_expr()?;
                    let mut descending = false;
                    if let Tok::Ident(dir) = self.peek().clone() {
                        if dir == "asc" || dir == "desc" {
                            self.advance();
                            descending = dir == "desc";
                        }
                    }
                    let span = key_start.merge(self.toks[self.pos.saturating_sub(1)].span);
                    keys.push(SortKey {
                        expr,
                        descending,
                        span,
                    });
                    if self.accept_punct(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
                if keys.is_empty() {
                    self.err_here(Code::ParUnexpected, "sort 至少需要一个排序键");
                    return None;
                }
                self.accept_punct(&Tok::Comma);
                Op::Sort {
                    keys,
                    span: start,
                }
            }
            "take" => {
                let n = match self.peek().clone() {
                    Tok::Int(v) => {
                        self.advance();
                        if v > i64::MAX as i128 || v < i64::MIN as i128 {
                            self.diags.error(
                                Code::SemConstDomain,
                                format!("take 的参数 `{}` 超出 i64 范围", v),
                                self.toks[self.pos.saturating_sub(1)].span,
                            );
                            return None;
                        }
                        v as i64
                    }
                    other => {
                        self.diags.error(
                            Code::ParUnexpected,
                            format!("take 的参数必须是非负整数字面量，遇到 {}", describe(&other)),
                            self.peek_span(),
                        );
                        return None;
                    }
                };
                Op::Take {
                    n,
                    span: start,
                }
            }
            "join" => {
                let right = self.parse_source();
                if !self.accept_punct(&Tok::Comma) {
                    let got = describe(self.peek());
                    self.err_here(Code::ParUnexpected, format!("join 需要 `,` 分隔右表与 on 条件，遇到 {}", got));
                    return None;
                }
                // on = expr；缺 `=` 报 PAR-007
                let on_kw = self.expect_ident("`on`")?;
                if on_kw != "on" {
                    self.diags.error(
                        Code::ParJoinOnMalformed,
                        format!("join 的第二个参数必须是固定绑定 `on = expr`，遇到 `{}`", on_kw),
                        self.toks[self.pos.saturating_sub(1)].span,
                    );
                    return None;
                }
                if !self.accept_punct(&Tok::Assign) {
                    let got = describe(self.peek());
                    self.err_here(
                        Code::ParJoinOnMalformed,
                        format!("`on` 之后必须是 `=`（固定绑定名），遇到 {}", got),
                    );
                    return None;
                }
                let on = self.parse_expr()?;
                self.accept_punct(&Tok::Comma);
                Op::Join {
                    right,
                    on,
                    span: start,
                }
            }
            "group_by" => {
                let mut keys = Vec::new();
                loop {
                    if self.peek() == &Tok::RParen {
                        break;
                    }
                    let key_start = self.peek_span();
                    let expr = self.parse_expr()?;
                    let mut alias = None;
                    if self.accept_kw("as") {
                        alias = self.expect_ident("分组键输出名");
                    }
                    let span = key_start.merge(self.toks[self.pos.saturating_sub(1)].span);
                    keys.push(GroupKey {
                        expr,
                        alias,
                        span,
                    });
                    if self.accept_punct(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
                if keys.is_empty() {
                    self.err_here(Code::ParUnexpected, "group_by 至少需要一个分组键");
                    return None;
                }
                self.accept_punct(&Tok::Comma);
                Op::GroupBy {
                    keys,
                    span: start,
                }
            }
            "agg" => {
                let mut bindings = Vec::new();
                loop {
                    if self.peek() == &Tok::RParen {
                        break;
                    }
                    let b_start = self.peek_span();
                    let name = self.expect_ident("聚合绑定名")?;
                    if !self.accept_punct(&Tok::Assign) {
                        let got = describe(self.peek());
                        self.err_here(
                            Code::ParUnexpected,
                            format!("聚合绑定 `{} = aggCall(...)`，遇到 {}", name, got),
                        );
                        return None;
                    }
                    let call = self.parse_agg_call()?;
                    let span = b_start.merge(self.toks[self.pos.saturating_sub(1)].span);
                    bindings.push(AggBinding {
                        name,
                        call,
                        span,
                    });
                    if self.accept_punct(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
                if bindings.is_empty() {
                    self.err_here(Code::ParUnexpected, "agg 至少需要一个绑定");
                    return None;
                }
                self.accept_punct(&Tok::Comma);
                Op::Agg {
                    bindings,
                    span: start,
                }
            }
            _ => return None,
        };
        self.expect_punct(&Tok::RParen, "`)`");
        let end = self.toks[self.pos.saturating_sub(1)].span;
        let mut op = op;
        set_span(&mut op, start.merge(end));
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
                self.diags.error(
                    Code::ParAggNotAggCall,
                    format!(
                        "agg 绑定右侧必须是聚合调用（count/sum/min/max/avg），遇到 {}",
                        describe(&other)
                    ),
                    start,
                );
                return None;
            }
        };
        if !AGG_FNS.contains(&fn_name.as_str()) {
            self.diags.error(
                Code::ParAggNotAggCall,
                format!("agg 绑定右侧必须是聚合调用（count/sum/min/max/avg），遇到 `{}`", fn_name),
                start,
            );
            return None;
        }
        if !self.expect_punct(&Tok::LParen, "`(`") {
            return None;
        }
        let mut arg = None;
        if self.peek() != &Tok::RParen {
            arg = Some(self.parse_expr()?);
        }
        self.expect_punct(&Tok::RParen, "`)`");
        let end = self.toks[self.pos.saturating_sub(1)].span;
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
            span: start.merge(end),
        })
    }

    fn parse_record_literal(&mut self) -> Option<RecordLiteral> {
        let start = self.peek_span();
        if !self.expect_punct(&Tok::LBrace, "`{`") {
            return None;
        }
        let mut bindings = Vec::new();
        loop {
            if self.peek() == &Tok::RBrace {
                break;
            }
            let b_start = self.peek_span();
            let name = self.expect_ident("记录绑定名")?;
            if !self.accept_punct(&Tok::Assign) {
                let got = describe(self.peek());
                self.err_here(
                    Code::ParUnexpected,
                    format!("记录绑定 `{} = 表达式`，遇到 {}", name, got),
                );
                return None;
            }
            let value = self.parse_expr()?;
            let span = b_start.merge(self.toks[self.pos.saturating_sub(1)].span);
            bindings.push(Binding {
                name,
                value,
                span,
            });
            if self.accept_punct(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.accept_punct(&Tok::Comma);
        self.expect_punct(&Tok::RBrace, "`}`");
        let end = self.toks[self.pos.saturating_sub(1)].span;
        Some(RecordLiteral {
            bindings,
            span: start.merge(end),
        })
    }

    // ---------- 表达式（优先级自低到高） ----------

    fn parse_expr(&mut self) -> Option<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut left = self.parse_and()?;
        while self.peek() == &Tok::OrOr {
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
        while self.peek() == &Tok::AndAnd {
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
        if self.peek() == &Tok::Not {
            let start = self.peek_span();
            self.advance();
            let expr = self.parse_not()?;
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
        if self.peek() == &Tok::Minus {
            let start = self.peek_span();
            self.advance();
            let expr = self.parse_unary()?;
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
        while self.peek() == &Tok::Dot {
            self.advance();
            let name = self.expect_ident("字段名")?;
            // 最多两级：alias.name 或 name；更多级报 PAR-001
            match expr {
                Expr::Field {
                    alias: None,
                    name: base,
                    span,
                } => {
                    let new_span = span.merge(self.toks[self.pos.saturating_sub(1)].span);
                    expr = Expr::Field {
                        alias: Some(base),
                        name,
                        span: new_span,
                    };
                }
                other => {
                    let span = other.span().merge(self.toks[self.pos.saturating_sub(1)].span);
                    self.diags.error(
                        Code::ParUnexpected,
                        "`.` 只支持 `别名.字段` 两级引用".to_string(),
                        span,
                    );
                    return None;
                }
            }
        }
        Some(expr)
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        let span = self.peek_span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.advance();
                // 编译期可确定的整数字面量越界（02-lexical：超出 i64 范围报错）
                if v > i64::MAX as i128 || v < i64::MIN as i128 {
                    self.diags.error(
                        Code::SemConstDomain,
                        format!("整数字面量 `{}` 超出 i64 范围", v),
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
                    self.diags.error(
                        Code::SemLiteralOutOfRange,
                        format!("浮点字面量超出 MVP 有限 f64 值域"),
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
            Tok::Ident(name) => {
                match name.as_str() {
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
                        // 函数调用：ident "(" ...
                        if self.peek() == &Tok::LParen {
                            self.advance();
                            let mut args = Vec::new();
                            if self.peek() != &Tok::RParen {
                                loop {
                                    args.push(self.parse_expr()?);
                                    if self.accept_punct(&Tok::Comma) {
                                        if self.peek() == &Tok::RParen {
                                            break;
                                        }
                                        continue;
                                    }
                                    break;
                                }
                            }
                            self.accept_punct(&Tok::Comma);
                            if !self.expect_punct(&Tok::RParen, "`)`") {
                                return None;
                            }
                            let end = self.toks[self.pos.saturating_sub(1)].span;
                            return Some(Expr::Call {
                                name,
                                args,
                                span: span.merge(end),
                            });
                        }
                        Some(Expr::Field {
                            alias: None,
                            name,
                            span,
                        })
                    }
                }
            }
            other => {
                let got = describe(&other);
                if other == Tok::LBrace {
                    // PAR-003：记录字面量只允许出现在 extend 参数位
                    self.err_here(
                        Code::ParExtendNotRecord,
                        "记录字面量 `{ ... }` 只允许作为 extend 的参数",
                    );
                } else {
                    self.err_here(Code::ParUnexpected, format!("期望表达式，遇到 {}", got));
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
            self.err_here(Code::ParIfMalformed, "if 表达式必须以 `else { ... }` 结束");
            return None;
        }
        let else_ = if self.accept_kw("if") {
            // `else if ...` → 嵌套 if（其 `if` 关键字已消费）
            Box::new(self.parse_if_body(start)?)
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

fn set_span(op: &mut Op, span: Span) {
    match op {
        Op::Filter { span: s, .. }
        | Op::Extend { span: s, .. }
        | Op::Select { span: s, .. }
        | Op::Distinct { span: s }
        | Op::Sort { span: s, .. }
        | Op::Take { span: s, .. }
        | Op::Join { span: s, .. }
        | Op::GroupBy { span: s, .. }
        | Op::Agg { span: s, .. } => *s = span,
    }
}

fn is_field_ref(expr: &Expr) -> bool {
    matches!(expr, Expr::Field { .. })
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Ident(s) => format!("标识符 `{}`", s),
        Tok::Int(v) => format!("整数 `{}`", v),
        Tok::Float(v) => format!("浮点数 `{}`", v),
        Tok::Str(_) => "字符串字面量".to_string(),
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
