//! 教程契约门禁（教程驱动开发）：
//! 本仓库承诺「实现与 docs/tutorial/ 同步演进」。此测试把承诺变成 CI 硬门禁：
//! - 教程目录结构完整（课程地图 + 各章五段结构）；
//! - 教程引用的样例 / 文法 / lock / 数据文件真实存在；
//! - README 的快速上手引用真实路径。
//!
//! 这不是「文档存在性」的形式检查：章节清单与 docs/tutorial/README.md 的
//! 章节地图是一份受测契约——改教程结构必须同时改这里，防止教程悄然失配。

use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root")
}

/// 章节地图（与 docs/tutorial/README.md 同步维护；新增章节时两个文件一起改）。
const CHAPTERS: &[&str] = &[
    "ch00-立项.md",
    "ch01-词法.md",
    "ch02-解析与AST.md",
    "ch03-诊断契约.md",
    "ch04-类型与语义.md",
    "ch05-HIR与哈希.md",
    "ch06-DuckDB语义守卫.md",
    "ch07-证据链.md",
    "appendix-ci-发布门禁.md",
];

/// 每章必含的五段（教程纪律第 4 条）。例外：ch00 是立项章，无代码可走读，
/// 豁免「代码走读」——豁免本身写在受测契约里，防止例外悄悄扩散。
const REQUIRED_SECTIONS: &[&str] = &["本章目标", "为什么", "代码走读", "跑起来", "练习"];

fn required_sections_for(chapter: &str) -> Vec<&'static str> {
    if chapter.starts_with("ch00") {
        REQUIRED_SECTIONS
            .iter()
            .filter(|s| **s != "代码走读")
            .copied()
            .collect()
    } else {
        REQUIRED_SECTIONS.to_vec()
    }
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e))
}

fn assert_exists(path: PathBuf, what: &str) {
    assert!(
        path.exists(),
        "教程契约违反：{} 缺失（{}）",
        what,
        path.display()
    );
}

#[test]
fn tutorial_chapters_exist_and_well_formed() {
    let tutorial = repo().join("docs").join("tutorial");
    assert_exists(tutorial.join("README.md"), "教程课程地图 README.md");
    for ch in CHAPTERS {
        let path = tutorial.join(ch);
        assert_exists(path.clone(), &format!("教程章节 {ch}"));
        let text = read(path);
        for section in required_sections_for(ch) {
            assert!(
                text.contains(section),
                "{ch} 缺少必需段落「{section}」（教程纪律：每章五段）"
            );
        }
    }
    // 课程地图必须列出每一章（链接存在性）
    let map = read(tutorial.join("README.md"));
    for ch in CHAPTERS {
        assert!(
            map.contains(ch),
            "docs/tutorial/README.md 章节地图缺少 {ch}；新增章节时必须同步地图与本测试的 CHAPTERS"
        );
    }
}

#[test]
fn tutorial_referenced_assets_exist() {
    // ch00–ch07 引用的实现资产必须真实存在
    let repo = repo();
    let assets = [
        "rationale.md",
        "grammar/pebble.ebnf",
        "docs/design-decisions.md",
        "docs/review-2026-11-23.md",
        "docs/review-r2-2026-11-23.md",
        "docs/draft/spec/02-lexical.md",
        "docs/draft/spec/08-diagnostics.md",
        "docs/draft/design/ast.md",
        "crates/pebble-parser/src/lexer.rs",
        "crates/pebble-parser/src/parser.rs",
        "crates/pebble-parser/src/ast.rs",
        "crates/pebble-diagnostic/src/lib.rs",
        "crates/pebble-semantics/src/lib.rs",
        "crates/pebble-hir/src/lib.rs",
        "crates/pebble-hir/src/canonical.rs",
        "crates/pebble-hir/src/lock.rs",
        "crates/pebble-hir/src/manifest.rs",
        "crates/pebble-duckdb/src/lib.rs",
        "crates/pebble-driver/src/lib.rs",
        "crates/pebble-driver/src/main.rs",
        "crates/pebble-driver/tests/evidence_chain.rs",
        "examples/00_m0_revenue.pebble",
        "examples/01_filter_extend_sort.pebble",
        "examples/02_join_group_agg.pebble",
        "examples/03_conditional_projection.pebble",
        "examples/04_select_distinct.pebble",
        "examples/05_aggregate_stats.pebble",
        "examples/06_memory_table.pebble",
        "tests/locks/orders_csv.lock.json",
        "tests/data/orders.csv",
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        "crates/pebble-driver/tests/docs_gate.rs",
    ];
    for a in assets {
        assert_exists(repo.join(a), &format!("教程引用的资产 {a}"));
    }
    // README 快速上手指向的路径必须真实（README 即门禁）
    let readme = read(repo.join("README.md"));
    for path in [
        "examples/00_m0_revenue.pebble",
        "tests/locks",
        "docs/tutorial/README.md",
    ] {
        assert!(readme.contains(path), "README 未引用关键路径 {path}");
        assert_exists(repo.join(path), &format!("README 引用的 {path}"));
    }
}

#[test]
fn every_example_has_a_lock_binding() {
    // 样例可编译的前提：每个被引用的数据源在 tests/locks 有对应 lock。
    // （SCH-001 是运行时契约；这里提前在 CI 层挡住「样例没 lock」的提交。）
    let repo = repo();
    let locks = std::fs::read_dir(repo.join("tests").join("locks")).expect("locks 目录");
    let mut lock_identities: Vec<String> = Vec::new();
    for entry in locks.flatten() {
        let text = read(entry.path());
        if let Some(locator) = extract_json_string(&text, "locator") {
            lock_identities.push(format!("table:{locator}"));
        }
        if let Some(name) = extract_json_string(&text, "name") {
            if text.contains("\"memory_table\"") {
                lock_identities.push(format!("memory:{name}"));
            }
        }
    }
    for ex in [
        "00_m0_revenue",
        "01_filter_extend_sort",
        "02_join_group_agg",
        "03_conditional_projection",
        "04_select_distinct",
        "05_aggregate_stats",
        "06_memory_table",
    ] {
        let text = read(repo.join("examples").join(format!("{ex}.pebble")));
        for line in text.lines() {
            let line = line.trim();
            let Some(arg) = source_arg_of(line) else {
                continue;
            };
            if line.starts_with("Table(") {
                assert!(
                    lock_identities.contains(&format!("table:{arg}")),
                    "样例 {ex} 引用 Table(\"{arg}\") 但 tests/locks 无对应 lock（SCH-001）"
                );
            } else if line.starts_with("MemoryTable(") {
                assert!(
                    lock_identities.contains(&format!("memory:{arg}")),
                    "样例 {ex} 引用 MemoryTable(\"{arg}\") 但 tests/locks 无对应 lock（SCH-001）"
                );
            }
        }
    }
}

/// 从 `Table("x")` / `MemoryTable("y")` 行提取首个字符串参数。
fn source_arg_of(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("Table(")
        .or_else(|| line.strip_prefix("MemoryTable("))?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 朴素 JSON 字符串字段提取（lock 文件结构简单，不值得引 JSON 依赖到测试）。
fn extract_json_string(text: &str, field: &str) -> Option<String> {
    let pat = format!("\"{field}\": \"");
    let start = text.find(&pat)? + pat.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
