# pebble-hir

职责：typed Logical HIR、AST → HIR lowering、Schema 附着、稳定 SourceId/FieldId、字段来源身份与 DAG 不变量校验。覆盖无 Lambda 的 `extend/select/join`、单条件 `if`、分组/全局聚合和顺序哨兵；可空性仅由 `Type::Opt` 表达。

同时负责规范拓扑编号、canonical HIR 序列化与版本化内容 hash。Span、注释和 HashMap 顺序不得进入摘要。

状态：目录骨架，暂不写实现。结构及 BackendArtifact/Manifest/Receipt 边界见 `docs/draft/design/hir.md`。
