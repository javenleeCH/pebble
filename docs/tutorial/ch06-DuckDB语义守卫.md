# 第 6 章 后端 codegen：不继承 DuckDB 的默认行为

> 对应文件：`crates/pebble-duckdb/src/lib.rs`。本章回答一个被两轮评审反复拷问的问题：**把执行委托给外部引擎后，「审的那份」和「跑的那份」到底什么关系？**

## 本章目标

1. 学会把语言语义「翻译」而不是「直译」成目标 SQL；
2. 逐个实现语义守卫：判空、整数 checked、f64 规范化、round ties-to-even、排序 null 位置；
3. 明确信任边界：什么被证据链绑定，什么必须信任 DuckDB；
4. 认识 codegen 的两个结构性陷阱——列的来源取错了一层、为了取内层而递归——以及「能编译 ≠ 能执行」对测试意味着什么。

## 为什么后端是最容易背叛语义的一层

前端语义层（第 4 章）再严谨，只要 codegen 把 `x != null` 直译成 `x != NULL`，判空就退化成 SQL 三值逻辑的坑；只要整数溢出直译成 `a + b`，checked-null 就变成运行时报错。**语义封闭红线的最后一公里在后端**：语言承诺的每一条取值规则，都必须在生成的 SQL 里有显式的对应物，或者有测试钉住「后端默认行为恰好一致」。「恰好一致」不写测试，下次引擎升级就变「悄悄错」——这是本章反复出现的纪律。


## 代码走读

### 翻译，不是直译

最朴素的 codegen 是 `a + b` → `a + b`。问题出在语义差集上：

| Pebble 语义 | DuckDB 默认 | 后果 |
|---|---|---|
| `x != null` 恒 `bool` | `x != NULL` → NULL | 判空必须走 `IS NOT NULL` |
| 整数溢出 → null | 溢出 → **报错** | 必须加守卫 |
| `/0`、`%0` → null | 整除零报错 | 必须加守卫 |
| f64 非有限结果 → null | `1e308*10` → inf | 必须 isfinite 检查 |
| `-0.0` 规范化为 `+0.0` | 保留 -0.0 | 必须规范化 |
| `round` ties-to-even | DuckDB 四舍五入进位 | 必须手写公式 |
| `trim` 去除两端 Unicode White_Space | 默认的 `trim` 不去制表符 | 显式给出全部 25 个码点（第 11 章） |
| `cast` 的四种转换 | 接受空白、下划线、`inf`；`f64→i64` 进位且越界报错 | 先用守卫收敛到规范，再取值（第 11 章） |
| sort 全键 nulls last | 默认 nulls last…但 asc/desc 语义有方言差异 | 显式写 `NULLS LAST` |

规则很简单：**DuckDB 的每个默认值都要审**。碰巧一致的（`AND`/`OR` 恰是 Kleene、聚合跳过 null、`GROUP BY` null 同组）可以直用，但要在测试里钉住——「碰巧对」不写测试，下次升级就变「悄悄错」。

## 守卫三例

**1）整数 checked：HUGEINT 中转**

两个 i64 相加可能溢出，但两个 i64 转成 HUGEINT（128 位）相加**不可能**溢出。先在宽类型里算，再检查是否落回 i64：

```text
CASE WHEN (l) IS NULL THEN NULL
     WHEN (r) IS NULL THEN NULL
     WHEN CAST(l AS HUGEINT) + CAST(r AS HUGEINT) BETWEEN -9223372036854775808 AND 9223372036854775807
     THEN CAST(CAST(l AS HUGEINT) + CAST(r AS HUGEINT) AS BIGINT)
     ELSE NULL END
```

`i64::MIN / -1`、`abs(i64::MIN)` 这些经典边界全部被同一个模式覆盖。**在更宽的类型里计算、再收窄**——这是整数 checked 语义的通用技巧，比逐条边界判断简洁得多。

**2）f64 规范化：`+ 0.0` 技巧**

需求：非有限结果（NaN/±∞）→ null；`-0.0` → `+0.0`。朴素的 CASE 要三处引用表达式（爆炸见下）。利用 IEEE 的两个恒等式：

