# AST 节点定义草案

> 对应 crate：`pebble-parser`。本草案为 Rust 2024 Edition 风格伪代码，字段命名以实现时为准；AST 只保留语法结构与源码位置，不做类型检查、字段解析或后端假设。

## 1. 通用类型

```rust
/// 源码区间（字节偏移），贯穿诊断与 HIR 映射。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

pub type Ident = String;
```

`Span`、注释、空白和别名拼写只服务诊断与源码映射，不进入 canonical HIR hash。

## 2. 程序与流水线

```rust
pub struct Program {
    pub pipeline: Pipeline,
}

pub struct Pipeline {
    pub source: SourceOp,
    pub ops: Vec<Op>,
    /// 解析恢复期可为空；完整程序的语义校验要求恰好一个终端。
    pub terminal: Option<TerminalOp>,
    pub span: Span,
}
```

AST 不包含语句、赋值、函数定义、循环或递归节点。`if` 只出现在表达式位置，不能选择流水线结构。

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

源别名只是字段解析期的限定符，不是输出字段名，也不是数据版本。`Table` 的 Schema 来自 Schema lock；AST 不读取文件元信息。

## 4. 变换与聚合算子

```rust
pub enum Op {
    Filter { pred: Expr, span: Span },
    Extend { record: RecordLiteral, span: Span },
    Select { items: Vec<SelectItem>, span: Span },
    Distinct { span: Span },
    Sort { keys: Vec<SortKey>, span: Span },
    Take { n: i64, span: Span },
    Join { right: SourceOp, on: Expr, span: Span },
    GroupBy { keys: Vec<GroupKey>, span: Span },
    Agg { bindings: Vec<AggBinding>, span: Span },
}

pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<Ident>,
    pub span: Span,
}

pub struct GroupKey {
    pub expr: Expr,
    pub alias: Option<Ident>,
    pub span: Span,
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

语法与位置约束：

1. `filter` 直接接受表达式，不再有 `|u|` Lambda 包装。
2. `extend` 唯一接受记录字面量；其所有 RHS 同时对输入行求值，兄弟绑定互不可见。
3. `selectItem = expr [as ident]`。无 `as` 时只允许字段引用；派生复合表达式必须命名。输出顺序与列表顺序一致。
4. `join(other, on = expr)` 中的 `on =` 是固定语法，不改变 `Join` 的 AST 结构；当前语法只允许右表为源算子。
5. 不接 `GroupBy` 的 `Agg` 是全局聚合；`GroupBy` 后必须且只能紧跟一个 `Agg`。
6. `count()` 使用 `arg = None`；其他聚合的参数要求由语义层检查。
7. `having` 在 v0.1.1 前不进入 AST；MVP 使用聚合后的普通 `filter`。
8. `sort/take` 是顺序敏感的表示层操作，后端不得把它们当普通无序关系算子自由重排。

## 5. 终端算子

```rust
pub enum TerminalOp {
    Write { path: String, span: Span },
    Collect { span: Span },
    Print { span: Span },
}
```

终端只能位于流水线末尾。当前 MVP 只硬化 `collect`，其他终端可保留语法但由功能开关或后端能力检查拒绝。

## 6. Extend 与记录字面量

```rust
pub struct RecordLiteral {
    pub bindings: Vec<Binding>,
    pub span: Span,
}

pub struct Binding {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}
```

记录字面量只能作为 `extend` 的直接参数，不能嵌套在普通表达式中，也不是一等值。`map` 不再存在；扩展算子的名字统一为 `extend`。

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
    /// 语法固定为：if <condition> { <then> } else { <else> }。
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
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

`if` 的条件位于 `{}` 之外；只有一个条件、一个 then 分支和一个 else 分支，不再使用旧的 `Vec<(condition, branch)>` 多分支结构。条件为 `true` 时选择 then，`false/null` 时选择 else。

`x == null` / `x != null` 在语义层识别为独立判空形式，不按普通相等/不等处理；普通比较遵循严格 null 传播。

## 8. 作用域与字段解析输入

AST 中的字段仍只是 `alias + name` 语法引用。lowering 必须依据当前可见 Schema 将其解析为稳定字段身份，不能把字符串名字当作跨节点身份：

- 源别名与输出字段名属于不同命名空间。
- 裸字段只有在当前可见 Schema 中名称唯一时才能解析，否则报歧义，禁止默认取左表。
- join 后重名字段仍以来源身份区分；进入不允许重名的输出边界前必须用 `select as` 消除重名。
- `extend` 覆盖后旧字段不可再按原名访问；`select` 后被裁字段不可再经源别名访问。
- `agg` 参数引用组内输入行，分组键是组级值；同一 `agg` 的输出绑定互不可见。
- `agg` 后只能引用分组键与聚合输出。全局 agg 中，聚合函数之外不得引用输入行字段。

## 9. AST 不变量

1. AST 保留源码顺序和 `Span`，不做类型标注或 Schema 推导。
2. `Expr::Call` 仅表示内置标量函数；聚合调用只由 `AggBinding` 中的 `AggCall` 表示。
3. 解析错误恢复丢弃无法完整解析的算子（也可使用显式错误占位节点）；含未恢复错误的程序以 `PartialAst` 交还，不得 lowering 或生成执行工件。解析入口返回 `Result<Program, PartialAst>`，语义层只接受 `Program`。
4. AST 不直接交给任何后端；所有 DuckDB、Polars、LaTeX 和未来原生后端统一消费 typed HIR。
