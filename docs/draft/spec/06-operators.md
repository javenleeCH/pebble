# 06 内置算子目录

算子分四类：**源算子**（流水线起点）、**变换算子**（纯、无副作用）、**聚合算子**、**终端算子**（唯一允许副作用）。

> 语法定稿以 `docs/design-decisions.md` v3 为准：保留 `.` 链、`map→extend`、废 Lambda、`on =`、`select as`、统一尾逗号。
> `join`/`distinct`/`write`/`print` 保留在草案，标记为 **MVP 硬化候选**；`having` 与受限 `cast` 自 v0.1.1 起启用（见 04 §4.12、本节 6.4）；时间函数/`coalesce`/`split` 仍后置。

## 6.1 算子总览

| 类别 | 算子 | 关系代数 | 是否纯变换 | MVP 状态 |
|---|---|---|---|---|
| 源 | `Table` | 关系 R | — | 稳定则保留 |
| 源 | `MemoryTable` | 关系 R | — | MVP |
| 变换 | `filter` | 选择 σ | 是 | MVP |
| 变换 | `extend` | 扩展投影 ε | 是 | MVP |
| 变换 | `select` | 投影 π | 是 | MVP |
| 变换 | `distinct` | 去重 δ | 是 | 硬化候选 |
| 变换 | `sort` | — | 是 | MVP |
| 变换 | `take` | — | 是 | MVP |
| 变换 | `join` | 连接 ⋈ | 是 | 硬化候选 |
| 聚合 | `group_by` | 分组 γ | 是 | MVP |
| 聚合 | `agg` | 聚合 γ | 是 | MVP |
| 聚合 | `having` | 选择 σ（作用于分组结果） | 是 | v0.1.1 |
| 终端 | `write` | — | 否（副作用） | 硬化候选 |
| 终端 | `collect` | — | 否（回传宿主） | MVP |
| 终端 | `print` | — | 否（标准输出） | 硬化候选 |

## 6.2 源算子

### `Table(path)`

```pebble
Table("users.parquet") as u
```

| 项 | 说明 |
|---|---|
| 参数 | `path: string`，本地结构化文件路径 |
| 输出 Schema | 来自文件元信息 / schema lock |
| 约束 | 数据源必须是有限集合；MVP 限单文件 |

### `MemoryTable(name)`

```pebble
MemoryTable("users") as u
```

| 项 | 说明 |
|---|---|
| 参数 | `name: string`，宿主程序传入的内存表名 |
| 输出 Schema | 来自宿主注册的不可变表元信息 |
| 约束 | 用于与宿主内存表对接；Schema 需版本化 |

**别名规则**：`as <alias>` 声明源别名，用于多源/自引用或需要明确字段来源的表达式。单源流水线中可直接使用裸字段；多源/join 后同名字段必须用别名限定，裸字段只允许在可见 Schema 中名称唯一时解析，否则报歧义。

## 6.3 变换算子

### `filter(pred)`

```pebble
.filter(age >= 18 && o.amount > 0)
```

| 项 | 说明 |
|---|---|
| 参数 | 返回 `bool` 或 `?bool` 的表达式 |
| 语义 | 保留谓词为 `true` 的行；`false` 或 `null` 均不保留 |
| 关系代数 | σ_pred(R) |
| Schema | 与输入一致；流敏非空收缩规则见 §5.3 |

### `extend(record)`

```pebble
.extend({
  full_name = first + " " + last,
  birth_year = 2026 - age,
})
```

| 项 | 说明 |
|---|---|
| 参数 | 记录字面量：`{ 字段 = 表达式, ... }` |
| 语义 | 所有 RHS 同时对输入行求值，叠加或覆盖到输入行；兄弟绑定互不可见 |
| 关系代数 | ε_proj(R)（扩展算子，非投影 π） |
| Schema | 见类型系统章节；覆盖后旧值不再按原名可见 |

### `select(item...)`

```pebble
.select(id, full_name as name, birth_year as year,)
```

| 项 | 说明 |
|---|---|
| 参数 | 一个或多个 `expr [as ident]`；无 `as` 项必须是字段引用 |
| 语义 | 仅保留列出字段，顺序按参数顺序；`as` 为派生表达式或字段重命名 |
| 关系代数 | π_cols(R) |
| Schema | 仅含输出字段；被裁字段不得再经源别名访问 |

### `distinct()`（硬化候选）

```pebble
.distinct()
```

| 项 | 说明 |
|---|---|
| 参数 | 无 |
| 语义 | 按整行去重；null 与自身同一 |
| 关系代数 | δ(R) |
| Schema | 与输入一致 |

### `sort(sortKey...)`

```pebble
.sort(total_amount desc, name asc,)
```

