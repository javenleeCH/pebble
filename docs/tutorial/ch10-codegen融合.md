# 第 10 章 codegen 融合：不是每个算子都要一层子查询

> 对应文件：`crates/pebble-duckdb/src/lib.rs`（`Block` 及其构建过程）、`crates/pebble-hir/src/manifest.rs`（工件版本）。
> 演示文件：`tests/fixtures/codegen/fusion_demo.pebble`。
> 测试：`pebble-duckdb` 单测、`pebble-duckdb/tests/long_pipeline.rs`、`pebble-driver/tests/evidence_chain.rs`。

## 本章目标

1. 看懂一条 SELECT 能装下什么，从而知道哪些算子可以并进同一层、哪些必须分层；
2. 为每条合并规则写一句**等价性论证**；论证不了的就不合并；
3. 学会负责任地改变一个会被审批的工件：升版本、与旧实现对拍、把"没变的部分"钉成测试；
4. 认识两种很有用的验证手段：差分测试与变异验证。

## 为什么：三百个算子就跑不动了

第 6 章末尾留下过一张表。连续的 `.filter` 接到 300 个时 DuckDB 还能跑，350 到 450 个时它自己栈溢出，500 个以上解析器直接拒绝。原因不在引擎，在我们的 SQL 长什么样。

v1 的 codegen 给**每个算子**都套一层子查询。拿本章的演示文件来说：

```pebble
Table("tests/data/orders.csv") as o
  .filter(o.status == "paid")
  .extend({ doubled = o.amount * 2 })
  .select(order_id, customer_id, doubled)
  .sort(customer_id, order_id)
  .collect()
```

四个算子，v1 生成的 SQL 是四层嵌套，从里到外依次是 `s1 → s2 → s3 → s4`，最外面再包一层给终端起名字的 `final`。四层无所谓；但这条规则是"算子越多、嵌套越深"，四百个算子就是四百层，而 DuckDB 的解析器只能吃下几百层。

换个角度想：一条 SQL 的 `SELECT` 本来就有好几个子句，一个子句能对应好几个算子。

| SELECT 的子句 | 对应的 Pebble 算子 |
|---|---|
| `FROM … [JOIN … ON …]` | 源、`join` |
| `WHERE …` | `filter` |
| 选择列表 | `extend`、`select` |
| `GROUP BY …` | `group_by` + `agg` |
| `ORDER BY …` | `sort` |
| `LIMIT …` | `take` |

四个行级算子完全可以写进**同一条** SELECT。问题是：哪些真的可以合并，而结果不变？

## 代码走读

### 1. 块：一条 SELECT 的骨架

先把"一层"做成一个结构体，它的字段恰好是 SELECT 的各个子句：

```rust
struct Block {
    /// 开启这个块的节点编号，用作表别名 `s<n>`（join 块：`l<n>` / `r<n>`）
    id: pebble_hir::NodeId,
    /// FROM 里的内层 SQL
    from: String,
    /// FROM 直接提供的列：内层节点的输出字段 id（join 块：左、右两侧的并集）
    from_ids: BTreeSet<pebble_hir::FieldId>,
    /// join 的右侧 SQL 与连接条件
    join: Option<(String, String)>,
    /// join 块里由左表带来的列：生成列引用时要带表别名
    left_ids: Option<BTreeSet<pebble_hir::FieldId>>,
    /// 选择列表：列引用（透传）或 `<表达式> AS "fN"`
    select: Vec<String>,
    /// 已有 extend / select 并了进来：选择列表不再是纯透传，再来的投影必须开新的一层
    projected: bool,
    distinct: bool,
    /// WHERE 里的谓词，按流水线顺序。发射时合成一棵平衡的 AND 树（见 `and_all`）
    predicates: Vec<String>,
    group_by: Option<Vec<String>>,
    // …（节选）第 11 章在这里加了两个字段：having 与 subst
    order_by: Vec<String>,
    limit: Option<i64>,
    /// 屏障：之后的算子不得再并进来（distinct / take / 聚合）。
    /// 它们后面的 filter 若并进 WHERE，就等于跑到了它们前面——结果会变。
    /// 唯一的例外是聚合层：紧跟其后的 filter 是「对分组结果过滤」，并进它的 HAVING
    barrier: bool,
}
```

一个块就是一条 `SELECT [DISTINCT] <选择列表> FROM (<from>) [JOIN …] [WHERE …] [GROUP BY …] [ORDER BY …] [LIMIT …]`。生成 SQL 的过程变成：沿着流水线走，每遇到一个算子，**要么把它并进当前这个块，要么把当前块封口、开一个新块**。

