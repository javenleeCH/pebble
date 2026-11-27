# Pebble 文档与阶段状态

> 当前权威入口是 [`design-decisions.md`](design-decisions.md) v3。
> **项目第一目标**：完整体验一门语言从立项、实现到落地；因此本仓库采用**教程驱动开发**——实现改动必须与 `tutorial/` 同步演进。
> 后端路线：DuckDB（M0 已实现）→ Polars（M3）→ Cranelift/MLIR 原生后端（M4）。

## 从 0 到 1 系列教程（教程驱动，一章一状态）

| 章 | 主题 | 位置 |
|---|---|---|
| 0 | 立项：rationale 与五条红线（含被否决的第一版定位） | [`tutorial/ch00-立项.md`](tutorial/ch00-立项.md) |
| 1 | 词法分析：token 模型与错误码 | [`tutorial/ch01-词法.md`](tutorial/ch01-词法.md) |
| 2 | 递归下降解析器与 AST | [`tutorial/ch02-解析与AST.md`](tutorial/ch02-解析与AST.md) |
| 3 | 诊断系统：错误码是稳定契约 | [`tutorial/ch03-诊断契约.md`](tutorial/ch03-诊断契约.md) |
| 4 | 类型系统与 P0 语义（null/Kleene/checked-null/流敏收缩） | [`tutorial/ch04-类型与语义.md`](tutorial/ch04-类型与语义.md) |
| 5 | HIR：字段身份、canonical 序列化与内容寻址 | [`tutorial/ch05-HIR与哈希.md`](tutorial/ch05-HIR与哈希.md) |
| 6 | 后端 codegen：不继承 DuckDB 默认行为 | [`tutorial/ch06-DuckDB语义守卫.md`](tutorial/ch06-DuckDB语义守卫.md) |
| 7 | 落地：build/run/verify 证据链闭环 | [`tutorial/ch07-证据链.md`](tutorial/ch07-证据链.md) |
| 8 | 解析器错误恢复：同步点、级联错误、阶段门控；Span 单位 bug 修复 | [`tutorial/ch08-解析器错误恢复.md`](tutorial/ch08-解析器错误恢复.md) |
| 9 | 语义层错误恢复：毒化，让坏消息静默传播；codegen 两个 M0 遗留缺陷的修复记在第 6 章 | [`tutorial/ch09-语义层错误恢复.md`](tutorial/ch09-语义层错误恢复.md) |

课程地图与开发纪律见 [`tutorial/README.md`](tutorial/README.md)。

## 定稿与评审

| 产物 | 位置 |
|---|---|
| v3 设计定稿：目标、边界、功能、语法、语义、后端路线 | [`design-decisions.md`](design-decisions.md) |
| 第一轮专家评审 | [`review-2026-11-23.md`](review-2026-11-23.md) |
| 第二轮专家复审 | [`review-r2-2026-11-23.md`](review-r2-2026-11-23.md) |
| 定位与取舍一页纸 | [`../rationale.md`](../rationale.md) |

## 语言规范（draft/spec/）

以下章节已按 v3 同步，仍属于原型前规范草案；实现必须通过规范测试后才固化为正式 Spec。

- `01-overview.md`：定位、边界、语义模型、红线、后端路线
- `02-lexical.md`：词法；无 Lambda `|param|`
- `04-expressions.md`：表达式、优先级、作用域、if、内置函数
- `05-type-system.md`：类型、Schema、Kleene、非空保证、checked-null、聚合签名
- `06-operators.md`：算子目录、全局聚合、MVP 状态
- `07-semantics.md`：唯一数据语义、顺序哨兵、值同一性、部分函数
- `08-diagnostics.md`：编译期与执行期诊断分类

## 内部设计（draft/design/）

- `ast.md`：无 Lambda AST、extend/select alias、分组/全局聚合
- `hir.md`：稳定字段身份、canonical HIR、BackendArtifact、Manifest/Receipt、多后端边界

## 文法与样例

- [`../grammar/pebble.ebnf`](../grammar/pebble.ebnf)：Draft v0.2
- [`../examples/`](../examples/)：6 个新语法样例及预期输出

## 流程纪律

1. 新特性先过 `rationale.md` 和五条红线。
2. 先写/改样例，再文法和语义规则，再实现；禁止用后端默认行为填补规范空白。
3. 每个保留语法必须有唯一类型与取值规则，并通过 null、溢出、空输入和作用域反例测试。
4. MVP 先闭环 DuckDB 证据链；多后端不得反向分裂语言语义。
5. Cranelift/MLIR 属 M4 正式原生后端路线，研究原型和基准可先行，但不阻塞 DuckDB 首发。
6. **教程驱动**：任何实现改动与对应教程章节同一 commit 更新（滞后不得超过一章）；commit 格式 `<type>(ch<N>-<crate>): 摘要`；每章结束时的代码必须 `cargo test` 全绿。