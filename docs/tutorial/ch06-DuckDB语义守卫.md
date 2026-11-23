# 第 6 章 后端 codegen：不继承 DuckDB 的默认行为

> 对应文件：`crates/pebble-duckdb/src/lib.rs`。本章回答一个被两轮评审反复拷问的问题：**把执行委托给外部引擎后，「审的那份」和「跑的那份」到底什么关系？**

## 本章目标

1. 学会把语言语义「翻译」而不是「直译」成目标 SQL；
2. 逐个实现语义守卫：判空、整数 checked、f64 规范化、round ties-to-even、排序 null 位置；
3. 明确信任边界：什么被证据链绑定，什么必须信任 DuckDB。

## 翻译，不是直译

最朴素的 codegen 是 `a + b` → `a + b`。问题出在语义差集上：

| Pebble 语义 | DuckDB 默认 | 后果 |
|---|---|---|
| `x != null` 恒 `bool` | `x != NULL` → NULL | 判空必须走 `IS NOT NULL` |
| 整数溢出 → null | 溢出 → **报错** | 必须加守卫 |
| `/0`、`%0` → null | 整除零报错 | 必须加守卫 |
| f64 非有限结果 → null | `1e308*10` → inf | 必须 isfinite 检查 |
| `-0.0` 规范化为 `+0.0` | 保留 -0.0 | 必须规范化 |
| `round` ties-to-even | DuckDB 四舍五入进位 | 必须手写公式 |
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

DuckDB 的 `round(2.5) = 3`，Pebble 承诺 `round(2.5) = 2`。手写：

```text
CASE WHEN frac(x) < 0.5 THEN FLOOR(x)          -- 远离 .5：直接取
     WHEN frac(x) > 0.5 THEN FLOOR(x) + 1
     WHEN MOD(CAST(FLOOR(x) AS BIGINT), 2) = 0 THEN FLOOR(x)   -- 恰在 .5：取偶
     ELSE FLOOR(x) + 1 END
```

注意第三行安全的边界条件：`frac = 0.5` 只可能出现在 `|x| < 2^53`（再大的浮点没有小数部分），所以 `CAST(FLOOR(x) AS BIGINT)` 不会溢出——**每个守卫都要自证安全性**，这条推论写进了 crate 文档。

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
cargo test -p pebble-duckdb
# 看守卫的实际产物：
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks
Select-String -Path examples/target/pebble/00_m0_revenue/plan.sql -Pattern "HUGEINT|IS NOT NULL|NULLS LAST"
```

## 练习

1. **边界测试**：写一个端到端测试：造一列 i64 数据让 `sum` 恰好溢出，断言结果是 NULL 而不是报错/饱和值（提示：`9223372036854775807` + `1`）。
2. **MOD 符号验证**：Pebble 规范说 `%` 结果符号随被除数。查 DuckDB 行为并写测试钉住；如果它随除数，你需要在守卫里补什么？（提示：`sign(r)*abs(x)%abs(r)` 模式）
3. **write 的 CSV null 表示**：DuckDB 默认把 NULL 写成空串。规范没定义 null 的 CSV 序列化——先在 06-operators 补规范，再改 codegen，体会「规范先行」的顺序。
4. **思考题**：如果把 `SET threads TO 1` 去掉，哪些现有测试会挂？哪些**永远不会**挂但语义已经悄悄不可复现？（提示：整数聚合 vs 浮点聚合）
