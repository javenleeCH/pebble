# 01 语言总览

> 本章已同步 `docs/design-decisions.md` v3。与其他旧资料冲突时，以设计定稿及本轮同步后的规范为准。

## 1.1 定位

Pebble 是面向**离线批结构化数据**的可信变换 DSL 与嵌入式编译器 SDK，专注过滤、派生、投影、关联和分组聚合。

- **不做通用编程语言**：无通用控制流、无一等函数、MVP 无任意 UDF。
- **语法即数据流**：阅读顺序与数据流向一致。
- **静态可校验**：字段、类型、作用域和 Schema 在编译期检查。
- **证据链可核对**：源码、Schema lock、canonical HIR、BackendArtifact 和 ExecutionReceipt 可绑定验证。
- **可数学化**：逻辑计划可导出关系代数/LaTeX 可读视图；该视图不是执行结果正确性证明。

Pebble 不与 DuckDB 比速度、不与 Polars 比算子数量，也不把非图灵完备当作资源安全或沙箱。

## 1.2 适用边界

### 适合

- 离线批过滤、派生、投影、关联、分组聚合。
- 需要唯一静态语义、计划审计和执行工件核对的变换。
- 受监管报表、可复现实验和受限用户变换的前端校验。

### 不适合

- 实时无限流、业务状态机和通用递归算法。
- 任意 IO、网络请求和用户自定义副作用。
- 异构文本抽取（归属 UniExtract）。
- MVP 阶段的窗口、pivot、unnest、完整时间/通用 cast 分析。

## 1.3 核心语义模型

### 数据流 DAG

程序是由算子节点和数据依赖边组成的 DAG；当前语法形态为单条线性流水线，join 可引入额外源节点。节点携带稳定 FieldId/SourceId，避免重名列和跨后端名称解析漂移。

算子优化必须遵守语义：`sort/take` 是顺序哨兵，自由重排只适用于无顺序依赖的代数片段。

### 受控效应

- filter/extend/select/join/group_by+agg 等变换核心无用户可见任意副作用。
- Table/MemoryTable 是显式输入能力，经 Schema lock 或宿主不可变 Schema 校验。
- write/collect/print 是受控终端效应，只能位于流水线末尾。
- IO、OOM、取消和资源配额耗尽是执行状态，不得伪装成表达式 null。

### 有限性与终止性

语言无用户递归、循环或无界迭代；所有迭代由内置有限集合算子完成。该性质支持终止论证，但**不承诺有界时间/内存**。资源控制由后续宿主 SDK、配额和后端能力共同实现。

### 唯一数据语义

- 普通运算采用严格 null 传播；判空和强 Kleene 逻辑是显式例外。
- 普通比较返回 `?bool`；`x == null`、`x != null` 恒返回 `bool`。
- filter、join on、if 只认 `true`，`false/null` 均不成立。
- 整数溢出、除零等定义域失败返回类型化 null，不抛用户异常、不饱和。
- group/distinct 使用独立值同一性；sort 固定 nulls last。

完整规则见 04/05/07 章。

## 1.4 五条设计红线

1. **总性**：无用户递归/循环/无界迭代；数据源有限，终止不冒充资源安全。
2. **一阶纯函数**：无一等/高阶函数；MVP 无任意 UDF。
3. **线性数据流**：单入口单出口；if 只能值选择，不能选择流水线。
4. **受控效应**：输入输出只能经显式能力边界。
5. **语义封闭**：表达式取值和算子行为唯一确定，禁止「由实现定义」。

## 1.5 程序外观

```pebble
// 单行注释
Table("users.parquet") as u
  .join(
    Table("orders.parquet") as o,
    on = u.id == o.uid,
  )
  .filter(u.age >= 18 && o.amount != null)
  .extend({
    full_name = u.first + " " + u.last,
    birth_year = 2026 - u.age,
  })
  .group_by(u.first, u.last,)
  .agg(
    order_count = count(),
    total_amount = sum(o.amount),
  )
  .filter(total_amount >= 1000)
  .select(first, last, order_count, total_amount,)
  .sort(total_amount desc,)
  .take(100)
  .write("top_users.csv")
```

注意：`group_by + agg` 是单一聚合节点，agg 输入仍含 `o.amount`；agg 后只能引用分组键和聚合输出。示例仅作语法外观展示，具体输出字段须满足各算子的可见性与重名规则。

## 1.6 后端路线

统一边界是 canonical typed Logical HIR，每个后端生成独立、可审计的 BackendArtifact：

1. **M0/M1：DuckDB SQL 首发**，闭环 Schema lock、Manifest、Receipt、Verify。
2. ~~**M3：Polars 第二后端**~~（已取消，2026-11-28）：语义差集与一致性测试由「DuckDB vs 差分参考实现」承担，MLIR 后端落地后扩展对拍。
3. **M4：Cranelift/MLIR 原生后端**，引入 Pebble Physical Plan；Cranelift 负责低延迟表达式/算子 JIT，MLIR 探索跨算子融合、向量化与多目标 lowering。

外部引擎后端把指定版本 DuckDB 纳入信任基；不得宣称已审 HIR 等同于外部引擎实际物理计划。MLIR 原生后端则相反：物理计划由 Pebble 生成，可绑定哈希——届时该限制按新信任边界改写。