哪些字段需要解释一下：

- `from_ids`：这一层的 FROM **直接提供**的列。它是判断"能不能并"的关键。
- `projected`：这一层已经算过新列了。一层只能投影一次。
- `barrier`：屏障。它后面的算子一律不许并进来。

把块变成字符串的函数就是把子句按 SQL 规定的顺序拼起来：

```rust
    fn render(&self) -> String {
        let mut sql = String::from("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&self.select.join(", "));
        match &self.join {
            None => {
                let _ = write!(sql, " FROM ({}) AS \"s{}\"", self.from, self.id);
            }
            Some((right, on)) => {
                let _ = write!(
                    sql,
                    " FROM ({}) AS \"l{}\" JOIN ({}) AS \"r{}\" ON ({})",
                    self.from, self.id, right, self.id, on
                );
            }
        }
        if !self.predicates.is_empty() {
            let _ = write!(sql, " WHERE ({})", and_all(&self.predicates));
        }
        if let Some(keys) = &self.group_by {
            if !keys.is_empty() {
                let _ = write!(sql, " GROUP BY {}", keys.join(", "));
            }
        }
        if !self.having.is_empty() {
            let _ = write!(sql, " HAVING ({})", and_all(&self.having));
        }
        if !self.order_by.is_empty() {
            let _ = write!(sql, " ORDER BY {}", self.order_by.join(", "));
        }
        if let Some(n) = self.limit {
            let _ = write!(sql, " LIMIT {n}");
        }
        sql
    }
```

下面是四条规则。每一条都配一句论证；**说不出论证的合并，就不做**。

### 2. 规则一：filter 并进 WHERE

```rust
            TransformKind::Filter { pred } => {
                // …（节选）第 11 章在这里加了一段：聚合层开着时，filter 并进它的 HAVING
                let pred_sql = self.emit_expr(pred, &NO_SUBST);
                // 能并进当前的 WHERE 吗？谓词引用的列必须都直接来自这一层的 FROM。
                // 引用了本层选择列表里新算出来的列就不行：标准 SQL 的 WHERE 看不到同一个 SELECT 里的别名。
                //（DuckDB 其实允许这种引用——那是它的方言扩展。不依赖它：生成的 SQL 只建立在标准语义上。）
                let mut refs = BTreeSet::new();
                field_refs(pred, &mut refs);
                let fusable = match open {
                    Some(b) if !b.barrier => refs.iter().all(|id| b.from_ids.contains(id)),
                    _ => false,
                };
                if !fusable {
                    let from = self.close(open, sql, input)?;
                    *open = Some(Block::passthrough(me, from, input_ids));
                }
                open.as_mut().expect("刚开的新层").predicates.push(pred_sql);
            }
```

**论证**：`WHERE p1` 之后再 `WHERE p2`，留下的行是"p1 为真且 p2 为真"的行；写成一个 `WHERE p1 AND p2`，留下的是同一批行。SQL 的 `AND` 也是三值逻辑的 Kleene 语义（第 6 章），所以谓词结果为 NULL 的行在两种写法下都被丢弃。

**条件**：谓词引用的每一列都必须是这一层的 FROM 直接提供的（`from_ids`）。为什么？看这个程序：

```pebble
  .extend({ doubled = o.amount * 2 })
  .filter(doubled > 100)
```

如果两个算子并进同一层，SQL 会是 `SELECT …, "f2" * 2 AS "f5" … WHERE ("f5") > (100)`——`WHERE` 里用到了**同一层选择列表里刚起的名字**。标准 SQL 不允许这样写：`WHERE` 在逻辑上先于选择列表求值，看不到它的别名。所以这个 `filter` 必须另开一层。

有意思的是，我在 DuckDB 上直接试了：

```text
D > SELECT a AS c FROM (VALUES (1),(2),(3)) t(a) WHERE c > 1;
2
3
```

它居然允许。这是 DuckDB 的方言扩展。规则仍然按标准 SQL 来定——不依赖它，生成的 SQL 就只建立在标准语义上，换一个更严格的引擎也不会出问题。这和第 6 章"每个默认行为都要审"是同一条纪律：**碰巧被允许，不等于可以依赖**。

### 3. 规则二：extend / select 并进选择列表

