# 测试与验收

> 测试体系：`cargo test` 全绿（149 项）；证据链集成测试在真实 DuckDB 1.4.x 上执行。

## 单元测试（随 crate）

| crate | 覆盖 |
|---|---|
| `pebble-diagnostic` | Span 定位/渲染（含中文注释后行列精确、离谱 Span 不 panic、跨行只标注首行）、错误码稳定性（LEX/PAR/SEM/SCH/RUN）、诊断排序去重、`render_all` 汇总行、行号二分查找（CRLF 与多字节字符）、大量诊断时的显示上限 |
| `pebble-parser` | 全部样例可解析；LEX-001（`|`/`^`）、PAR-003~007、PAR-002（终端后接算子）、SEM-016（字面量越界）；`having` 与 `cast` 的文法（第 11 章，5 项：三种目标类型、八种写法都是 PAR-001、没跟括号的 `cast` 仍是字段、坏的 `cast` 只毁掉自己那个算子）；**错误恢复矩阵**（`tests/recovery.rs`，37 项）：多错误一次报告、三道同步点、词法洞不重复报错、残骸模式（恢复不制造诊断）、流水线层多余 token 与 PAR-002 之后继续解析、提示只说站得住的话、嵌套/长度上限不崩溃、词法边角（BOM、全角标点、隐形字符）、病态输入不死循环 |
| `pebble-hir` | lock 哈希稳定性与篡改检测、manifest/receipt 绑定、工件哈希确定性；`cast` 以标签 `V` 编码且该标签在全部编码中独一无二（第 11 章） |
| `pebble-semantics` | M0 lowering + 哈希确定性、注释不影响 hash（含中文注释）、语义变更改变 hash、流敏收缩、聚合签名、SEM/SCH 错误矩阵（001/003/004/005/007/008/010/011/012/014/016、SCH-001/002/004）；**多错误报告与毒化**（12 项）：每个用例断言完整的错误列表——独立元素逐个检查、毒化的名字不再报（裸名 / 别名限定 / 覆盖）、坏 select 仍裁剪作用域、缺 lock 时停止；**having 与 cast**（第 11 章，9 项）：参考实现的取值表、checked-null 类型规则、四种之外的转换报 SEM-003 且带提示、常量 cast 只在必然失败时诊断、常量内置调用不再误报、`having` 与 `filter` 哈希相同、位置 / 谓词 / 作用域 / 非空收缩逐项与 `filter` 一致 |
| `pebble-duckdb` | canonical SQL 前言（threads=1）、判空 IS NOT NULL、sum checked 守卫（HUGEINT 范围）、nulls last、f64 规范化、round ties-to-even；`extend` / `join` 直接接在源后面时的 SELECT 列表（回归：曾分别报「无来源」与生成 `SELECT , …`）；**融合规则**（第 10 章，7 项）：单个算子的流水线 SQL 正文与 v1 逐字相同、连续 `filter` 共用一个 WHERE、AND 树平衡（两万个谓词 15 层）、`filter` 只在引用的列来自 FROM 时并入、`select` / `sort` 并入而 `take` 不并、`take` / `distinct` / 聚合是屏障、`join` 层吸收其后的算子；第 11 章：紧跟聚合的 `filter` 成为 `HAVING`（子句顺序、聚合输出替换成表达式、只吸收紧跟的那个）、四种 `cast` 的守卫结构、`trim` 的 25 个 White_Space 码点 |
| `pebble-latex` | LaTeX 转义；长流水线的代数视图不需要调用栈（在 256 KiB 栈的线程里给 2000 个算子出视图）；`cast` 在审计视图里写成 `\mathrm{cast}_{目标类型}` |

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
4. ch08、ch09、ch10、ch11 的每个 rust 代码块都逐字出现在源码里（可用「（节选）」行断开，断开后每段仍须逐字一致）；
5. ch10、ch11 贴的生成的 SQL（`plan.sql`）与实现生成的逐字一致，只允许换行与缩进不同。

## 集成测试（pebble-driver/tests/evidence_chain.rs）

