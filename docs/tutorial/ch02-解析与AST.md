# 第 2 章 递归下降解析器与 AST

> 对应文件：`crates/pebble-parser/src/{ast.rs, parser.rs}`；规范：`grammar/pebble.ebnf`（Draft v0.2）。

## 本章目标

1. 把 EBNF 逐条翻译成递归下降函数，学会「优先级阶梯」写法；
2. 设计一个**语法层守规矩**的 AST：错误的东西在文法层就造不出来；
3. 理解为什么 M0 不用解析器框架（Chumsky）——以及欠了什么债。

## 代码走读

### AST：让非法程序「无法表达」

AST 的第一设计原则不是「表达一切」，而是**把语言的红线编码成类型**（`docs/draft/design/ast.md`）：

- 没有语句节点、赋值节点、循环节点——文法层就不会产生；
- 没有 Lambda 节点——`|param|` 在词法层就是非法字符（LEX-001）；
- `If` 是**单条件**结构 `{condition, then, else_}`——`else if` 链在解析期脱糖为嵌套 If，HIR 层永远只见二元分支；
- 记录字面量 `RecordLiteral` 是独立类型而不是普通表达式——**它只能作为 `extend` 的参数存在**，想在 `filter` 里写 `{...}` 会被解析器直接拒绝（PAR-003）。

对比一下：如果把记录字面量做进 `Expr::Record`，那么「只许 extend 用」就得靠语义层报错——错误从「结构性不可能」退化成「运行时检查」。**能用类型表达的约束不要留给检查**。

## 优先级阶梯：一条产生式一个函数

表达式语法（04-expressions §4.1）自低到高：

```text
||  →  &&  →  !  →  比较  →  + -  →  * / %  →  一元 -  →  后缀 .字段  →  原子
```

每个优先级一个函数，**低优先级调用高优先级**：

```rust
fn parse_or(&mut self) -> Option<Expr> {
    let mut left = self.parse_and()?;
    while *self.peek() == Tok::OrOr {
        self.advance();
        let right = self.parse_and()?;
        left = Expr::Binary { op: BinOp::Or, left: Box::new(left), right: Box::new(right), ... };
    }
    Some(left)
}
```

同级左结合由 `while` 循环实现；`!` 和一元 `-` 右结合由递归实现（`parse_not` 自己调用自己）。**这个阶梯的顺序就是优先级表的顺序**——改规范里的优先级，就是改函数调用链。

容易踩的坑：`&&`/`||` 夹在中间，`!` 的函数夹在 `&&` 和比较之间。如果 `!` 放错层，`!a && b` 的结合就会错。防错手段：单测覆盖每个优先级组合（见 parser 测试）。

## 上下文关键字与固定绑定

`join(..., on = expr)` 是本章最有教学价值的片段：

```rust
let on_kw = self.expect_ident("`on`")?;
if on_kw != "on" { ... PAR-007 ... }
if !self.accept_punct(&Tok::Assign) { ... PAR-007 ... }
let on = self.parse_expr()?;
```

为什么不用命名参数（`on: expr` 或 `cond = expr`）？三个理由：

1. `on` 是**固定绑定名**而不是用户可自选的名字——文法上消除「`cond = a == b` 也合法」的歧义空间；
2. `=` 已是语言唯一的绑定符（agg 绑定、记录绑定），`:` 没有存在必要；
3. 绑定右侧的表达式里**不存在** `=` 产生式，所以 `on = u.id == o.uid` 的解析唯一——歧义在文法层就被排除，而不是靠语义判断。

第一版设计曾用 `on:`，EBNF 与样例不一致（一轮评审抓出的硬伤）。教训：**spec、EBNF、样例三处必须同一次 commit 内同步**。

## 尾逗号：格式化器的隐藏需求

`select`/`sort`/`group_by`/`agg`/`join`/记录字面量统一允许尾逗号：

```rust
if self.accept_punct(&Tok::Comma) { continue; }   // 吃掉逗号继续
// 下一轮循环若遇 RParen 则 break —— 尾逗号自然合法
```

为什么统一？多行列表场景下用户习惯在最后一个元素后留逗号；不一致的规则会让未来的 `pebble fmt` 无法机械化处理。**文法是格式化器的前置设施**——这类「看起来无所谓」的一致性决定，等 fmt 出现再改就迟了。

## 被否决的方案：为什么 M0 不用 Chumsky

| 维度 | 手写递归下降 | Chumsky 0.10+ |
|---|---|---|
| 错误码精确控制 | 每个错误点一行代码，码/span/hint 全可控 | 需要绕它的错误类型做映射层 |
| 文法规模 | 600 行直接对 EBNF | 组合子图需要框架知识才能审 |
| API 稳定性 | 自有代码 | 0.9→0.10 彻底重写 |
| 错误恢复（多错误） | 本章是首错即止；多错误恢复见第 8 章（同步点 + 括号栈） | 内建 `recover_with`——但策略照样要自己设计 |

决策依据：诊断契约是产品（第 3 章），文法又足够小。**对外契约已隔离**：`pebble_parser::parse(source, diags)` 是唯一入口（第 8 章起返回 `Result<Program, PartialAst>`），哪天换 Chumsky 只动一个 crate，全部测试保留。这不是永久的否决，是有意的排序。

## AST 不变量自查

解析器完成时必须满足（`docs/draft/design/ast.md` §9）：

1. 只保留语法结构与 Span，不做类型/字段解析；
2. `Expr::Call` 只代表内置标量函数；聚合调用只存在于 `AggBinding`；
3. 含未恢复错误的程序不得进入语义层（本章靠 `parse()` 返回 `None`；第 8 章起返回 `Err(PartialAst)`，由类型系统代为把关）。

## 跑起来

```powershell
cargo test -p pebble-parser
# 感受 PAR 错误的精确定位（PAR-007：on 后缺 =）
"Table(`"a.csv`") as a`n  .join(Table(`"b.csv`") as b, on: a.x == b.x)`n  .collect()" |
  Set-Content examples/pardemo.pebble
cargo run -p pebble-driver -- build examples/pardemo.pebble --locks tests/locks
Remove-Item examples/pardemo.pebble
```

第 11 章给文法加了两个产生式：算子 `having(pred)` 与表达式 `cast(expr as T)`。后者的参数形式 `expr as 类型` 与普通调用不同，需要单独的解析函数——它是「给文法加一个产生式」的完整范例，见[第 11 章](ch11-having与cast.md)。

## 练习

1. **把 `else if` 展开画出来**：手写 `if a {x} else if b {y} else {z}` 脱糖后的嵌套 If 树（画在纸上），对照 `parse_if_body` 的递归。
2. **禁止嵌套引用**：`a.b.c` 目前报 PAR-001。把报错改成更友好的 hint（提示最多两级 `别名.字段`），并补一个单测。
3. **挑战题（为第 8 章热身）**：让 `parse_expr` 在失败后能跳到下一个 `.` 或行首继续，使一次编译报出两个语法错误。想清楚：partial AST 怎么表示（占位节点？）、哪些 invariant 会破。（第 8 章给出了答案：同步点、括号栈与 `parse_list`。注意「跳到下一个 `.`」和「跳到行首」这两个朴素想法各自为什么不行——做完再对照。）