- `v + 0.0 == v`（除 `v = -0.0` → `+0.0`，正是我们要的）
- `isfinite(NULL)` → NULL，CASE 落到 ELSE → NULL（null 自动传播，不用显式判）

于是：

```text
CASE WHEN NOT isfinite((e)) THEN NULL ELSE ((e) + 0.0) END
```

两处引用代替三处。除法 `x/0.0` 在 IEEE 下得 ±inf，正好被 isfinite 拦下。

**3）round ties-to-even：SQL 里没有的东西自己拼**

DuckDB 的 `round(2.5) = 3`，Pebble 承诺 `round(2.5) = 2`。手写（与实现生成的 SQL 一致，只把 `x` 简写了）：

```text
CASE WHEN ((x) - FLOOR((x))) < 0.5 THEN FLOOR((x)) + 0.0        -- 离 .5 远：直接取
     WHEN ((x) - FLOOR((x))) > 0.5 THEN FLOOR((x)) + 1.0
     WHEN MOD(CAST(FLOOR((x)) AS BIGINT), 2) = 0 THEN FLOOR((x)) + 0.0   -- 恰在 .5：取偶
     ELSE FLOOR((x)) + 1.0 END
```

小数部分写成 `(x) - FLOOR(x)` 有两个原因。其一，DuckDB 1.4 里没有 `frac` 这个函数——本节的示意曾经写成 `frac(x)`，与实现不一致，是第 11 章把 `round` 的结果放到真实 DuckDB 上验证时发现的。其二，也不能图省事写成 `x - TRUNC(x)`：负数的小数部分会变成负的（`-1.2 - (-1) = -0.2`），第一行 `< 0.5` 就会误判，`round(-1.2)` 得到 -2。用 `FLOOR` 时小数部分始终落在 `[0, 1)`，公式对正负数一视同仁：`round(-1.2) = -1`，`round(-2.5) = -2`。

注意第三行安全的边界条件：小数部分等于 0.5 只可能出现在 `|x| < 2^53`（再大的浮点没有小数部分），所以 `CAST(FLOOR(x) AS BIGINT)` 不会溢出——**每个守卫都要自证安全性**，这条推论写进了 crate 文档。

## 表达式膨胀：守卫的代价

守卫让每个表达式出现 2~4 次，嵌套深度 d 的表达式膨胀到 O(2^d) 或 O(4^d)。M0 的表达式都很浅（1~3 层），可接受；这是**用可读性换正确性**的明确取舍。解法在未来：CTE 物化中间结果（`WITH t AS (...) SELECT ... FROM t`），把表达式拆成命名节点——那是 M4 原生后端之前的优化项，规范先记录问题。

## canonical SQL：输出即工件

生成的 SQL 不是「能跑就行」的字符串，它是**进入 manifest 的执行工件**（第 7 章）：

```text
-- pebble-duckdb artifact v1 (canonical)
SET threads TO 1;
SELECT ... ;
```

`SET threads TO 1` 不是性能选项——**单线程浮点聚合的求值顺序是语义的一部分**。多线程并行浮点求和的结果与结合顺序相关，会破坏「同一计划跑两次结果一致」。把引擎配置写进 canonical SQL、再进 manifest，配置本身就是被审的内容。

列名一律 `f<field_id>`（第 5 章的身份直通后端），只在最外层 select 才出现用户可见名——中间层根本不存在「重名字段」问题。

## 一层套一层：流水线怎么变成 SQL

> 本节描述的是 **v1** 的 SQL 形态：每个算子一层子查询。[第 10 章](ch10-codegen融合.md)把它改成了 v2——连续的行级算子并进同一层 `SELECT`。两个病例的病因分析与修法不受影响。

先把结构说清楚，下面两个 bug 都出在这里。v1 把 HIR 的每个节点翻译成**一层子查询**，包住它的输入：

```text
Table("orders.csv")        →  SELECT "t0"."status" AS "f0", … FROM read_csv(…) AS "t0"
  .filter(o.amount > 1)    →  SELECT "f0", "f1", "f2" FROM ( …上一层… ) AS "s1" WHERE (…)
  .extend({ x = 1 })       →  SELECT "f0", "f1", "f2", 1 AS "f3" FROM ( …上一层… ) AS "s2"
```