1. **all_examples_compile**：examples/ 全部 7 个样例编译通过（含 lock 绑定）。注意它只证明「能编译」——能执行由第 8、9 项和端到端测试证明。
2. **build_is_deterministic**：重复编译 → hir/artifact/manifest 三哈希一致。
3. **verify_binds_receipt_to_manifest_and_detects_tamper**：一致 receipt → APPROVED；篡改 receipt 工件哈希 / 篡改 manifest 本体 → VERIFY FAILED。
4. **run_without_duckdb_writes_failed_receipt**：引擎缺失 → RUN-003 receipt；工件绑定仍成立。
5. **end_to_end_demo_when_duckdb_present**：00_m0_revenue 端到端，输出行核对（c2/c6、过滤与 null 语义生效）→ verify APPROVED。
6. **toctou_data_tamper_fails_run002_when_duckdb_present**：build 后篡改数据文件 → run 拒绝执行（RUN-002）。
7. **nonfinite_f64_input_fails_run001_when_duckdb_present**：f64 列含 inf → RUN-001 预检拒绝。
8. **extend_and_join_directly_on_source_run_when_duckdb_present**：`extend` / `join` 作为第一个算子的程序在真实 DuckDB 上执行，逐行核对输出（回归，见第 6 章）。
9. **fused_layers_match_a_plain_calculation_when_duckdb_present**：融合后的 SQL 在真实 DuckDB 上的结果，与测试里直接从 CSV 算出的一致（第三种独立算法，第 10 章）。含四个程序：`filter` + `extend` 并层、`filter` 紧跟算出新列的 `extend`、`join` 层吸收后续算子、`take` 之后的 `filter`（屏障：去掉它会多出一行），以及两千个 `filter` 的长流水线。

## 长流水线回归（pebble-duckdb/tests/long_pipeline.rs）

**codegen_of_a_long_pipeline_needs_no_call_stack_and_two_layers**：给 2000 个 `filter` 的流水线做 codegen。两个检查：
- 在一个只有 256 KiB 栈的线程里做——递归实现会栈溢出（测试进程崩溃），迭代实现不需要栈（第 6 章）；
- 生成的 SQL 只有两层嵌套、一个 WHERE，2000 个谓词一个不少（第 10 章）。

## 语义对拍（pebble-driver/tests/differential.rs）

语言规则的参考实现（语义层里的几十行 Rust）与真实 DuckDB 上执行的 SQL 必须给出相同的结果。缺 duckdb 时跳过。

1. **cast_matrix_on_duckdb_matches_the_reference_implementation**：`tests/data/casts.csv` 的 25 行边界值 × 7 列，逐格等于 `pebble_semantics::cast`。
2. **string_builtins_on_duckdb_match_the_spec_except_known_gaps**：`trim` / `length` / `contains` 逐格等于规范；`lower` / `upper` 有三处写明的已知差集（简单映射 vs 全映射），测试钉住现状。
3. **round_and_abs_on_duckdb_match_rust**：`round` 的取偶公式（含 `4503599627370492.5` 这种走取偶分支的大数）与 `abs` 等于 Rust 标准库。
4. **having_keeps_the_same_groups_as_a_filter_after_agg**：`having` 的结果等于紧跟 `agg` 的 `filter`，并等于直接从 CSV 数出来的。
5. **existing_programs_keep_their_hir_hash**：样例 00 的 HIR 哈希写死在这里。给语言加特性不得改变既有程序的哈希。

## 验收规则（docs/design-decisions.md §7）

- 同一计划重复编译哈希一致（测试 2）；
- 语义变更后旧审批失效、注释/格式变更不失效（CLI 四幕演示 + 测试 2/3）；
- 数据被篡改可被发现（测试 6）；
- 非有限输入在表达式求值前被拒绝（测试 7）；
- 保留语法在 null / 部分函数边界有唯一行为（semantics 单测 + duckdb SQL 守卫）。

## fixtures

- `locks/`：全部样例与 demo 的 Schema lock（`pebble lock` 生成）。
- `data/orders.csv`：M0 demo 数据（含 null、refund/open 过滤、f64 列）。
- `data/casts.csv`：`cast` 的对拍矩阵（字符串的各种写法、取偶与越界的边界、精度丢失）。
- `data/strings.csv`：内置字符串函数的对拍数据（制表符、全角空格、不换行空格、零宽字符、特殊大小写映射）。
- `fixtures/diagnostics/`：故意写错的源文件，供教程第 8、9 章与诊断集成测试使用（**请勿修复**）。
- `fixtures/codegen/fusion_demo.pebble`：教程第 10 章的演示文件（四个行级算子并成两层 SELECT）；教程里贴的 `plan.sql` 必须与它生成的逐字一致；同目录的 `having_demo.pebble`、`cast_demo.pebble` 是第 11 章的演示文件。
- 运行 DuckDB 依赖：PATH 或 `tools/duckdb(.exe)`（测试在缺失时自动跳过端到端部分）。