| 项 | 说明 |
|---|---|
| 参数 | 排序键列表，每键可带 `asc`（默认）/`desc`；允许尾逗号 |
| 语义 | 按键字典序/数值序排序；所有键统一 `nulls last` |
| Schema | 与输入一致 |

### `take(n)`

```pebble
.take(100)
```

| 项 | 说明 |
|---|---|
| 参数 | `n: i64`，非负整数 |
| 语义 | 取前 N 行；通常置于 `sort` 之后 |
| Schema | 与输入一致 |

### `join(other, on = cond)`（硬化候选）

```pebble
.join(
  Table("orders.parquet") as o,
  on = u.id == o.uid,
)
```

| 项 | 说明 |
|---|---|
| 参数 | 右表源算子；`on` 是固定绑定名，值为返回 `bool` 或 `?bool` 的连接表达式 |
| 语义 | 内连接两表；仅保留 `on = true` 的行对，普通等值 join 不匹配 null 键 |
| 关系代数 | R ⋈_cond S |
| Schema | 保留左右字段来源身份；同名字段必须别名限定或进入 `select` 前显式改名 |
| 约束 | 右表必须是源算子，不得是上游算子结果；MVP 仅内连接 |

## 6.4 聚合算子

### `group_by(cols...)`

```pebble
.group_by(first, last,)
```

| 项 | 说明 |
|---|---|
| 参数 | 分组键字段列表；允许尾逗号 |
| 语义 | 按键分组，等待后续 `agg`；分组键使用值同一性（null 同一） |
| Schema | 不单独产生可观察的输出 Schema；与后续 `agg` 合并为单一聚合节点，输入仍保留完整行字段 |
| 约束 | 必须且只能由 `agg` 接续 |

### `agg(bindings...)`

```pebble
.agg(
  order_count = count(),
  total_amount = sum(o.amount),
)
```

| 项 | 说明 |
|---|---|
| 参数 | 一组 `字段 = 聚合调用` 绑定；允许尾逗号 |
| 聚合函数 | `count` / `sum` / `min` / `max` / `avg` |
| 语义 | 对每个分组或全局组计算聚合值 |
| Schema | 分组键 ∪ 聚合绑定字段；全局 agg 时仅含聚合绑定字段 |
| 约束 | 可直接接在任意上游算子后表示全局聚合；`group_by + agg` 视为单个聚合节点；聚合函数不得出现在其他算子中 |
| 空输入 | 分组聚合：零行；全局聚合：恰好一行，`count()`=0，`sum/min/max/avg` 为 null |

### `having(pred)`（v0.1.1）

```pebble
.group_by(o.customer_id)
.agg(order_count = count(), revenue = sum(o.amount))
.having(revenue >= 10000)
```

| 项 | 说明 |
|---|---|
| 参数 | 返回 `bool` 或 `?bool` 的表达式；解析域是聚合节点的输出（分组键与聚合绑定） |
| 语义 | 与紧随其后的普通 `filter` **完全相同**：保留谓词为 `true` 的分组；非空收缩规则也相同 |
| 约束 | 必须紧跟在 `agg` 之后，否则 SEM-005 |
| Lowering | 实现把它 lowering 成与 `filter` 相同的 HIR 结构：`.agg(…).having(p)` 与 `.agg(…).filter(p)` 的 HIR 哈希相同 |

`having` 是语法糖。Pebble 的 `filter` 本来就能作用于聚合输出（不像 SQL 的 `WHERE` 看不到聚合值），所以它不引入新的语义，只是让「对分组结果过滤」的意图在源码里更醒目。后端可以把这个过滤发射为 SQL 的 `HAVING`。
## 6.5 终端算子

### `write(path)`（硬化候选）

```pebble
.write("top_users.csv")
```

| 项 | 说明 |
|---|---|
| 参数 | `path: string`，输出文件路径 |
| 语义 | 写入本地文件（MVP：CSV） |
| 副作用 | 文件 IO；受控效应边界 |

### `collect()`

```pebble
.collect()
```

| 项 | 说明 |
|---|---|
| 参数 | 无 |
| 语义 | 结果收集到内存，回传宿主程序 |
| 副作用 | 仅内存回传 |

### `print()`（硬化候选）

```pebble
.print()
```

| 项 | 说明 |
|---|---|
| 参数 | 无 |
| 语义 | 打印结果到标准输出（调试用） |
| 副作用 | 标准输出 |

## 6.6 流水线位置约束

1. 流水线以源算子开头，以终端算子结尾。
2. 终端算子之后不得再接续任何算子。
3. `group_by` 之后只能接 `agg`；`agg` 可直接接在任意上游算子后表示全局聚合。
4. `join` 右表必须是源算子。
5. `recordLiteral` 只允许作为 `extend` 的参数。
6. `having` 必须紧跟在 `agg` 之后；它等价于作用于聚合输出的 `filter`。
7. 变换算子不得产生外部 IO；除终端算子外全部为纯变换。
