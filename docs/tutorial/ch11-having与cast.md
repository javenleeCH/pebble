# 第 11 章 having 与受限 cast：给语言加一个特性的完整路径

> 对应文件：`crates/pebble-parser/src/{ast,parser}.rs`、`crates/pebble-hir/src/{lib,canonical}.rs`、`crates/pebble-semantics/src/lib.rs`、`crates/pebble-duckdb/src/lib.rs`、`crates/pebble-latex/src/lib.rs`。
> 规范：04 §4.12、05 §5.2–5.4、06 §6.4、07 §7.8、08。
> 测试：`pebble-semantics` 单测、`pebble-driver/tests/differential.rs`（在真实 DuckDB 上与参考实现对拍）。

## 本章目标

1. 完整走一遍「给语言加一个特性」的路：规范 → 文法 → AST → HIR 与哈希 → 语义 → 后端 → 审计视图 → 诊断 → 测试 → 教程；
2. 分清**语法糖**和**新语义**：`having` 是前者，`cast` 是后者，两者的证据标准不同；
3. 学会为一条取值规则先写**参考实现**，再拿它和引擎对拍；
4. 认识「引擎接受得比规范更宽」这类语义差集，以及怎样用守卫把它拦住。

## 为什么：两个来自使用的需求

**第一个需求：字符串里的数字。** CSV 里的数字经常是以字符串读进来的。在没有 `cast` 的 Pebble 里，这样的程序编译不过：

```text
error[SEM-003]: 运算符 `+` 不支持 string 与 i64
  ┌─ tut-no_cast.pebble:2:17
  │
2 │   .extend({ n = c.s + 1 })
  │                 ^^^^^^^ SEM-003
  │
编译中止：共 1 个错误（SEM 1）
```

这不是 bug：规范禁止字符串与数值之间的隐式转换。但没有显式转换的话，这份数据就没法用了。

**第二个需求：对分组结果过滤。** 写过 SQL 的人都会想这样写：

```pebble
  .group_by(o.customer_id)
  .agg(n = count(), total = sum(o.amount))
  .having(total > 1000)
```

有意思的是，Pebble 其实不需要 `having`：它的 `filter` 本来就能作用于聚合的输出（`.agg(…).filter(total > 1000)` 早就合法）。SQL 需要 `HAVING`，是因为 SQL 的 `WHERE` 看不见聚合值；Pebble 的算子是一级一级流下去的，没有这个限制。所以 `having` 是**语法糖**：它让意图在源码里更醒目，但不引入任何新的语义。

两个需求，两种性质，刚好凑成这一章：`having` 教你怎样负责任地加一点甜味，`cast` 教你怎样加一块真正的骨头。

## 代码走读

### 1. 第一步不是写代码，是改规范

规范先行在这个项目里不是口号。动手之前先回答四个问题，答案写进规范，代码只是实现它：

| 问题 | 决定 | 写在 |
|---|---|---|
| `cast` 长什么样 | `cast(expr as T)`；`T` 只有 `i64`、`f64`、`string` | 04 §4.12 |
| 允许哪些转换 | 只有四种：`i64→f64`、`f64→i64`、`string→i64`、`string→f64` | 04 §4.12 |
| `f64→i64` 怎么取整 | round-to-nearest, ties-to-even；越界得 null | 07 §7.8 |
| 字符串怎样才算数字 | 严格文法：整数 `[+-]?[0-9]+`；小数与科学记数法另有文法；`inf`、空白、下划线一律不合法 | 07 §7.8 |

三个决定值得多说一句。

**为什么只有四种？** 数值转字符串被排除了：f64 的输出格式要定得与引擎无关很麻烦（`1e20` 打印成什么？`0.1` 呢？），值得单独一章。`bool` 不参与任何转换。相同类型之间的 `cast` 也报错——它什么都没做，只会让读的人以为发生了什么。

**为什么取偶而不是截断？** 因为语言里已经有一处把 f64 变成整数的地方——`round`——它用的是 ties-to-even。两处规则一致，用户只需要记一个。

**为什么要自己定字符串文法？** 因为「字符串像不像数字」每个引擎的回答都不一样。实测 DuckDB 1.4.1 会把 `" 1"`、`"1 "`、`"1_000"` 都读成 1000，把 `"0x10"` 读成 16。规范如果只说「解析成数字」，这些行为就都成了语言的一部分。

`having` 的规范改动要小得多：06 §6.4 加一节，写明「语义与紧随其后的 `filter` 完全相同」「必须紧跟在 `agg` 之后」。

### 2. having：一个语法糖的三种证据

说「`having` 就是 `filter`」很容易，难的是让这句话可检验。这里有三层证据。

**证据一：HIR 完全相同。** 语义层把 `having` 交给和 `filter` 同一个函数：

