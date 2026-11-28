//! 第 8 章：`pebble build` 的诊断输出即契约。
//!
//! 用户看到的是 `CompileFailure.report` 的全文。这里对它做端到端断言：
//! 错误的数量、顺序、行列、汇总行，以及「编译失败时不写出任何工件」。

use pebble_driver::{CompileFailure, cmd_build};
use std::path::{Path, PathBuf};
use std::sync::Once;

static CWD: Once = Once::new();

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root")
}

fn set_repo_cwd() {
    CWD.call_once(|| {
        std::env::set_current_dir(repo()).expect("设置测试 CWD 为仓库根");
    });
}

/// 编译一个必然失败的源文件，返回失败报告；并断言没有写出任何工件。
fn build_failure(src: &Path, name: &str) -> CompileFailure {
    set_repo_cwd();
    let out = std::env::temp_dir().join("pebble-it").join(name);
    let _ = std::fs::remove_dir_all(&out);
    let failure = match cmd_build(src, &repo().join("tests").join("locks"), &out, None, None) {
        Ok(_) => panic!("{} 不应编译成功", src.display()),
        Err(f) => f,
    };
    assert!(
        !out.exists(),
        "编译失败时不得写出任何工件，但 {} 存在",
        out.display()
    );
    failure
}

/// 把一段源码写到仓库内的临时目录（数据路径按 CWD = 仓库根解析）。
fn scratch_source(name: &str, src: &str) -> PathBuf {
    let dir = repo().join("target").join("pebble-it-diag").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(format!("{name}.pebble"));
    std::fs::write(&path, src).expect("write scratch source");
    path
}

/// 报告里所有 `┌─ 文件:行:列` 位置行——即用户看到的错误顺序。
fn positions(report: &str) -> Vec<String> {
    report
        .lines()
        .filter(|l| l.contains("┌─ "))
        .map(str::to_string)
        .collect()
}

#[test]
fn multi_error_report_lists_every_error_in_source_order() {
    // 教程「跑起来」一节展示的就是这份文件；此处断言与教程里的输出逐项对应
    let f = build_failure(
        &repo().join("tests/fixtures/diagnostics/multi_error.pebble"),
        "diag-multi",
    );
    assert_eq!(f.code, 2, "编译期诊断的退出码是 2");
    let report = &f.report;

    // 三个错误，按源码顺序（而不是「先词法、后语法」的发现顺序）。
    // 行列别手数——以 fixture 为准：`.select` 在第 9 行，`)` 是该行第 37 个字符。
    assert_eq!(
        positions(report),
        [
            "  ┌─ multi_error.pebble:4:30",
            "  ┌─ multi_error.pebble:9:3",
            "  ┌─ multi_error.pebble:9:37",
        ],
        "{report}"
    );
    assert_eq!(report.matches("error[").count(), 3, "{report}");
    assert!(report.contains("error[LEX-001]"), "{report}");
    assert_eq!(report.matches("error[PAR-001]").count(), 2, "{report}");
    // 没有被吞掉的级联错误，也没有混进语义错误
    assert!(!report.contains("SEM-"), "{report}");

    // 修复提示（同时覆盖两种成因：少了 `)` / 写成了方法调用）
    assert!(report.contains("逻辑与写作 `&&`"), "{report}");
    assert!(report.contains("前面的 `(` 缺少 `)`"), "{report}");

    // 汇总行收尾
    assert!(
        report.ends_with("编译中止：共 3 个错误（LEX 1，PAR 2）\n"),
        "{report}"
    );
}

#[test]
fn recovered_program_is_not_semantically_checked() {
    // 一个语法错误 + 一个拼错的字段：解析阶段恢复过，语义阶段就不运行。
    // 于是只报语法错误——「字段不存在」此时只是残骸的后果，报出来反而是噪音。
    let src = "// 中文注释在前\nTable(\"tests/data/orders.csv\") as o\n  .select(o.customer_id, o.nosuch_field + )\n  .collect()\n";
    let path = scratch_source("gate", src);
    let f = build_failure(&path, "diag-gate");
    assert_eq!(f.code, 2);
    assert_eq!(f.report.matches("error[").count(), 1, "{}", f.report);
    assert!(f.report.contains("error[PAR-001]"), "{}", f.report);
    assert!(
        f.report.ends_with("编译中止：共 1 个错误（PAR 1）\n"),
        "{}",
        f.report
    );
}

