# pebble-semantics

职责：Schema 自动推导、类型检查、字段/别名/来源身份解析与算子位置校验。落实严格 null 传播、Kleene 逻辑、判空特殊形式、流敏非空收缩、checked-null 部分函数、分组/全局聚合及聚合后作用域；可空性只使用 `Type::Opt`。

编译只信任 Schema lock/宿主注册的不可变 Schema，运行时元数据仅用于一致性验证。语义结果 lowering 为 typed canonical HIR，不得由 DuckDB、Polars 或未来原生后端自行补齐语言规则。

状态：目录骨架，暂不写实现。类型与推导规则草稿见 `docs/draft/spec/05-type-system.md`。
