# 测试与验收

> M0 测试体系：`cargo test` 全绿；证据链集成测试在真实 DuckDB 1.4.x 上执行。

## 单元测试（随 crate）

| crate | 覆盖 |
|---|---|
| `pebble-diagnostic` | Span 定位/渲染、错误码稳定性（LEX/PAR/SEM/SCH/RUN） |
| `pebble-parser` | 全部样例可解析；LEX-001（`|`/`^`）、PAR-003~007、PAR-002（终端后接算子）、SEM-016（字面量越界） |
| `pebble-hir` | lock 哈希稳定性与篡改检测、manifest/receipt 绑定、工件哈希确定性 |
| `pebble-semantics` | M0 lowering + 哈希确定性、注释不影响 hash、语义变更改变 hash、流敏收缩、聚合签名、SEM/SCH 错误矩阵（001/003/004/005/007/008/010/011/012/014/016、SCH-001/002/004） |
| `pebble-duckdb` | canonical SQL 前言（threads=1）、判空 IS NOT NULL、sum checked 守卫（HUGEINT 范围）、nulls last、f64 规范化、round ties-to-even |
| `pebble-latex` | LaTeX 转义 |

## 集成测试（pebble-driver/tests/evidence_chain.rs）

1. **all_examples_compile**：examples/ 全部 7 个样例编译通过（含 lock 绑定）。
2. **build_is_deterministic**：重复编译 → hir/artifact/manifest 三哈希一致。
3. **verify_binds_receipt_to_manifest_and_detects_tamper**：一致 receipt → APPROVED；篡改 receipt 工件哈希 / 篡改 manifest 本体 → VERIFY FAILED。
4. **run_without_duckdb_writes_failed_receipt**：引擎缺失 → RUN-003 receipt；工件绑定仍成立。
5. **end_to_end_demo_when_duckdb_present**：00_m0_revenue 端到端，输出行核对（c2/c6、过滤与 null 语义生效）→ verify APPROVED。
6. **toctou_data_tamper_fails_run002_when_duckdb_present**：build 后篡改数据文件 → run 拒绝执行（RUN-002）。
7. **nonfinite_f64_input_fails_run001_when_duckdb_present**：f64 列含 inf → RUN-001 预检拒绝。

## 验收规则（docs/design-decisions.md §7）

- 同一计划重复编译哈希一致（测试 2）；
- 语义变更后旧审批失效、注释/格式变更不失效（CLI 四幕演示 + 测试 2/3）；
- 数据被篡改可被发现（测试 6）；
- 非有限输入在表达式求值前被拒绝（测试 7）；
- 保留语法在 null / 部分函数边界有唯一行为（semantics 单测 + duckdb SQL 守卫）。

## fixtures

- `locks/`：全部样例与 demo 的 Schema lock（`pebble lock` 生成）。
- `data/orders.csv`：M0 demo 数据（含 null、refund/open 过滤、f64 列）。
- 运行 DuckDB 依赖：PATH 或 `tools/duckdb(.exe)`（测试在缺失时自动跳过端到端部分）。