```rust
    fn project(
        &self,
        me: pebble_hir::NodeId,
        base: &pebble_hir::NodeBase,
        items: Vec<(pebble_hir::FieldId, Option<String>)>,
        sql: &mut [Option<String>],
        open: &mut Option<Block>,
    ) -> Result<(), String> {
        let input = base.inputs[0] as usize;
        let mut block = match open.take() {
            Some(b) if !b.barrier && !b.projected => b,
            other => {
                // 放回去让 close() 封口（other 是 Some(不可并的块) 或 None）
                *open = other;
                let from = self.close(open, sql, input)?;
                let mut fresh = Block::passthrough(me, from, self.output_ids(base.inputs[0]));
                fresh.projected = true;
                fresh
            }
        };
        block.select = items
            .iter()
            .map(|(id, expr)| match expr {
                None => block.column_ref(*id),
                Some(e) => format!("{e} AS \"f{id}\""),
            })
            .collect();
        block.projected = true;
        *open = Some(block);
        Ok(())
    }
```

**论证**：`extend` 算出来的新列是所在行的纯函数。先算新列、还是把行先过滤一遍再算，每一行得到的新列都一样。`select` 同理——它只是选择列表变短了。

**条件**：这一层还没投影过（`!projected`），而且不是屏障。一层只能投影一次的原因和上一条一样：第二个 `extend` 的表达式会引用第一个 `extend` 刚起的名字，而同一个选择列表里的表达式互相看不见。所以 `.extend(…).extend(…)` 永远是两层。

透传列怎么写，交给块自己决定：

```rust
    fn column_ref(&self, id: pebble_hir::FieldId) -> String {
        match &self.left_ids {
            Some(left) if left.contains(&id) => format!("\"l{}\".\"f{}\"", self.id, id),
            Some(_) => format!("\"r{}\".\"f{}\"", self.id, id),
            None => format!("\"f{}\"", id),
        }
    }
```

`join` 开的那一层里，原有的列带表别名（第 6 章：不依赖两边列名恰好不重复这种巧合），并进来的新列直接追加在选择列表末尾。

### 4. 规则三：sort 并进 ORDER BY，但 take 不并

**`sort` 的论证**：`ORDER BY` 只决定输出的顺序。先排序、之后再算新列，行还是那个顺序；先过滤、再排序，和先排序、再过滤留下的是同一批行，留下的行之间的相对顺序也不变。条件是这一层还没有 `ORDER BY` 和 `LIMIT`。

**`take` 为什么不并**：`ORDER BY … LIMIT n` 写在同一条 SELECT 里，DuckDB 可能走一条专门的执行路径（top-N）。当排序键有并列、而截断线正好落在并列处时，它选出的行不一定和"先完整排序、再取前 n 行"一样。规范（7.11）对并列时的顺序不做承诺，所以两种写法都合规；但 v2 对 v1 的承诺是**结果逐字相同**，而同一个程序在并列处的输出可能因这次合并而改变。所以 `take` 自己占一层，和 v1 完全一样。

### 5. 屏障：并进去就会改变结果的算子

`distinct`、`take`、聚合是屏障。原因都是同一个：**在流水线里，它们后面的 `filter` 是在它们之后执行的；并进同一个 `SELECT` 的 `WHERE`，却会在它们之前执行。**

| 屏障 | 后面的 filter 若并进 WHERE | 例子 |
|---|---|---|
| `take` | 变成"先过滤、再截取" | 取金额最高的 3 行（11000、9000、3000），再保留小于 10000 的：应当剩 2 行；并进去会多出一行 700 |
| 聚合 | 变成"分组之前过滤"——那是 `having`，不是 `filter` | `.agg(n = count()).filter(n > 1)` |
| `distinct` | 变成"去重之前过滤" | 结果其实相同（重复的行一模一样，谓词结果也相同），但这是一次**改写**而不是一次**合并**；改写需要单独论证，本章不做 |

第三行是故意保守的：它说明"结果相同"不是合并的唯一标准，还得说清楚**为什么**相同，并为此负责。

聚合那一行后来有了唯一的例外：紧跟在聚合之后的 `filter`，并进聚合层的 `HAVING`。论证和实现都在[第 11 章](ch11-having与cast.md)——那正是 `having` 这个语法糖落脚的地方。

### 6. join：开新的一层，但它不是屏障

`join` 改变了 FROM 本身，所以必须开新的一层。但它之后的世界是确定的：两侧的列都由这一层直接提供（`from_ids` 是左右两侧的并集）。所以后面的 `filter`、`extend`、`select`、`sort` 都可以并进来：