```rust
    /// filter 的 lowering：解析谓词、检查类型、发射节点；输出 Schema 是非空收缩之后的。
    /// 谓词解析不出来或不是 bool 时返回 `None`（错误已报告，或是毒化）：算子跳过，Schema 不变。
    /// `having` 走的是同一个函数——它在语义上就是紧跟 agg 的 filter。
    fn lower_filter(
        &mut self,
        current_id: NodeId,
        current_schema: &Schema,
        pred: &ast::Expr,
        span: Span,
    ) -> Option<(NodeId, Schema)> {
        self.history.push(current_schema.clone());
        let resolved = self.resolve_expr(pred, current_schema, false)?;
        self.check_predicate(&resolved)?;
        self.const_domain_check(&resolved);
        let narrowed = self.narrow(current_schema, &resolved);
        let node_id = self.push_node(vec![current_id], narrowed, span, |base| {
            HirNode::Transform {
                base,
                kind: TransformKind::Filter { pred: resolved },
            }
        });
        let schema = self.nodes[node_id as usize].base().output.clone();
        Some((node_id, schema))
    }
```

（第 9 章引用过这个函数：两处 `?` 表示「已报告或已毒化，跳过这个算子」。）

既然 HIR 相同，HIR 哈希就相同。测试 `having_is_sugar_for_filter_after_agg` 断言的正是这件事：

```text
.having(total > 100 && n >= 2)   hir_hash = sha256:1c6733bb…
.filter(total > 100 && n >= 2)   hir_hash = sha256:1c6733bb…
```

这是「语义等价」的机器证据——不是我说它们一样，是哈希说它们一样。

**证据二：位置规则是语法糖唯一多出来的东西。**

```rust
                ast::Op::Having { pred, span } => {
                    // having 是语法糖（06 §6.4）：必须紧跟在 agg 之后，然后按与 filter 完全
                    // 相同的方式 lowering。位置不对时报告并跳过这个算子，后面的检查照常进行。
                    if !matches!(ops.get(i.wrapping_sub(1)), Some(ast::Op::Agg { .. })) {
                        self.diags
                            .error(Code::SemOpPosition, "having 必须紧跟在 agg 之后", *span);
                        i += 1;
                        continue;
                    }
                    if let Some((node_id, schema)) =
                        self.lower_filter(current_id, &current_schema, pred, *span)
                    {
                        current_schema = schema;
                        current_id = node_id;
                    }
                    i += 1;
                }
```

`i.wrapping_sub(1)` 在 `i == 0` 时回绕成 `usize::MAX`，`ops.get` 得到 `None`——流水线开头的 `having` 自然不满足「前一个算子是 `agg`」，不需要单独判空。位置不对时报 `SEM-005` 并跳过这个算子，第 9 章的多错误机制照常工作。

**证据三：行为逐项相同。** 谓词不是 bool 报 `SEM-010`；看得见的东西只有聚合的输出（引用输入行字段报 `SEM-012`）；非空收缩规则也一样——`having(amount != null)` 之后，分组键 `amount` 从 `?i64` 收缩成 `i64`。端到端测试再把 `having` 和 `filter` 两个程序在真实 DuckDB 上的输出逐行比较，并与直接从 CSV 数出来的结果比较。

### 3. HAVING：聚合屏障唯一的例外

`having` 到了 HIR 是一个紧跟聚合的 `filter`。第 10 章说过聚合是屏障，它后面的 `filter` 不能并进 `WHERE`——那样就成了分组之前的过滤。但 SQL 本来就为「对分组结果过滤」准备了子句：`HAVING`。

**论证**：对外层关系（每个分组一行）做 `WHERE`，与对分组做 `HAVING`，保留的是同一批分组。所以这个 `filter` 可以并进聚合层，放在 `HAVING` 里。

```rust
            TransformKind::Filter { pred } => {
                // 聚合层是屏障的唯一例外：紧跟其后的 filter 是「对分组结果过滤」，并进它的 HAVING。
                // 论证：对外层关系（每个分组一行）做 WHERE，与对分组做 HAVING，保留的是同一批分组
                let mut into_having = false;
                if let Some(b) = open.as_mut() {
                    if b.group_by.is_some() {
                        let env = EmitEnv {
                            subst: b.subst.clone(),
                        };
                        b.having.push(self.emit_expr(pred, &env));
                        into_having = true;
                    }
                }
                if into_having {
                    return Ok(());
                }
```

这里有一个第 10 章没有的新问题。谓词里写的是 `total > 1000`，`total` 是聚合的一个输出；换成 SQL，最顺手的写法是 `HAVING "f7" > 1000`，引用选择列表里的别名 `COUNT(*) AS "f7"`。可是**标准 SQL 的 `HAVING` 看不到选择列表的别名**——和 `WHERE` 一样（DuckDB 碰巧允许，那是方言扩展，第 10 章已经决定不依赖它）。标准的写法是把聚合表达式原样再写一遍：`HAVING (COUNT(*)) > (1000)`。

所以发射 `HAVING` 时需要一张替换表：输出字段 id → 它的表达式。