#[test]
fn semantic_error_is_located_after_chinese_comments() {
    // Span 改用字节偏移前，这个文件会把错误渲染到错误的位置，甚至让编译器 panic
    let src = "// 审批要点：只保留已支付订单\n// 第二行中文注释\nTable(\"tests/data/orders.csv\") as o\n  .filter(o.statusx == \"paid\")\n  .collect()\n";
    let path = scratch_source("nonascii", src);
    let f = build_failure(&path, "diag-nonascii");
    assert_eq!(f.code, 2);
    // 标注的是整个字段引用 `o.statusx`：从第 4 行第 11 列（`o`）开始
    assert_eq!(
        positions(&f.report),
        ["  ┌─ nonascii.pebble:4:11"],
        "{}",
        f.report
    );
    assert!(f.report.contains("error[SEM-001]"), "{}", f.report);
    assert!(
        f.report.ends_with("编译中止：共 1 个错误（SEM 1）\n"),
        "{}",
        f.report
    );
}

/// `needle` 在 `src` 中第一次出现处的 `文件:行:列`（期望值不靠手数）。
fn at(src: &str, needle: &str) -> String {
    let start = src
        .find(needle)
        .unwrap_or_else(|| panic!("测试源码里找不到 {needle:?}"));
    let before = &src[..start];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    format!("semantic.pebble:{line}:{col}")
}

/// 语义阶段的多错误报告（第 9 章）：独立的错误一次报完；
/// 引用了一个失败名字的下游算子不重复报错（毒化）。
#[test]
fn semantic_errors_are_all_reported_without_cascades() {
    // 第 3 行拼错 `amount`，第 4 行用到了它产生的 `total`（后果，不该再报），第 5 行又拼错一个字段
    let src = concat!(
        "Table(\"tests/data/orders.csv\") as o\n",
        "  .extend({ total = o.amont + 1 })\n",
        "  .filter(total > 0)\n",
        "  .filter(o.statuz == \"paid\")\n",
        "  .collect()\n",
    );
    let path = scratch_source("semantic", src);
    let f = build_failure(&path, "diag-semantic");
    assert_eq!(f.code, 2);
    assert_eq!(
        positions(&f.report),
        [
            format!("  ┌─ {}", at(src, "o.amont")),
            format!("  ┌─ {}", at(src, "o.statuz")),
        ],
        "{}",
        f.report
    );
    assert_eq!(f.report.matches("error[").count(), 2, "{}", f.report);
    assert!(
        !f.report.contains("`total`"),
        "total 是失败定义的后果，不应被报: {}",
        f.report
    );
    // 提示里的「当前可见字段」也不该把它当成可用字段推荐出来
    for hint in f.report.lines().filter(|l| l.contains("提示:")) {
        assert!(
            !hint.contains("total"),
            "提示不应推荐一个定义失败的名字: {hint}"
        );
    }
    assert!(
        f.report.ends_with("编译中止：共 2 个错误（SEM 2）\n"),
        "{}",
        f.report
    );
}

/// 几千个错误也要很快出结果。
///
/// 独立评审测得：以前一行里上万个非法字符、或者两万行各有笔误的文件，编译一分钟以上
/// 都出不来——每渲染一条诊断都把整个源码从头扫一遍，代价是「诊断数 × 文件大小」。
/// 现在行号靠二分查找，显示有条数上限。
#[test]
fn thousands_of_errors_render_promptly() {
    let mut src = String::from("Table(\"tests/data/orders.csv\") as o\n");
    for _ in 0..5000 {
        src.push_str("  .filter(o.amount > )\n");
    }
    src.push_str("  .collect()\n");
    let path = scratch_source("many", &src);
    let started = std::time::Instant::now();
    let f = build_failure(&path, "diag-many");
    let elapsed = started.elapsed();

    assert!(
        f.report.ends_with("编译中止：共 5000 个错误（PAR 5000）\n"),
        "{}",
        f.report
    );
    assert_eq!(
        f.report.matches("error[").count(),
        pebble_diagnostic::MAX_RENDERED
    );
    // 不设精确的时间断言（机器快慢不同）；只要求远低于评审测到的分钟级
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "5000 个错误的编译用了 {elapsed:?}"
    );
}

#[test]
fn environment_failure_is_a_single_pebble_message() {
    set_repo_cwd();
    let out = std::env::temp_dir().join("pebble-it").join("diag-missing");
    let _ = std::fs::remove_dir_all(&out);
    let f = match cmd_build(
        &repo().join("tests/fixtures/diagnostics/no-such-file.pebble"),
        &repo().join("tests").join("locks"),
        &out,
        None,
        None,
    ) {
        Ok(_) => panic!("不存在的源文件不应编译成功"),
        Err(f) => f,
    };
    assert_eq!(f.code, 1, "环境问题的退出码是 1");
    assert!(
        f.report.starts_with("[pebble] 读取源文件失败"),
        "{}",
        f.report
    );
    assert!(!out.exists());
}