三个算子就是三层嵌套。列名一律是 `f<字段 id>`，所以每一层只做一件事：决定**输出哪些列**。

`extend` 这一层的规则：输出字段要么是从输入**透传**来的（写 `"fN"`），要么是本层某个绑定算出来的（写 `<表达式> AS "fN"`）。`join` 这一层的规则：左表的列加右表的列，各出现一次。

### 病例一：「输入的输入」——编译通过的坏 SQL

两条规则都得先知道「输入有哪些列」。取这件事的辅助函数，最初写成了这样：

```rust
fn input_schema_ids(&self, node_id: NodeId) -> BTreeSet<FieldId> {
    for &input in &self.hir.nodes[node_id].base().inputs {    // ← 取的是 node_id 的「输入」
        for f in &self.hir.nodes[input].base().output.fields { … }
    }
}
```

它返回的不是「节点 `node_id` 的输出列」，而是「`node_id` 的**输入**的输出列」——多套了一层，而调用方要的是前者。

于是：

- `extend` 直接接在源后面：源的「输入」是空集，透传列一个都找不到，报 `extend 输出字段 f0 无来源`。合法程序被拒绝，退出码 1。
- `join` 直接接在源后面：左表的列一个都列不出来，生成 `SELECT , "f0", "f1", …`。注意这**不会让编译失败**——SQL 只是一个字符串，编译器并不检查它合不合语法。坏 SQL 进了 manifest，`pebble build` 报告成功；要到 `pebble run` 时 DuckDB 才说 `Parser Error: syntax error at or near ","`。
- 它们前面只要隔着一个别的算子（比如 `.filter`），结果又碰巧是对的。

最后一条解释了这个 bug 为什么能活到现在：全部样例都「能编译」，而样例 02 恰好是 `join` 直接接在源后面——它的 `plan.sql` 一直是坏的，只是从来没有被真正执行过。

| 写法 | 修复前 | 修复后 |
|---|---|---|
| `.extend({ x = o.amount + 1 })` 作为第一个算子 | 编译失败 | 正常 |
| `.filter(…).extend({ x = 1 })` | 正常 | 正常 |
| `.join(…)` 作为第一个算子 | **编译成功，SQL 是坏的** | 正常 |
| `.filter(…).join(…)` | 能跑，但左表的列出现两遍 | 正常 |

两条教训都很值钱：

1. **症状不是病因。** 这个问题最初被报告为「`extend` 里写常量绑定会失败」。听起来合理，却是错的：`.filter(…).extend({ x = 1 })` 一切正常，`.extend({ x = o.amount + 1 })` 反而失败。找到病因靠的是把四种形状各跑一遍（上表），而不是盯着被报告的那一个。
2. **「能编译」不等于「能执行」。** 后端的测试如果只断言 build 成功，就等于没测后端。所以这次的回归测试里，专门有一条在真实 DuckDB 上把 `extend`、`join` 直接接在源后面的程序跑起来，并核对输出的每一行。

修复之后，7 个样例里只有样例 02 的 SQL 变了，其余 6 个逐字节不变。修 bug 时顺手证明「没改坏别的」，这个对照值得每次都做。

### 病例二：为了取「内层」而递归

既然每个节点包住它的输入，最顺手的写法是递归：

```rust
fn emit_node(&self, id: NodeId) -> Result<String, String> {
    let inner = self.emit_node(base.inputs[0])?;    // 先递归地生成内层
    Ok(format!("SELECT … FROM ({inner}) AS …"))
}
```

n 个算子的流水线，就在调用栈上叠 n 层。两万个 `.filter` 连起来，编译器栈溢出，一条诊断都没有。这和第 8 章「别把调用栈撑爆」是同一个教训，只是换了一层：那次是表达式递归，这次是流水线递归。

这里的解法比设上限更好，因为**根本不需要递归**。HIR 的节点是按拓扑序存放的：每个节点都排在它的输入之后。按存放顺序从前往后逐个生成，轮到某个节点时，它的输入早就生成好了，直接从数组里取：

