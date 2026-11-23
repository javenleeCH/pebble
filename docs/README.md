# Pebble 文档与阶段状态

> 当前权威入口是 [`design-decisions.md`](design-decisions.md) v3。
> 两轮专家评审后，规范、文法、AST/HIR 和样例已完成一轮同步；原型实现尚未开始。
> 后端路线：DuckDB（MVP）→ Polars（M3）→ Cranelift/MLIR 原生后端（M4）。

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