```text
SELECT "f0", "f10" FROM (
  SELECT "f0", …, "f9", <a.amount + b.amount> AS "f10" FROM (
    … 左表 …
  ) AS "l2" JOIN (
    … 右表 …
  ) AS "r2" ON ((("f0") = ("f5"))) WHERE ((("f7") > (300)))
) AS "s5" ORDER BY "f0" ASC NULLS LAST
```

这是 `.join(…).filter(b.amount > 300).extend({ z = a.amount + b.amount }).select(a.order_id, z).sort(a.order_id)` 的结果。v1 里它是六个中间层，现在是两个。

### 7. AND 必须是一棵平衡树

```rust
fn and_all(preds: &[String]) -> String {
    match preds.len() {
        1 => preds[0].clone(),
        n => {
            let (left, right) = preds.split_at(n.div_ceil(2));
            format!("({} AND {})", and_all(left), and_all(right))
        }
    }
}
```

两万个 `filter` 并进一个 `WHERE` 之后，问题只是换了个地方：如果把它们从头到尾串成 `(((p1 AND p2) AND p3) AND …)`，括号深度等于谓词个数，DuckDB 的解析器在 1000 层就会拒绝（`Max expression depth limit of 1000 exceeded`）。

平分两半、递归下去，深度就只有 log₂(n)：两万个谓词是 15 层。

这不是理论上的担心。我做变异验证时把它改回左结合，两千个 `filter` 的端到端测试立刻在 DuckDB 上失败了。

### 8. 还没封口的块

并进来的节点自己不产生 SQL——它的输出在它最后一个同层邻居那里。所以每个节点的 SQL 是 `Option<String>`，只有"封口"的那一刻才写进去：

```rust
    fn close(
        &self,
        open: &mut Option<Block>,
        sql: &mut [Option<String>],
        input: usize,
    ) -> Result<String, String> {
        if let Some(block) = open.take() {
            let text = block.render();
            sql[input] = Some(text.clone());
            return Ok(text);
        }
        sql[input]
            .clone()
            .ok_or_else(|| format!("内部错误：节点 {input} 的 SQL 尚未生成"))
    }
```

什么时候封口？当一个不能并进来的算子到来，它需要"上一层的结果"作为自己的 FROM——那就把当前块封口，结果记在它的输入节点名下。流水线是线性的，节点又按拓扑序处理，所以"当前块的最后一个节点"永远就是下一个算子的输入。

### 9. 版本标在哪里

SQL 的形态变了，所有程序的工件哈希都会变。要让旧审批不可能被误用到新构建上，靠的是两个地方：

```text
plan.sql 的第一行：          -- pebble-duckdb artifact v2 (canonical)
工件哈希的域分隔前缀：       pebble-backend-artifact/duckdb-sql/v2
```

后者参与工件哈希的计算，所以 v1 和 v2 的工件**不可能**得到相同的哈希。

值得注意的是**没有**改的两个字段。`manifest_version` 是 manifest 这个文件格式的版本，格式没变；`backend.version` 是**引擎**（DuckDB）的版本，和 SQL 怎么写无关。我在动手前一度说过要把它们一起升上去，读了 `manifest.rs` 才发现不对——**改版本号之前，先弄清楚那个版本号数的是什么。**

## 怎么证明没改坏

这是本章最重要的部分。改 codegen 不难，难的是相信改完之后每个程序的结果都没变。这里用了五种互相独立的办法。

### 1. 没变的部分先钉住

融合只在"相邻且可并"时才改变 SQL。所以给每种**单个算子**的流水线写一条测试，断言 SQL 正文与 v1 **逐字相同**（`single_operator_pipelines_keep_the_v1_text`）。有了这条，v1 到 v2 的差别就只剩下"相邻可并的地方"，可以逐条枚举。

这条测试还顺手抓到两处我自己都没察觉的漂移：单个 `filter` 少了一对括号，单个 `join` 的透传列丢了表别名。

### 2. 差分测试：和旧实现比结果

把提交 `bebd87c`（v1）检出一个临时工作树，编译出旧的 `pebble.exe`；再写 44 个程序，覆盖每条规则、每个"不该合并"的边界以及它们的组合，分别用新旧两个编译器编译、在同一个 DuckDB 上运行，比较输出：