```rust
    fn emit_all(&self) -> Result<Vec<Option<String>>, String> {
        let mut sql: Vec<Option<String>> = vec![None; self.hir.nodes.len()];
        let mut open: Option<Block> = None;
        for (i, node) in self.hir.nodes.iter().enumerate() {
            // 防御：若某个输入排在消费者之后（或不存在），下标取值就会错——明确报内部错误
            if let Some(bad) = node
                .base()
                .inputs
                .iter()
                .find(|&&input| input as usize >= i)
            {
                return Err(format!(
                    "内部错误：节点 {i} 的输入 {bad} 未排在它之前（HIR 不是拓扑序）"
                ));
            }
            match node {
                HirNode::Source { base, source, kind } => {
                    // 源是最里层：把表里的列重命名成 f<id>（第 5 章的身份直通后端）
                    sql[i] = Some(self.emit_scan(base.id, *source, kind)?);
                }
                HirNode::Terminal { base, .. } => {
                    // 终端不属于任何一层；它只负责把最后一个块封口
                    self.close(&mut open, &mut sql, base.inputs[0] as usize)?;
                }
                HirNode::Transform { base, kind } => {
                    self.lower_transform(i, base, kind, &mut sql, &mut open)?;
                }
                HirNode::Aggregate {
                    base,
                    group_keys,
                    bindings,
                } => {
                    let input = base.inputs[0] as usize;
                    let from = self.close(&mut open, &mut sql, input)?;
                    open = Some(self.aggregate_block(
                        i as pebble_hir::NodeId,
                        from,
                        self.output_ids(base.inputs[0]),
                        group_keys,
                        bindings,
                    ));
                }
            }
        }
        Ok(sql)
    }

```

同一个仓库里，「沿着输入走」一共有三样办法，各管一类情形：

| 场景 | 办法 | 例子 |
|---|---|---|
| 递归深度由用户的嵌套决定，嵌套本身就是语言结构 | **设上限**，超过就报诊断 | 表达式嵌套 64 层（第 8 章） |
| 节点已经排好拓扑序 | **自底向上**循环，结果存进数组 | SQL codegen、LaTeX 视图 |
| 要反向走，或者要判断有没有环 | **显式栈** | `Hir::validate` |

### 怎么测「会不会栈溢出」

两万个算子的流水线，测试里跑一遍要十几秒，太慢。换个思路：不放大输入，**缩小栈**。

```rust
std::thread::Builder::new()
    .stack_size(256 * 1024)    // 256 KiB
    .spawn(move || { /* 给 2 000 个算子的流水线做 codegen */ })
```

递归实现在 256 KiB 的栈上叠 2 000 层，必然溢出；迭代实现根本不碰栈。这条测试在修复前会让测试进程直接崩溃（`thread '<unknown>' has overflowed its stack`，CI 红灯），修复后 0.07 秒跑完。SQL codegen 和 LaTeX 视图各有一条（`crates/pebble-duckdb/tests/long_pipeline.rs`、`pebble-latex` 的单测）。

### 诚实的边界：引擎也有上限（已在第 10 章解决）

编译器不再崩溃了，但「一个算子一层子查询」这种 SQL 形态本身有极限。在 DuckDB 1.4.1 上实测，连续 `.filter` 的个数：

| 个数 | 结果 |
|---|---|
| 300 | 正常运行 |
| 350–450 | **DuckDB 自己栈溢出**（回执里记着退出码 `-1073741571`） |
| 500 及以上 | DuckDB 解析器拒绝：`Max expression depth limit of 1000 exceeded` |

也就是说，超过三百个算子的流水线，v1 后端执行不了。失败会被如实记进回执（第 7 章：失败也是证据），不会悄悄出错，但这是个真实的限制——发现它的当天就如实记在这里，而不是等修好之后再补。根治的办法是换一种 SQL 形态：把连续的行级算子并进同一层 `SELECT`。它会改变所有程序的 canonical SQL，所以作为一次工件版本升级（`artifact v2`）来做——那是[第 10 章](ch10-codegen融合.md)的内容。v2 下两万个连续 `filter` 的流水线只有两层嵌套，在同一个 DuckDB 上正常运行。

