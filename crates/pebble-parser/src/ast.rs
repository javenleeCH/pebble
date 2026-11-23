//! AST 节点：仅语法结构与 Span，不做类型/字段解析（docs/draft/design/ast.md）。

use pebble_diagnostic::Span;

pub type Ident = String;

#[derive(Clone, Debug)]
pub struct Program {
    pub pipeline: Pipeline,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub source: SourceOp,
    pub ops: Vec<Op>,
    /// 语义校验要求恰好一个终端；缺失时报 SEM-005。
    pub terminal: Option<TerminalOp>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum SourceOp {
    Table {
        path: String,
        alias: Option<Ident>,
        span: Span,
    },
    MemoryTable {
        name: String,
        alias: Option<Ident>,
        span: Span,
    },
}

impl SourceOp {
    pub fn span(&self) -> Span {
        match self {
            SourceOp::Table { span, .. } | SourceOp::MemoryTable { span, .. } => *span,
        }
    }

    pub fn alias(&self) -> Option<&str> {
        match self {
            SourceOp::Table { alias, .. } | SourceOp::MemoryTable { alias, .. } => alias.as_deref(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    Filter {
        pred: Expr,
        span: Span,
    },
    Extend {
        record: RecordLiteral,
        span: Span,
    },
    Select {
        items: Vec<SelectItem>,
        span: Span,
    },
    Distinct {
        span: Span,
    },
    Sort {
        keys: Vec<SortKey>,
        span: Span,
    },
    Take {
        n: i64,
        span: Span,
    },
    Join {
        right: SourceOp,
        on: Expr,
        span: Span,
    },
    GroupBy {
        keys: Vec<GroupKey>,
        span: Span,
    },
    Agg {
        bindings: Vec<AggBinding>,
        span: Span,
    },
}

impl Op {
    pub fn span(&self) -> Span {
        match self {
            Op::Filter { span, .. }
            | Op::Extend { span, .. }
            | Op::Select { span, .. }
            | Op::Distinct { span }
            | Op::Sort { span, .. }
            | Op::Take { span, .. }
            | Op::Join { span, .. }
            | Op::GroupBy { span, .. }
            | Op::Agg { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<Ident>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct GroupKey {
    pub expr: Expr,
    pub alias: Option<Ident>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct SortKey {
    pub expr: Expr,
    pub descending: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct AggBinding {
    pub name: Ident,
    pub call: AggCall,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct AggCall {
    pub func: AggFunc,
    pub arg: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggFunc {
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Avg => "avg",
        }
    }
}

#[derive(Clone, Debug)]
pub enum TerminalOp {
    Write { path: String, span: Span },
    Collect { span: Span },
    Print { span: Span },
}

impl TerminalOp {
    pub fn span(&self) -> Span {
        match self {
            TerminalOp::Write { span, .. }
            | TerminalOp::Collect { span }
            | TerminalOp::Print { span } => *span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordLiteral {
    pub bindings: Vec<Binding>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Literal(Literal, Span),
    /// `alias.name` 或裸 `name`；解析由语义层绑定到稳定 FieldId。
    Field {
        alias: Option<Ident>,
        name: Ident,
        span: Span,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
        span: Span,
    },
    /// 语法：`if <cond> { <then> } else { <else> }`；else-if 链已脱糖为嵌套 If。
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
        else_: Box<Expr>,
        span: Span,
    },
    /// 内置标量函数调用；聚合调用只出现在 AggBinding 中。
    Call {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal(_, span)
            | Expr::Field { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::If { span, .. }
            | Expr::Call { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Literal {
    /// 词法层保留 i128 以便语义层报 SEM-016/SEM-015；AST 收敛为 i64。
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    And,
    Or,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Negate,
    Not,
}
