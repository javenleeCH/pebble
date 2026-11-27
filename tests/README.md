# 测试与验收

> 测试体系：`cargo test` 全绿（115 项）；证据链集成测试在真实 DuckDB 1.4.x 上执行。

## 单元测试（随 crate）

| crate | 覆盖 |
|---|---|
| `pebble-diagnostic` | Span 定位/渲染（含中文注释后行列精确、离谱 Span 不 panic、跨行只标注首行）、错误码稳定性（LEX/PAR/SEM/SCH/RUN）、诊断排序去重、`render_all` 汇总行、行号二分查找（CRLF 与多字节字符）、大量诊断时的显示上限 |
| `pebble-parser` | 全部样例可解析；LEX-001（`|`/`^`）、PAR-003~007、PAR-002（终端后接算子）、SEM-016（字面量越界）；**错误恢复矩阵**（`tests/recovery.rs`，37 项）：多错误一次报告、三道同步点、词法洞不重复报错、残骸模式（恢复不制造诊断）、流水线层多余 token 与 PAR-002 之后继续解析、提示只说站得住的话、嵌套/长度上限不崩溃、词法边角（BOM、全角标点、隐形字符）、病态输入不死循环 |
| `pebble-hir` | lock 哈希稳定性与篡改检测、manifest/receipt 绑定、工件哈希确定性 |
| `pebble-semantics` | M0 lowering + 哈希确定性、注释不影响 hash（含中文注释）、语义变更改变 hash、流敏收缩、聚合签名、SEM/SCH 错误矩阵（001/003/004/005/007/008/010/011/012/014/016、SCH-001/002/004）；**多错误报告与毒化**（12 项）：每个用例断言完整的错误列表——独立元素逐个检查、毒化的名字不再报（裸名 / 别名限定 / 覆盖）、坏 select 仍裁剪作用域、缺 lock 时停止 |
| `pebble-duckdb` | canonical SQL 前言（threads=1）、判空 IS NOT NULL、sum checked 守卫（HUGEINT 范围）、nulls last、f64 规范化、round ties-to-even；`extend` / `join` 直接接在源后面时的 SELECT 列表（回归：曾分别报「无来源」与生成 `SELECT , …`） |
| `pebble-latex` | LaTeX 转义；长流水线的代数视图不需要调用栈（在 256 KiB 栈的线程里给 2000 个算子出视图） |

## 诊断集成测试（pebble-driver/tests/diagnostics.rs）

用户看到的是 `CompileFailure.report` 的全文，这些测试对它做端到端断言：

1. **multi_error_report_lists_every_error_in_source_order**：教程演示文件（`tests/fixtures/diagnostics/multi_error.pebble`）报出 3 个错误，按源码顺序，提示与汇总行逐项核对；且不写出任何工件。
2. **recovered_program_is_not_semantically_checked**：解析阶段恢复过 → 只报语法错误，不混入语义错误（阶段门控）。
3. **semantic_error_is_located_after_chinese_comments**：中文注释之后的语义错误定位准确（此前会漂移甚至 panic）。
4. **environment_failure_is_a_single_pebble_message**：环境问题退出码 1、单行 `[pebble]` 消息。
5. **thousands_of_errors_render_promptly**：5 000 个错误的文件远低于分钟级完成；最多显示 100 条，汇总行给出真实总数。
6. **semantic_errors_are_all_reported_without_cascades**：两处拼写错误一次报完；引用了失败名字的算子不重复报错，提示里也不推荐它。

## 教程契约门禁（pebble-driver/tests/docs_gate.rs）

教程驱动开发的机器化检查（附录 A）：

1. 章节清单与课程地图一致，每章含五段；
2. 教程引用的实现资产、样例与 lock 真实存在；
3. ch08、ch09「跑起来」贴的输出与实现对同一文件的报告**逐字**一致；
4. ch08、ch09 的每个 rust 代码块都逐字出现在源码里（可用「（节选）」行断开，断开后每段仍须逐字一致）。

## 集成测试（pebble-driver/tests/evidence_chain.rs）

1. **all_examples_compile**：examples/ 全部 7 个样例编译通过（含 lock 绑定）。注意它只证明「能编译」——能执行由第 8 项和端到端测试证明。
2. **build_is_deterministic**：重复编译 → hir/artifact/manifest 三哈希一致。
3. **verify_binds_receipt_to_manifest_and_detects_tamper**：一致 receipt → APPROVED；篡改 receipt 工件哈希 / 篡改 manifest 本体 → VERIFY FAILED。
4. **run_without_duckdb_writes_failed_receipt**：引擎缺失 → RUN-003 receipt；工件绑定仍成立。
5. **end_to_end_demo_when_duckdb_present**：00_m0_revenue 端到端，输出行核对（c2/c6、过滤与 null 语义生效）→ verify APPROVED。
6. **toctou_data_tamper_fails_run002_when_duckdb_present**：build 后篡改数据文件 → run 拒绝执行（RUN-002）。
7. **nonfinite_f64_input_fails_run001_when_duckdb_present**：f64 列含 inf → RUN-001 预检拒绝。
8. **extend_and_join_directly_on_source_run_when_duckdb_present**：`extend` / `join` 作为第一个算子的程序在真实 DuckDB 上执行，逐行核对输出（回归，见第 6 章）。

## 长流水线回归（pebble-duckdb/tests/long_pipeline.rs）

**codegen_of_a_long_pipeline_needs_no_call_stack**：在一个只有 256 KiB 栈的线程里，给 2000 个算子的流水线做 codegen。
递归实现会栈溢出（测试进程崩溃），迭代实现不需要栈。编译器不再因流水线过长而崩溃。

（引擎一侧的上限是另一回事：DuckDB 实测连续约 300 个算子以上无法执行，见第 6 章。）

## 验收规则（docs/design-decisions.md §7）

- 同一计划重复编译哈希一致（测试 2）；
- 语义变更后旧审批失效、注释/格式变更不失效（CLI 四幕演示 + 测试 2/3）；
- 数据被篡改可被发现（测试 6）；
- 非有限输入在表达式求值前被拒绝（测试 7）；
- 保留语法在 null / 部分函数边界有唯一行为（semantics 单测 + duckdb SQL 守卫）。

## fixtures

- `locks/`：全部样例与 demo 的 Schema lock（`pebble lock` 生成）。
- `data/orders.csv`：M0 demo 数据（含 null、refund/open 过滤、f64 列）。
- `fixtures/diagnostics/`：故意写错的源文件，供教程第 8 章与诊断集成测试使用（**请勿修复**）。
- 运行 DuckDB 依赖：PATH 或 `tools/duckdb(.exe)`（测试在缺失时自动跳过端到端部分）。