```rust
/// 发射表达式时的环境。
///
/// 绝大多数场合它是空的。唯一的例外是 `HAVING`：标准 SQL 的 HAVING 不能引用同一层选择列表的
/// 别名（比如 `COUNT(*) AS "f6"` 里的 `"f6"`），所以要把聚合输出的字段引用换成它的表达式。
/// `subst` 就是这张「输出字段 id → 表达式 SQL」的替换表。
struct EmitEnv {
    subst: BTreeMap<pebble_hir::FieldId, String>,
}

/// 没有替换的环境：WHERE、选择列表、ORDER BY 等场合都用它。
const NO_SUBST: EmitEnv = EmitEnv {
    subst: BTreeMap::new(),
};
```

表在生成聚合层的时候顺手记下来，每个输出字段对应它在选择列表里的那段 SQL：

```rust
        let mut subst = BTreeMap::new();
        for k in group_keys {
            let key_sql = self.emit_expr(&k.value, &NO_SUBST);
            select.push(format!("{} AS \"f{}\"", key_sql, k.output));
            group_by.push(key_sql.clone());
            subst.insert(k.output, key_sql);
        }
        for b in bindings {
            let call_sql = self.emit_agg_call(&b.call, &NO_SUBST);
            select.push(format!("{} AS \"f{}\"", call_sql, b.output));
            subst.insert(b.output, call_sql);
        }
```

发射表达式时，字段引用先查这张表：

```rust
            THirExpr::Field { field, .. } => match env.subst.get(field) {
                // HAVING 里引用聚合的输出：换成它的表达式。标准 SQL 的 HAVING 看不到选择列表的别名
                Some(sql) => sql.clone(),
                None => format!("\"f{}\"", field),
            },
            THirExpr::Cast { expr, to, .. } => self.emit_cast(expr, to, env),
```

代价是给发射函数族加了一个参数——四十多个调用点各补一个 `env` 或 `&NO_SUBST`。这种改动我用了一段十几行的小脚本按括号匹配自动完成，再手工改签名和 `Field` 那一臂；脚本第一次运行时我给错了结束标记，它按设计拒绝了，没有碰文件。

全局聚合（没有 `GROUP BY`）同样适用：`HAVING` 不成立时，那唯一的一行被过滤掉，结果零行——与 `filter` 作用于全局聚合的输出一致。

### 4. cast 的文法：三个类型名，在语法层就限死

`cast` 是一个表达式。它的参数形式 `expr as 类型` 和普通函数调用不同，所以单独解析：

