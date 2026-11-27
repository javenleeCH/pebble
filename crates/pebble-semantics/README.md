# pebble-semantics

职责：Schema 自动推导、类型检查、字段/别名/来源身份解析与算子位置校验。落实严格 null 传播、Kleene 逻辑、判空特殊形式、流敏非空收缩、checked-null 部分函数、分组/全局聚合及聚合后作用域；可空性只使用 `Type::Opt`。

编译只信任 Schema lock/宿主注册的不可变 Schema，运行时元数据仅用于一致性验证。语义结果 lowering 为 typed canonical HIR，不得由 DuckDB、Polars 或未来原生后端自行补齐语言规则。

多错误报告（第 9 章）：算子里相互独立的部分（extend 的绑定、select 的项、agg 的键与绑定、sort 的键）逐个检查；定义失败的名字被毒化——它的输出位置留一个占位，之后任何对它的引用都沉默地失败，不再重复报错。失败的算子不发射，流水线带着占位继续，所以下游独立的错误照常报出。任何 Error 存在时不产出 HIR。唯一的致命错误是数据源缺少 Schema lock。

having 与受限 cast（第 11 章）：`having` 是语法糖，lowering 成紧跟聚合的 `filter`（HIR 哈希相同），位置不对报 SEM-005。`cast` 只允许四种转换；可能失败的转换结果一律可空。取值规则的参考实现在公开模块 `pebble_semantics::cast`，常量诊断（SEM-016）与端到端对拍都用它。常量表达式里的内置函数按规范求值。

状态：已实现（M0 + 第 9 章）。类型与推导规则见 `docs/draft/spec/05-type-system.md`；教程见 `docs/tutorial/ch04-类型与语义.md`、`docs/tutorial/ch09-语义层错误恢复.md`。
