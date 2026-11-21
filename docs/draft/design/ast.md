# AST 节点定义草案

> 对应 crate：`pebble-parser`。本草案为 Rust 2024 Edition 风格伪代码，字段命名以实现时为准；AST 只保留语法结构，不做类型解析。

## 1. 通用类型

```rust
/// 源码区间（字节偏移），由 Chumsky 提供，贯穿诊断与 HIR 映射。
pub struct Span {
    pub start: usize,
    pub end: usize,
}

pub type Ident = String;
```

## 2. 程序与流水线

```rust
pub struct Program {
    pub pipeline: Pipeline,
}

pub struct Pipeline {
    pub source: SourceOp,
    pub ops: Vec<Op>,
    pub terminal: Option<TerminalOp>,
    pub span: Span,
}
```

## 3. 源算子

```rust
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
```

## 4. 变换与聚合算子

```rust
pub enum Op {
    Filter { lambda: Lambda, span: Span },
    Map { lambda: Lambda, span: Span },
    Select { fields: Vec<Expr>, span: Span },
    Distinct { span: Span },
    Sort { keys: Vec<SortKey>, span: Span },
    Take { n: i64, span: Span },
    Join { right: SourceOp, on: Expr, span: Span },
    GroupBy { keys: Vec<Expr>, span: Span },
    Agg { bindings: Vec<AggBinding>, span: Span },
}

pub struct SortKey {
    pub expr: Expr,
    pub descending: bool,
    pub span: Span,
}

pub struct AggBinding {
    pub name: Ident,
    pub call: AggCall,
    pub span: Span,
}

pub struct AggCall {
    pub func: AggFunc,
    pub arg: Option<Expr>,
    pub span: Span,
}

pub enum AggFunc { Count, Sum, Min, Max, Avg }
```

## 5. 终端算子

```rust
pub enum TerminalOp {
    Write { path: String, span: Span },
    Collect { span: Span },
    Print { span: Span },
}
```

## 6. Lambda 与记录字面量

```rust
pub struct Lambda {
    pub param: Ident,
    pub body: LambdaBody,
    pub span: Span,
}

pub enum LambdaBody {
    Expr(Expr),
    Record(Vec<Binding>),
}

pub struct Binding {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}
```

## 7. 表达式

```rust
pub enum Expr {
    Literal(Literal, Span),
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
    If {
        branches: Vec<(Expr, Expr)>, // (条件, 分支值)
        else_: Box<Expr>,
        span: Span,
    },
    Call {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
}

pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Neq, Gt, Lt, Gte, Lte,
    And, Or,
}

pub enum UnaryOp { Negate, Not }
```

## 8. 设计约束（AST 层）

1. AST 中**不存在**语句节点、赋值节点、函数定义节点、循环节点——文法层面就不会产生。
2. `Expr::Call` 仅用于内置函数与聚合调用；聚合调用只允许出现在 `AggBinding` 右侧（语义层复核）。
3. 字段访问的 `alias` 为 `None` 时表示裸标识符，语义层按当前行字段 / 分组键 / 聚合绑定顺序解析。
4. AST 保留源码顺序与 Span；不做类型标注，类型信息在语义层附加到 HIR。
5. 解析错误恢复时，缺失节点以占位变体保留（实现期补充），保证后续诊断可继续产出。
