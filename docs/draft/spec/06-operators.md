# 06 内置算子目录

算子分四类：**源算子**（流水线起点）、**变换算子**（纯、无副作用）、**聚合算子**、**终端算子**（唯一允许副作用）。

## 6.1 算子总览

| 类别 | 算子 | 关系代数 | 是否纯变换 |
|---|---|---|---|
| 源 | `Table` | 关系 R | — |
| 源 | `MemoryTable` | 关系 R | — |
| 变换 | `filter` | 选择 σ | 是 |
| 变换 | `map` | 投影 π | 是 |
| 变换 | `select` | 投影 π | 是 |
| 变换 | `distinct` | 去重 δ | 是 |
| 变换 | `sort` | — | 是 |
| 变换 | `take` | — | 是 |
| 变换 | `join` | 连接 ⋈ | 是 |
| 聚合 | `group_by` | 分组 γ | 是 |
| 聚合 | `agg` | 聚合 γ | 是 |
| 终端 | `write` | — | 否（副作用） |
| 终端 | `collect` | — | 否（回传宿主） |
| 终端 | `print` | — | 否（标准输出） |

## 6.2 源算子

### `Table(path)`

```pebble
Table("users.parquet") as u
```

| 项 | 说明 |
|---|---|
| 参数 | `path: string`，本地结构化文件路径 |
| 输出 Schema | 来自文件元信息 |
| 约束 | 数据源必须是有限集合 |

### `MemoryTable(name)`

```pebble
MemoryTable("users") as u
```

| 项 | 说明 |
|---|---|
| 参数 | `name: string`，宿主程序传入的内存表名 |
| 输出 Schema | 来自宿主注册的表元信息 |
| 约束 | 用于与 UniExtract 内存表对接 |

**别名规则**：`as <alias>` 为源声明行别名；后续谓词、投影、`join on` 通过别名限定字段。单源流水线中 Lambda 形参名可与别名不同，形参绑定当前行。

## 6.3 变换算子

### `filter(pred)`

```pebble
.filter(|u| u.age >= 18)
```

| 项 | 说明 |
|---|---|
| 参数 | Lambda，返回 `bool` |
| 语义 | 保留谓词为真的行 |
| 关系代数 | σ_pred(R) |
| Schema | 与输入一致 |

### `map(proj)`

```pebble
.map(|u| { full_name = u.first + " " + u.last })
```

| 项 | 说明 |
|---|---|
| 参数 | Lambda，函数体为记录字面量 |
| 语义 | 逐行计算投影字段，叠加覆盖到输入行 |
| 关系代数 | π_proj(R) |
| Schema | 见 [05-type-system.md](05-type-system.md) 5.3 |

### `select(...)`

```pebble
.select(u.id, u.name)
```

| 项 | 说明 |
|---|---|
| 参数 | 一个或多个字段表达式 |
| 语义 | 仅保留指定字段，顺序按参数顺序 |
| 关系代数 | π_cols(R) |
| Schema | 仅含列出字段 |

### `distinct()`

```pebble
.distinct()
```

| 项 | 说明 |
|---|---|
| 参数 | 无 |
| 语义 | 按整行去重 |
| 关系代数 | δ(R) |
| Schema | 与输入一致 |

### `sort(sortKey...)`

```pebble
.sort(total_amount desc)
.sort(age asc, name asc)
```

| 项 | 说明 |
|---|---|
| 参数 | 排序键列表，每键可带 `asc`（默认）/ `desc` |
| 语义 | 按键字典序 / 数值序排序 |
| Schema | 与输入一致 |

### `take(n)`

```pebble
.take(100)
```

| 项 | 说明 |
|---|---|
| 参数 | `n: i64`，正整数 |
| 语义 | 取前 N 行；通常置于 `sort` 之后 |
| Schema | 与输入一致 |

### `join(other, on: cond)`

```pebble
.join(Table("orders.parquet") as o, on: u.id == o.uid)
```

| 项 | 说明 |
|---|---|
| 参数 | 右表源算子；`on` 连接条件（返回 `bool`） |
| 语义 | 内连接两表 |
| 关系代数 | R ⋈_cond S |
| Schema | 左行字段 ∪ 右行字段；重名字段必须别名限定 |
| 约束 | 右表必须是源算子，不得是上游算子结果；v1 仅内连接 |

## 6.4 聚合算子

### `group_by(cols...)`

```pebble
.group_by(category)
```

| 项 | 说明 |
|---|---|
| 参数 | 分组键字段列表 |
| 语义 | 按键分组，等待后续 `agg` |
| Schema | 输出行仅含分组键 |
| 约束 | 必须且只能由 `agg` 接续 |

### `agg(bindings...)`

```pebble
.agg(
  order_count = count(),
  total_amount = sum(o.amount)
)
```

| 项 | 说明 |
|---|---|
| 参数 | 一组 `字段 = 聚合调用` 绑定 |
| 聚合函数 | `count` / `sum` / `min` / `max` / `avg` |
| 语义 | 对每个分组计算聚合值 |
| Schema | 分组键 ∪ 聚合绑定字段 |
| 约束 | 只能紧跟 `group_by`；聚合函数不得出现在其他算子中 |

## 6.5 终端算子

### `write(path)`

```pebble
.write("top_users.csv")
```

| 项 | 说明 |
|---|---|
| 参数 | `path: string`，输出文件路径 |
| 语义 | 写入本地文件（v1：CSV） |
| 副作用 | 文件 IO |

### `collect()`

```pebble
.collect()
```

| 项 | 说明 |
|---|---|
| 参数 | 无 |
| 语义 | 结果收集到内存，回传宿主程序 |
| 副作用 | 仅内存回传 |

### `print()`

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
3. `agg` 只能紧跟 `group_by`；`group_by` 之后只能接 `agg`。
4. `join` 右表必须是源算子。
5. 变换算子不得产生外部 IO；除终端算子外全部为纯变换。