| 程序 | v1 层数 | v2 层数 | 输出 |
|---|---|---|---|
| 三个 `filter` | 4 | 2 | 相同 |
| `extend` 后 `filter` 一个原有的列 | 3 | 2 | 相同 |
| `extend` 后 `filter` 新列（必须分层） | 3 | 3 | 相同 |
| `select` 后 `sort` | 3 | 2 | 相同 |
| `take` 后 `filter`（屏障） | 4 | 4 | 相同 |
| `agg` 后 `filter`（屏障） | 4 | 3 | 相同 |
| `join` + `filter` + `extend` + `select` + `sort` | 6 | 3 | 相同 |

44 个程序，输出全部逐字相同。它的力量来自：**期望值不靠人脑算**，而是一个已经经过四幕验收的旧实现。

它也有盲点，见练习 6。

### 3. 第三种算法：直接从数据算

端到端测试里的期望值既不取自 v1，也不取自 v2，而是测试自己读 CSV、用几十行 Rust 一行行算出来的。其中有一个程序是专门为"屏障"设计的：

```pebble
  .sort(o.amount desc)
  .take(3)
  .filter(o.amount < 10000)
  .select(order_id)
```

正确的答案是 2 行。如果 `take` 的屏障被去掉，`filter` 就会并进同一个 SELECT，变成"先过滤再截取"，答案是 3 行。

### 4. 变异验证：把规则弄坏，看测试会不会红

| 故意弄坏的东西 | 哪条测试变红 |
|---|---|
| `filter` 不再看它引用了哪些列 | 单元测试：`filter` 并进了算出新列的那一层 |
| 去掉 `take` 的屏障 | 单元测试 + **端到端**（结果多出一行） |
| AND 树改回左结合 | 单元测试 + **端到端**（DuckDB 拒绝两千层的表达式） |

每一行都是真实做过的。第一条只有单元测试变红——因为 DuckDB 恰好允许 `WHERE` 引用别名，结果碰巧还是对的；这正是"形状测试"和"结果测试"要同时有的理由。

### 5. 规模实测

| 连续 `filter` 个数 | v1 | v2 |
|---|---|---|
| 300 | 能跑 | 能跑 |
| 400 | 失败 | 能跑 |
| 5,000 | 失败 | 能跑 |
| 20,000 | 失败，编译 10 秒 | 能跑，编译 3 秒 |

v2 的嵌套始终是两层，结果与单个 `filter` 逐字相同。第 6 章末尾那张"诚实的边界"表，可以划掉了。

## 跑起来

```powershell
cargo test -p pebble-duckdb                              # 融合规则的单测
cargo test --workspace --test long_pipeline              # 两千个 filter：两层嵌套、一个小栈
cargo test --workspace --test evidence_chain fused -- --nocapture   # 在真实 DuckDB 上与第三种算法比结果

# 演示文件：四个行级算子
cargo run -p pebble-driver -- build tests/fixtures/codegen/fusion_demo.pebble --locks tests/locks --out target/fusion_demo
```

最后一条命令生成的 plan.sql（只调整了换行和缩进）：

> `SET memory_limit = '512MB';` 前言是第 13 章加的——运行时资源上限与 threads
> 一样属于被审内容，它改变全部 canonical SQL（见 ch13 的顺序契约）。

```sql
-- pebble-duckdb artifact v2 (canonical)
SET threads TO 1;
SET memory_limit = '512MB';
SELECT "f0" AS "order_id", "f4" AS "customer_id", "f5" AS "doubled"
FROM (
  SELECT "f0", "f4", "f5"
  FROM (
    SELECT "f0", "f1", "f2", "f3", "f4",
           CASE WHEN ("f2") IS NULL THEN NULL
                WHEN (2) IS NULL THEN NULL
                WHEN (CAST(("f2") AS HUGEINT)) * (CAST((2) AS HUGEINT)) BETWEEN -9223372036854775808 AND 9223372036854775807
                THEN CAST((CAST(("f2") AS HUGEINT)) * (CAST((2) AS HUGEINT)) AS BIGINT)
                ELSE NULL END AS "f5"
    FROM (
      SELECT "t0"."order_id" AS "f0", "t0"."status" AS "f1", "t0"."amount" AS "f2", "t0"."score" AS "f3", "t0"."customer_id" AS "f4"
      FROM read_csv('tests/data/orders.csv', header = true, columns = {'order_id': 'BIGINT', 'status': 'VARCHAR', 'amount': 'BIGINT', 'score': 'DOUBLE', 'customer_id': 'VARCHAR'}) AS "t0"
    ) AS "s1"
    WHERE ((("f1") = ('paid')))
  ) AS "s3"
  ORDER BY "f4" ASC NULLS LAST, "f0" ASC NULLS LAST
) AS "final";
```