```rust
    /// `cast(expr as T)`（04 §4.12）。与 `parse_if` 相同的约定：调用前 `cast` 关键字
    /// 已由 parse_primary 分派但**未消费**，这里先消费它。
    ///
    /// 目标类型只有三个，在语法层就限死：写成别的名字不是「类型不匹配」的语义问题，
    /// 而是这句话根本不是合法的 cast。后面没跟 `(` 的 `cast` 是一个普通的字段引用。
    fn parse_cast(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        self.advance(); // 消费 "cast"
        if *self.peek() != Tok::LParen {
            return Some(Expr::Field {
                alias: None,
                name: "cast".to_string(),
                span: start,
            });
        }
        self.advance(); // (
        let inner = self.parse_expr()?;
        if !self.accept_kw("as") {
            if !self.eat_hole() {
                let got = describe(self.peek());
                self.fail_hint(
                    Code::ParUnexpected,
                    format!("cast 的 `as` 之前应当是一个写完整的表达式，遇到 {got}"),
                    "写法：`cast(表达式 as 类型)`，类型只能是 i64 / f64 / string",
                );
            }
            return None;
        }
```

两个设计点：

- **类型名在语法层就限死。** `cast(x as bool)` 报的是 `PAR-001`，不是语义错误。理由是：它不是一个「类型不匹配」的程序，而是一个根本不符合文法的 `cast`。分清这两类错误，提示才能写得准：「类型只能是 i64 / f64 / string」。
- **`cast` 不是保留字。** 没跟括号的 `cast` 仍然是一个普通的字段名。加一个特性不该悄悄弄坏已有的程序——哪怕只是一个恰好叫 `cast` 的字段。

类型名在 AST 里是一个只有三个取值的枚举，不携带可空性——`cast(x as ?i64)` 不合法，可空性由语义层推导。

### 5. HIR：新表达式与哈希

新表达式是 `THirExpr::Cast { expr, to, ty, span }`：`to` 是目标**基础**类型，`ty` 是结果类型（含可空性）。它进入 canonical 序列化：

```rust
        // v0.1.1 新增。`V` 取「type conVersion」之意，是这个文件里此前从未用过的标签
        // （节点层的 `K` 已经表示 Take；让每个标签在全文件唯一，将来写解码器不用区分位置）。
        // 既有变体的编码一个字节都没变，所以不含 cast 的程序哈希不变（hir.md §6 第 8 条）
        crate::THirExpr::Cast { expr, to, ty, .. } => {
            w.tag(b"V");
            encode_type(w, to);
            encode_expr(w, expr);
            encode_type(w, ty);
        }
```

给语言加特性时，哈希是必须想清楚的：**旧程序的哈希不能变**。否则昨天审批过的计划，今天什么都没改却对不上了。这里靠的是「只加不改」：新变体用一个新标签，既有变体的编码一个字节都不动。两条测试钉住它：

- 单元测试把同一个程序的谓词换成 `cast`，断言编码里出现 `V`；再把 `cast` 去掉，断言整份编码里没有 `V`——这个标签没有别的构造在用；
- 端到端测试 `existing_programs_keep_their_hir_hash` 把样例 00 的哈希写死：`sha256:4a895435…`。它变了，要么语义被有意地改了（那就要更新常量并重新审批），要么是新特性意外地动了既有程序（那是 bug）。

`V` 这个标签我一开始选的是 `K`，读到节点层的 `K` 已经表示 `Take` 才改掉。两处处在文法的不同位置，解码不会混淆；但「每个标签全文件唯一」是一条更省心的纪律。

### 6. 类型：checked-null 决定可空性

```rust
                let from = inner.ty().base().clone();
                // 只有四种转换（04 §4.12）。可能失败的转换，结果一律可空（checked-null）
                let may_fail = match (&from, &to) {
                    (Type::I64, Type::F64) => false,
                    (Type::F64, Type::I64) | (Type::Str, Type::I64) | (Type::Str, Type::F64) => {
                        true
                    }
                    _ => {
                        self.diags.error_hint(
                            Code::SemOpUnsupported,
                            format!("不支持从 {} 转换到 {}", from.render(), to.render()),
                            *span,
                            "只支持四种转换：i64 → f64、f64 → i64、string → i64、string → f64",
                        );
                        return None;
                    }
                };
                let ty = if may_fail || inner.ty().is_opt() {
                    Type::Opt(Box::new(to.clone()))
                } else {
                    to.clone()
                };
```

（节选自 `resolve_expr` 的 `Cast` 分支。）规则是第 4 章那条 checked-null 的直接应用：**可能失败的运算，结果一律可空**，哪怕操作数本身非空。

| 表达式 | 结果类型 | 原因 |
|---|---|---|
| `cast(1 as f64)` | `f64` | 永不失败 |
| `cast(o.amount as f64)`，`amount: ?i64` | `?f64` | null 透传 |
| `cast(1.5 as i64)` | `?i64` | 取整可能越界 |
| `cast("1" as i64)` | `?i64` | 解析可能失败 |

`cast(null as i64)` 报的是 `SEM-008`（独立 `null` 无法确定基础类型），不是别的：操作数的类型没法从目标类型推出来，`cast` 不给 `null` 提供上下文。

### 7. 取值：先写参考实现

规范说「round-to-nearest, ties-to-even；越界得 null」。这句话要被实现两遍——一遍在语义层（给常量诊断用），一遍在 SQL 里（给引擎执行）。与其让两份实现各自对照规范，不如让其中一份成为**参考实现**，另一份与它对拍：

```rust
    /// `f64 → i64`：先按 round-to-nearest, ties-to-even 取整，再看是否落在 i64 值域内。
    pub fn f64_to_i64(x: f64) -> Option<i64> {
        if !x.is_finite() {
            return None;
        }
        let r = x.round_ties_even();
        // 2^63 在 f64 里恰好可表示；落在 [-2^63, 2^63) 里即在 i64 值域内
        if (-9223372036854775808.0..9223372036854775808.0).contains(&r) {
            Some(r as i64)
        } else {
            None
        }
    }
```

十几行，一眼能对照规范。字符串的文法则写成一个手递的小扫描器：

```rust
    /// `string → f64`：只接受 ASCII 十进制或科学记数法，结果必须有限。
    /// 文法：`[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?`
    pub fn parse_f64_strict(s: &str) -> Option<f64> {
        let bytes = s.as_bytes();
        let mut i = 0;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let digits = |i: &mut usize| {
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            *i > start
        };
        let int_digits = digits(&mut i);
        let mut frac_digits = false;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            frac_digits = digits(&mut i);
        }
        // 小数点前后至少一边有数字
        if !int_digits && !frac_digits {
            return None;
        }
        if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
            i += 1;
            if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                i += 1;
            }
            if !digits(&mut i) {
                return None;
            }
        }
        if i != bytes.len() {
            return None;
        }
        let v = s.parse::<f64>().ok()?;
        if v.is_finite() {
            Some(norm_zero(v))
        } else {
            None
        }
    }
```

文法检查通过之后，真正的数值交给标准库的解析（它做的是正确的舍入）。注意最后一步：解析成功但结果是无穷（`"1e400"`）也算失败；`-0` 规范化成 `+0.0`。

这四个函数放在公开的 `pebble_semantics::cast` 模块里：常量诊断用它们判断 `SEM-016`，端到端测试用它们当「第三种算法」。

### 8. SQL：先把引擎收敛到规范，再取值

在 DuckDB 上直接试一遍，就能看到引擎的行为和规范差在哪里：

| 规范 | DuckDB 1.4.1 默认 | 差在哪 |
|---|---|---|
| `" 1"` 不是整数 | `CAST(' 1' AS BIGINT)` = 1 | 接受空白 |
| `"1_000"`、`"0x10"` 不是整数 | 读成 1000、16 | 接受下划线、进制前缀 |
| `"inf"` 不是数 | `CAST('inf' AS DOUBLE)` = inf | 接受无穷 |
| `f64 → i64` 取偶 | `CAST(2.5 AS BIGINT)` = 3 | 四舍五入进位 |
| 越界得 null | `CAST(1e300 AS BIGINT)` 报错 | 抛异常 |

所以每一条转换都是「**先守卫，再取值**」：

```rust
    /// 受限 cast（07 §7.8）。每条规则都是「先把引擎的行为收敛到规范，再取值」：
    /// 引擎接受得更宽的写法（空白、下划线、`inf`）或别的取整习惯，都被守卫拦在外面。
    /// 取值的参考实现是语义层的 `f64_to_i64` / `parse_i64_strict` / `parse_f64_strict`，
    /// 测试拿它们和这里生成的 SQL 在真实 DuckDB 上对拍。
    fn emit_cast(&self, expr: &THirExpr, to: &Type, env: &EmitEnv) -> String {
        let x = self.emit_expr(expr, env);
        match (expr.ty().base(), to) {
            // 永不失败；null 经 CAST 仍是 null
            (Type::I64, Type::F64) => format!("CAST(({x}) AS DOUBLE)"),
            (Type::F64, Type::I64) => {
                // 先查值域，再取整。值域内的 x 取整后必然可表示：取整最多把 x 推高 0.5，
                // 而 2^63 附近能表示的 f64 都是整数、彼此至少相差 1024。
                // 取整公式里的 CAST(FLOOR(x) AS BIGINT) 只在小数部分恰为 0.5 时求值，那时 |x| < 2^53
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (isfinite(({x}))) THEN NULL \
                     WHEN ({x}) >= 9223372036854775808.0 OR ({x}) < -9223372036854775808.0 THEN NULL \
                     ELSE CAST({} AS BIGINT) END",
                    rne_integer(&x)
                )
            }
            (Type::Str, Type::I64) => {
                // 正则先把写法限死；越界的数字串 TRY_CAST 成 HUGEINT 得 NULL。
                // 最后一步也用 TRY_CAST：万一前两道都没拦住，结果是 NULL 而不是引擎报错
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (regexp_matches(({x}), '^[+-]?[0-9]+$')) THEN NULL \
                     WHEN NOT (TRY_CAST(({x}) AS HUGEINT) BETWEEN {I64_MIN} AND {I64_MAX}) THEN NULL \
                     ELSE TRY_CAST(({x}) AS BIGINT) END"
                )
            }
            (Type::Str, Type::F64) => {
                // 规范文法：[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?
                let grammar = r"^[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?$";
                let parsed = format!("TRY_CAST(({x}) AS DOUBLE)");
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (regexp_matches(({x}), '{grammar}')) THEN NULL \
                     ELSE {} END",
                    // 非有限（如 "1e400"）→ NULL；-0.0 → +0.0
                    norm_f64(&parsed)
                )
            }
            // 语义层只放行四种转换；走到这里说明编译器自己出了错
            _ => "NULL /* 内部错误：不支持的 cast */".to_string(),
        }
    }
```

每条规则里有三样东西值得留意：

- **`TRY_CAST`，不用 `CAST`。** 引擎的 `CAST` 失败时会抛异常；`TRY_CAST` 失败得 `NULL`。守卫已经把不合法的输入拦在前面了，但最后一步仍然用 `TRY_CAST`——万一守卫有漏洞，结果是 null，而不是让用户看到一个引擎错误（第 7 章：执行失败是执行状态，不是字段值）。
- **`f64 → i64` 的守卫顺序。** 先排除非有限和越界，再取整。这样取整公式里那句 `CAST(FLOOR(x) AS BIGINT)` 永远不会遇到放不下的数——和第 6 章「每个守卫都要自证安全性」是同一条纪律。
- **取整公式是 `round` 的翻版。** `rne_integer` 与 `emit_call(Round)` 用的是同一个「`(x) - FLOOR(x)` 落在 `[0, 1)`」的写法，所以正负数的取偶结果一致。

### 9. 常量诊断，以及顺手修掉的两个旧 bug

常量里的 `cast` 要能诊断：`cast("abc" as i64)` 是编译期就能确定的失败，报 `SEM-016`；`cast(2.5 as i64)` 不能报。做法就是让常量求值调用参考实现。

读这段代码时，我发现常量求值里**内置函数调用**那一臂写的是 `Err(*span)`：

```rust
        // 内置函数按规范（04 §4.8）在常量上求值。这里曾经是 `Err(*span)`——把「我不求值」
        // 当成了「必然失败」，于是 `round(1.5)`、`lower("ABC")` 这类合法的常量表达式全被误报成 SEM-016
```

`Err` 在这个函数里的含义是「这个常量**必然**定义域失败」。而原来的写法把「我没有实现求值」也当成了 `Err`。后果是：`.extend({ x = round(1.5) })`、`abs(-3)`、`lower("ABC")`——全是合法程序——一律编译失败。换成字段（`round(o.score)`）就没事，所以一直没人撞上。现在内置函数在常量上真正求值，`abs(i64::MIN)` 这类真正的失败照常报。

第二个是测试写出来才露出来的：`1.5 / 0` 不报 `SEM-016`。常量求值只认「f64 除以 `0.0`」和「i64 除以 `0`」；`1.5 / 0` 里的 `0` 是整数字面量，两种都不匹配。规范说混合数值运算先提升到 f64——那就先提升：

```rust
/// 混合数值运算的提升规则：一侧是 f64、另一侧是 i64 时，把 i64 按 round-to-nearest, ties-to-even
/// 转成 f64（与 `cast::i64_to_f64` 同一条规则）。
fn promote_mixed(l: &ConstVal, r: &ConstVal) -> (ConstVal, ConstVal) {
    match (l, r) {
        (ConstVal::Float(_), ConstVal::Int(b)) => {
            (l.clone(), ConstVal::Float(cast::i64_to_f64(*b)))
        }
        (ConstVal::Int(a), ConstVal::Float(_)) => {
            (ConstVal::Float(cast::i64_to_f64(*a)), r.clone())
        }
        _ => (l.clone(), r.clone()),
    }
}
```

提升用的正是 `cast` 的参考实现——`i64 → f64` 在语言里只有一条规则。

### 10. 对拍时挖出来的第三个：trim

`cast` 的对拍跑通之后，我把同一套办法用到了已有的内置函数上——第 6 章写过「DuckDB 的每个默认行为都要审」，但 `trim`、`lower` 这些从来没有在真实引擎上用刁钻输入验证过。

第一个就不一样。规范说 `trim` 去除两端 Unicode White_Space，制表符是 White_Space；DuckDB 的 `trim(x)` 却**不去掉制表符**。修法和 `cast` 是同一个思路——把规范要求的字符集显式交给引擎：

```text
trim((x), chr(9) || chr(10) || chr(11) || chr(12) || chr(13) || chr(32) || … || chr(12288))
```

全部 25 个 White_Space 码点。形似但不是 White_Space 的零宽空格（U+200B）不在这个集合里，所以会被保留——与参考实现一致。突变验证：把它改回默认的 `trim(x)`，字符串对拍立刻在制表符那一行变红。

### 11. 已知的语义差集：lower / upper（没有修）

同一个对拍还发现了三处 `lower` / `upper` 与规范不一致，我**没有修**，如实记在这里：

| 表达式 | 规范（Unicode 默认全映射） | DuckDB 1.4.1 |
|---|---|---|
| `lower("İ")`（U+0130） | `i` + U+0307（2 个码点） | `i`（1 个码点） |
| `upper("ß")` | `SS` | `ẞ` |
| `lower("ΣΑΣ")` | `σας`（词尾 σ → ς） | `σασ` |

DuckDB 实现的是 Unicode 的**简单**大小写映射（一对一），规范写的是**全**映射（允许一对多，以及词尾 sigma 这样的上下文规则）。要消除这个差集有两条路：要么在 SQL 里为全部一百多个特殊码点写守卫，要么把规范改成简单映射。这是一个语言设计的决定，不该由实现者顺手做掉。对拍测试里这三行是写明了的特例：它钉住现状，不假装现状是对的；差集消掉的那一天，这三行就该删掉。

## 怎么证明没改坏

| 测试 | 证明了什么 |
|---|---|
| 解析器单测（5 项） | `having` 是独立算子；`cast` 的三种目标；八种写错的 `cast` 都是 `PAR-001`；没跟括号的 `cast` 仍是字段；坏的 `cast` 只毁掉自己那个算子 |
| 语义层单测（9 项） | 参考实现的取值表；四种之外的转换都是 `SEM-003` 且带提示；常量 `cast` 只在必然失败时报；常量内置调用不再误报；`having` 与 `filter` 哈希相同；位置、谓词、作用域、非空收缩逐项与 `filter` 一致 |
| HIR、LaTeX 单测 | `cast` 以 `V` 编码且该标签独一无二；审计视图里是 `\mathrm{cast}_{i64}` |
| DuckDB 单测（4 项） | `HAVING` 的位置与子句顺序；四种 `cast` 的守卫结构；`trim` 的 25 个码点 |
| **对拍**（`differential.rs`，5 项） | 真实 DuckDB 上：`cast` 矩阵 25 行 × 7 列逐格等于参考实现；字符串函数逐格等于规范（三处已知差集除外）；`round`/`abs` 等于 Rust；`having` 等于 `filter` 并等于从 CSV 直接数出来的；样例 00 的哈希不变 |

突变验证做过三条：去掉 `string → i64` 的文法守卫（DuckDB 把 `" 1"` 读成 1）；`trim` 退回默认（制表符没被去掉）；二者都被对拍抓住，报错信息直接指出是哪一行哪一列。

对拍的盲点也要说清楚：数据是我挑的，不是随机生成的。它能抓住我想到过的边界，抓不住我没想到的。

## 跑起来

```powershell
cargo test -p pebble-semantics                                   # 类型、错误码、常量诊断、having 的等价性
cargo test -p pebble-driver --test differential -- --nocapture   # 在真实 DuckDB 上与参考实现对拍

# having：并进聚合层的 HAVING
cargo run -p pebble-driver -- build tests/fixtures/codegen/having_demo.pebble --locks tests/locks --out target/having_demo
```

having 程序生成的 plan.sql（只调整了换行和缩进）：

> `SET memory_limit = '512MB';` 前言是第 13 章加的。

```sql
-- pebble-duckdb artifact v2 (canonical)
SET threads TO 1;
SET memory_limit = '512MB';
SELECT "f5" AS "customer_id", "f6" AS "n", "f7" AS "total" FROM (
  SELECT "f5",
       "f6",
       "f7" FROM (
    SELECT "f4" AS "f5",
         COUNT(*) AS "f6",
         CASE WHEN (SUM(CAST(("f2") AS HUGEINT))) IS NULL THEN NULL WHEN (SUM(CAST(("f2") AS HUGEINT))) BETWEEN -9223372036854775808 AND 9223372036854775807 THEN CAST((SUM(CAST(("f2") AS HUGEINT))) AS BIGINT) ELSE NULL END AS "f7" FROM (
      SELECT "t0"."order_id" AS "f0",
           "t0"."status" AS "f1",
           "t0"."amount" AS "f2",
           "t0"."score" AS "f3",
           "t0"."customer_id" AS "f4"
       FROM read_csv('tests/data/orders.csv', header = true, columns = {'order_id': 'BIGINT', 'status': 'VARCHAR', 'amount': 'BIGINT', 'score': 'DOUBLE', 'customer_id': 'VARCHAR'}) AS "t0"
    ) AS "s1"
     GROUP BY "f4"
     HAVING (((((CASE WHEN (SUM(CAST(("f2") AS HUGEINT))) IS NULL THEN NULL WHEN (SUM(CAST(("f2") AS HUGEINT))) BETWEEN -9223372036854775808 AND 9223372036854775807 THEN CAST((SUM(CAST(("f2") AS HUGEINT))) AS BIGINT) ELSE NULL END) > (1000))) AND (((COUNT(*)) >= (2)))))
  ) AS "s3"
   ORDER BY "f5" ASC NULLS LAST
) AS "final";
```

从里往外读：最里层是把表的列重命名成 `f<id>` 的源；中间那一层同时有 `GROUP BY` 和 `HAVING`——`filter` 没有另开一层。`HAVING` 里写的 `total > 1000 && n >= 2`，发射时把 `total` 换成了 `sum` 的表达式（带着它的 checked 守卫），把 `n` 换成了 `COUNT(*)`。这就是「标准 SQL 的 `HAVING` 看不到选择列表别名」的代价：表达式要再写一遍。

```powershell
# cast：f64 → i64 的守卫
cargo run -p pebble-driver -- build tests/fixtures/codegen/cast_demo.pebble --locks tests/locks --out target/cast_demo
```

cast 程序生成的 plan.sql（只调整了换行和缩进）：

> `SET memory_limit = '512MB';` 前言是第 13 章加的。

```sql
-- pebble-duckdb artifact v2 (canonical)
SET threads TO 1;
SET memory_limit = '512MB';
SELECT "f0" AS "s", "f1" AS "x", "f3" AS "whole" FROM (
  SELECT "f0",
       "f1",
       "f3" FROM (
    SELECT "f0",
         "f1",
         "f2",
         "f3" FROM (
      SELECT "f0",
           "f1",
           "f2",
           CASE WHEN ("f1") IS NULL THEN NULL WHEN NOT (isfinite(("f1"))) THEN NULL WHEN ("f1") >= 9223372036854775808.0 OR ("f1") < -9223372036854775808.0 THEN NULL ELSE CAST(CASE WHEN (("f1") - FLOOR(("f1"))) < 0.5 THEN FLOOR(("f1")) WHEN (("f1") - FLOOR(("f1"))) > 0.5 THEN (FLOOR(("f1")) + 1.0) WHEN MOD(CAST(FLOOR(("f1")) AS BIGINT), 2) = 0 THEN FLOOR(("f1")) ELSE (FLOOR(("f1")) + 1.0) END AS BIGINT) END AS "f3" FROM (
        SELECT "t0"."s" AS "f0",
             "t0"."x" AS "f1",
             "t0"."n" AS "f2"
         FROM read_csv('tests/data/casts.csv', header = true, columns = {'s': 'VARCHAR', 'x': 'DOUBLE', 'n': 'BIGINT'}) AS "t0"
      ) AS "s1"
    ) AS "s2"
     LIMIT 2
  ) AS "s3"
) AS "final";
```

最里层选择列表的最后一项就是新列 `whole`。四个 `WHEN` 依次是：null 透传、非有限、越界、取偶取整。最后那个取整公式和第 6 章 `round` 的一模一样。这个演示文件只有两层有意义的变换，却占了三层嵌套：`extend` 开一层，`take` 是屏障、自己一层（第 10 章）。

几条报错，注意各自的错误码和提示（只保留首行与提示）：

```text
error[SEM-005]: having 必须紧跟在 agg 之后
error[PAR-001]: cast 的目标类型 `bool` 不在允许的范围里
  提示: 类型只能是 i64 / f64 / string
error[SEM-003]: 不支持从 f64 转换到 string
  提示: 只支持四种转换：i64 → f64、f64 → i64、string → i64、string → f64
error[SEM-016]: 编译期可确定的定义域失败（除零/模零/整数溢出/非有限 f64 结果/必然失败的 cast）
```

同一条流水线里写错几处，它们会按第 9 章的规则一次报完。
## 踩坑实录

**1. `cast` 在 `extend` 里完全解析不了。** 单独测 `cast(…)` 没问题，一放进 `.extend({ x = cast(…) })` 就报「期望 `,` 或 `}`，遇到标识符 `cast`」。原因：`parse_primary` 分派给 `parse_cast` 时并没有消费 `cast` 这个关键字——`parse_if` 也是同样的约定，它的第一行就是 `self.advance()`——而我以为已经消费了。写新的「关键字 + 特殊参数形式」的解析函数时，先看一眼它的兄弟函数是怎么开始的。

**2. `1e300` 不是 Pebble 的字面量。** 我在测试里写 `cast(1e300 as i64)` 期望它报 `SEM-016`，结果报的是语法错误：词法层不认科学计数法，`1e300` 被读成了 `1` 和标识符 `e300`。这是语言的现状，不是 bug——但它意味着 `cast("1e300" as f64)` 合法，而程序里没法直接写出这个常量。我自己写的测试也因此挂了一次：Rust 的 `{:?}` 把 `1e16` 打印成了科学计数法。

**3. 差点把教程当成实现。** 探测 `round` 的取整公式时，我看到第 6 章教程里写着 `frac(x)`，又在 DuckDB 上试出「没有 `frac` 这个函数」，于是断定 `round` 不能执行。实际上实现用的是 `(x) - FLOOR(x)`，一直是对的；错的是教程里那段示意 SQL。教训：判断实现的行为，去看实现，或者跑一下——别拿文档当证据。那段示意 SQL 这次已经改成和实现一致。

**4. 规范读漏了一句话。** 第 10 章我说「`take` 不和 `sort` 合并，是因为规范没规定并列时谁在前」。这一章通读规范时才发现 7.11 写得很清楚：「并列键不承诺稳定顺序」。所以合并其实是允许的；不合并的真正理由是 v2 承诺与 v1 结果逐字相同。第 10 章的那句话已经改准。

**5. 三个测试抢一个目录。** 对拍测试并行运行，却共用同一个输出目录，每个测试一开头就清空它——于是互相删掉对方刚写出的文件。症状是「读不到输出文件」，看着像引擎的问题。给每个测试自己的子目录就好了。并行测试不共享可变状态，这条老规矩也适用于文件系统。

**6. 「节选」标记写错了。** 门禁只认 `（节选）`——右括号紧跟在「节选」后面。我在第 10 章写成了 `（节选：说明文字）`，于是整行没被当成标记，被当成代码去源码里找。复现一遍门禁的逻辑，问题立刻显形。写这类标记之前，先看已有的章节是怎么写的。

## 练习

1. **先预测，再验证。** 下面每个表达式结果是 null 还是数值？先在纸上写下答案，再用 `cargo test -p pebble-semantics cast_reference` 对照。
   ```pebble
   cast("  +007" as i64)     cast("1." as f64)      cast("1." as i64)
   cast("-0" as f64)         cast("1e-400" as f64)  cast(9223372036854775807 as f64)
   cast(4503599627370492.5 as i64)                  cast(-0.5 as i64)
   ```
2. **往返。** `cast(cast(n as f64) as i64)` 对哪些 `n` 能回到原值？对哪些不能？`|n| > 2^53` 时会发生什么？写一个数据文件验证你的预测。
3. **`f64 → string` 为什么难。** 规范要把输出格式定得与引擎无关：`0.1` 该打印成什么？`1e20` 呢？`1.0` 呢？查 DuckDB 的 `CAST(1e20 AS VARCHAR)` 和 Rust 的 `format!("{:?}", 1e20)`，看看它们差在哪里。然后给这个转换写一节规范草稿。
4. **科学计数法字面量。** 给词法层加上 `1e300` 这样的写法要改哪些地方？它会让哪些现在报语法错误的程序变成合法的？`SEM-015`（f64 字面量超出有限值域）该在哪一步检查？
5. **大小写映射的全集。** 查 Unicode 的 `SpecialCasing.txt`：有多少码点的全映射与简单映射不同？如果要在 SQL 里用守卫实现全映射，`upper` 需要处理多少个特例？词尾 sigma 这种依赖上下文的规则能用 `regexp_replace` 表达吗？估算之后，你会建议改规范还是改实现？
6. **`having` 之后再 `select`。** `.agg(…).having(…).select(…)` 生成的 SQL 有几层？`select` 为什么不能也并进聚合层？（提示：第 10 章规则二。）