还有一个已知的小代价：每生成一层都要把内层的 SQL 复制一遍，n 层的总复制量是 O(n²)。现实中的流水线只有几十个算子，感觉不到；但心里要有数。

## 聚合的守卫

- `sum(i64)`：HUGEINT 中转 + 范围检查（同加法）+ 空集合 → NULL；
- `sum(f64)`/`avg`：isfinite 守卫 + `+ 0.0` 规范化；`avg = SUM/COUNT`（跳过 null 的 COUNT）；
- `min/max`：DuckDB 跳过 null 语义一致，直用；f64 只补零规范化。

## 信任边界（本 crate 的文档第一段）

写后端最容易犯的错是高估自己的承诺。本 crate 开头就写明：

> MVP 保证的是**证据链完整性和执行请求一致性**；DuckDB 的优化器会重写逻辑/物理计划，属于指定版本的信任基；**不得宣称已审 HIR 等于实际物理计划**。

这句话是第二轮评审的直接产物（review-r2 §r2-compiler），它决定了 manifest 里放什么：canonical SQL、引擎版本、引擎配置——**我们能绑定的东西**；而不是 DuckDB 的物理计划——我们绑不了的东西。

## 跑起来

```powershell
cargo test -p pebble-duckdb                 # 含「extend / join 直接接在源后面」的回归
cargo test --workspace --test long_pipeline # 在 256 KiB 的栈上给 2 000 个算子做 codegen
cargo test --workspace --test evidence_chain extend_and_join -- --nocapture   # 在真实 DuckDB 上跑一遍（缺 duckdb 时跳过）

# 亲手看「一个算子一层子查询」长什么样：
@'
Table("tests/data/orders.csv") as o
  .filter(o.amount > 1)
  .extend({ x = 1 })
  .collect()
'@ | Set-Content examples/shape.pebble
cargo run -q -p pebble-driver -- build examples/shape.pebble --locks tests/locks
Get-Content examples/target/pebble/shape/plan.sql
Remove-Item examples/shape.pebble

# 看守卫的实际产物：
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks
Select-String -Path examples/target/pebble/00_m0_revenue/plan.sql -Pattern "HUGEINT|IS NOT NULL|NULLS LAST"
```

## 练习

1. **边界测试**：写一个端到端测试：造一列 i64 数据让 `sum` 恰好溢出，断言结果是 NULL 而不是报错/饱和值（提示：`9223372036854775807` + `1`）。
2. **MOD 符号验证**：Pebble 规范说 `%` 结果符号随被除数。查 DuckDB 行为并写测试钉住；如果它随除数，你需要在守卫里补什么？（提示：`sign(r)*abs(x)%abs(r)` 模式）
3. **write 的 CSV null 表示**：DuckDB 默认把 NULL 写成空串。规范没定义 null 的 CSV 序列化——先在 06-operators 补规范，再改 codegen，体会「规范先行」的顺序。
4. **思考题**：如果把 `SET threads TO 1` 去掉，哪些现有测试会挂？哪些**永远不会**挂但语义已经悄悄不可复现？（提示：整数聚合 vs 浮点聚合）
5. **融合**：把连续的 `filter` 合并进同一层 `WHERE`。（这一题已经在[第 10 章](ch10-codegen融合.md)实现，可以先自己设计一遍，再对照。）动手前先想清楚三件事：canonical SQL 变了，对已经审批过的 manifest 意味着什么？第 4 章的流敏收缩会不会因为合并而改变？工件要不要升版本号？
6. **O(n²) 的复制**：`emit_all` 每生成一层都要复制内层的 SQL。改用一个显式栈，把各层的前缀和后缀直接写进同一个 `String`，让总代价变成 O(n)。（提示：只有一个输入的节点，输出的是「前缀 + 内层 + 后缀」。）
7. **找引擎的上限**：用二分法找出 DuckDB 能接受的最大连续算子数（v1 的实测结果见上一节；切回提交 `bebd87c` 可以自己复现）。编译器该不该在编译期就拒绝超过它的流水线？说说利弊。（提示：规范要求后端能力诊断使用独立类别，不能替代语言诊断。）