从里往外读：

- 最里层是把表的列重命名成 `f<id>` 的源；
- `s1`：`filter` 和 `extend` 并在了一起——`WHERE` 在这层，新列 `doubled`（`f5`）也在这层的选择列表里；
- `s3`：`select` 把选择列表换成了三列，`sort` 并进了它的 `ORDER BY`；
- `final`：把 `f<id>` 换回用户看得见的名字。

四个算子，两层。数一数任意一条流水线的层数：

```powershell
(Select-String -Path target/fusion_demo/plan.sql -Pattern 'FROM \(' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
```

## 踩坑实录

**1. 对拍脚本报了 44 个"不同"。** 第一次跑差分测试，44 个程序全部标成 `DIFFERENT`，可我逐个看数据行明明一模一样。原因是 `pebble run` 的输出里有一行 `receipt = …\out-xxx-v1\receipt.json`，新旧两个版本的输出目录不同。**当所有用例都以同一种方式失败时，先怀疑测试本身。**

**2. 两个对拍程序本身不合法。** 44 个里有两个在新旧编译器上都编译失败：一个在 `select` 之后引用了被裁掉的字段（SEM-012），一个在自连接之后用了有歧义的裸字段名（SEM-011）。这是我写测试程序时的错，不是 codegen 的错——两个版本的编译器报得完全一样。不合法的程序测不到 codegen，要从对拍里挑出去。

**3. 不变的部分原来已经变了。** "单算子与 v1 逐字相同"这条检查一开始是红的：单个 `filter` 少了一对括号，单个 `join` 的透传列丢了表别名。都是我在重写时不经意改掉的。**先钉住"不该变的部分"，才看得清"变了的部分"。**

**4. 期望值手打错了三次。** 测试里手写的 SQL 原文多了一对括号；层数两次数错（忘了最外层的 `final`，或者漏数了中间一层）。最后都是拿实际输出对照才改对的。给"生成的文本"写期望值时：编号从 HIR 里读（`node_id_where`），原文从可信的来源抄（旧实现的输出），层数先想清楚"块 + final"各是多少。

**5. 变异没被任何测试看见。** 我第一次破坏 `take` 的屏障：什么都没红。查下去发现变异本身写得不对（后面一行又把它设了回去），改对之后单元测试红了，端到端却还是绿的——因为没有程序能区分"先截取后过滤"和"先过滤后截取"。于是补了一个能区分的程序（规则五表格里的第一行），这才在结果层面抓住它。**变异验证不仅能检验测试，也能检验变异本身。**

## 练习

1. **先预测，再验证。** 对下面每条流水线，先在纸上写出层数（块数 + `final`），再用上面那条 `Select-String` 命令数。不一致的地方，回到四条规则里找原因。

   ```pebble
   .filter(a > 1).filter(b > 2).select(a).sort(b)
   .extend({ x = a * 2 }).extend({ y = x + 1 }).filter(y > 3)
   .group_by(a).agg(n = count()).filter(n > 1).sort(n)
   .join(…).filter(…).select(…).take(5)
   ```

2. **`sort` + `take` 能不能并？** 先在 `06-operators` 里把"排序键并列时谁在前"规定下来（稳定排序？按什么打破并列？），再试着把 `take` 并进带 `ORDER BY` 的那一层。构造一个并列恰好落在截断线上的数据集，看看 DuckDB 的 top-N 和完整排序的结果是否一样。

3. **连续两个 `extend`。** 它们必须分层，因为第二个引用了第一个的新列。要想合并，只能把第一个的表达式**内联**进第二个。代价是什么？（提示：第 6 章"表达式膨胀"——内联会让守卫代码重复多少次？）

4. **为 `having` 做准备。** 第 11 章要给语言加 `having`——"对分组结果过滤"。它恰好就是聚合这个屏障后面的 `filter`。到那时屏障规则该怎么改？`having` 应该并进聚合那一层的哪个子句？

5. **CTE 形态。** 把子查询改成 `WITH s1 AS (…), s3 AS (…)` 的写法：嵌套深度变成多少？可读性如何？哪些程序的 SQL 会变？它够不够格算一次新的工件版本？

6. **差分测试的盲点。** 对拍用的数据只有 10 行，里面没有：排序键并列、整数溢出、f64 的 NaN、空表。各举一个"新旧实现会给出不同结果、但对拍发现不了"的假设情形，然后往 `tests/data` 里补一份专门的对拍数据。
