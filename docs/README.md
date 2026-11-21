# Pebble 文档与阶段状态

> 流程：rationale → 样例 → 文法草案 → MVP 原型 → 规范固化 → 内测收敛 → 白皮书 → 发布。
> 正式 Spec 只收录原型跑通、验证稳定的特性；当前仓库内所有规范类文件均为**草稿**。

## 当前阶段产物（阶段 1–3）

| 阶段 | 产物 | 位置 |
|---|---|---|
| 1. 定位取舍 | Rationale 一页纸：定位、用户、取舍拍板、MVP 边界、红线 | [`../rationale.md`](../rationale.md) |
| 2. 样例先行 | 6 个 `.pebble` 样例 + 同名 `.expected` 预期输出 | [`../examples/`](../examples/) |
| 3. 文法草案 | EBNF 草稿，原型阶段允许随时改 | [`../grammar/pebble.ebnf`](../grammar/pebble.ebnf) |
| 4. 原型骨架 | crate 目录占位，暂不写实现 | [`../crates/`](../crates/)、[`../tests/`](../tests/) |

## 草稿设计笔记（docs/draft/，未定稿）

原型验证前不固化，仅供实现期对照；后续按流程分批修订并入正式 Spec。

### spec/（语言规范草稿）

- `01-overview.md` 定位、边界、语义模型、红线
- `02-lexical.md` 词法规范
- `04-expressions.md` 表达式、优先级、Lambda、if 表达式
- `05-type-system.md` 类型系统与 Schema 推导
- `06-operators.md` 算子目录
- `07-semantics.md` 语义规则
- `08-diagnostics.md` 错误分类与错误码

### design/（内部结构草案）

- `ast.md` AST 节点定义草案
- `hir.md` HIR DAG 结构与 AST → HIR 映射草案

## 流程纪律

1. 新特性先过 `rationale.md`：不符合定位就砍。
2. 先写 / 改样例，再文法，再实现；原型阶段推翻设计属于正常迭代。
3. 原型跑通前，不在此目录沉淀对外正式规范。
