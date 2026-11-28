//! 教程契约门禁（教程驱动开发）：
//! 本仓库承诺「实现与 docs/tutorial/ 同步演进」。此测试把承诺变成 CI 硬门禁：
//! - 教程目录结构完整（课程地图 + 各章五段结构）；
//! - 教程引用的样例 / 文法 / lock / 数据文件真实存在；
//! - README 的快速上手引用真实路径。
//!
//! 这不是「文档存在性」的形式检查：章节清单与 docs/tutorial/README.md 的
//! 章节地图是一份受测契约——改教程结构必须同时改这里，防止教程悄然失配。
//! 第 8 章起再加一道：教程「跑起来」里贴的输出，必须与实现对同一输入的真实报告逐字一致。
//! 第 9 章起代码引用的逐字检查按章节表登记；第 10 章起再加：教程里贴的生成的 SQL
//! 必须与实现生成的 plan.sql 逐字一致（只允许换行与缩进不同）。

use pebble_driver::cmd_build;
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
    "ch08-解析器错误恢复.md",
    "ch09-语义层错误恢复.md",
    "ch10-codegen融合.md",
    "ch11-having与cast.md",
    "ch12-签名与verifier.md",
    "ch13-资源配额.md",
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

/// 读取仓库里的文本文件，并把行尾统一成 LF。
///
/// 行尾属于检出环境，不属于内容：GitHub 的 Windows runner 以 `core.autocrlf=true` 检出，
/// 同一份提交在那里是 CRLF、在 Linux 上是 LF。门禁若不先统一行尾，就会出现
/// 「本地绿、CI 红」——第 8 章的输出比对就这样栽过一次：CRLF 下围栏后的换行是 `\r\n`，
/// 只剥 `\n` 的清理没起作用，于是比对面多出了一个空行。
fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e))
        .replace("\r\n", "\n")
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
    // ch00–ch08 引用的实现资产必须真实存在
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
        "tests/fixtures/diagnostics/multi_error.pebble",
        "tests/fixtures/diagnostics/multi_semantic_error.pebble",
        "tests/fixtures/codegen/fusion_demo.pebble",
        "tests/fixtures/codegen/having_demo.pebble",
        "tests/fixtures/codegen/cast_demo.pebble",
        "tests/data/casts.csv",
        "tests/data/strings.csv",
        "tests/locks/casts_csv.lock.json",
        "tests/locks/strings_csv.lock.json",
        // ch12：签名与独立 verifier
        "crates/pebble-hir/src/sign.rs",
        "crates/pebble-verifier/Cargo.toml",
        "crates/pebble-verifier/src/lib.rs",
        "crates/pebble-verifier/src/main.rs",
        "crates/pebble-verifier/tests/verifier.rs",
        // ch13：资源配额与威胁模型
        "crates/pebble-driver/src/budget.rs",
        "crates/pebble-driver/tests/budget.rs",
        "docs/threat-model.md",
        "crates/pebble-driver/tests/differential.rs",
        "crates/pebble-parser/tests/recovery.rs",
        "crates/pebble-driver/tests/diagnostics.rs",
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

/// 抽取 Markdown 中「`marker` 之后第一个语言为 `fence` 的代码块」的内容。
/// 紧贴代码围栏的换行属于排版，不算内容。行尾无论是 LF 还是 CRLF，结果都一样。
fn fenced_block_after(chapter: &str, marker: &str, fence: &str) -> String {
    // 先统一行尾：CRLF 下围栏之后的换行是 `\r\n`，只剥 `\n` 的清理会留下一个 `\r`
    let chapter = chapter.replace("\r\n", "\n");
    let (_, after) = chapter
        .split_once(marker)
        .unwrap_or_else(|| panic!("教程缺少标记文本「{marker}」"));
    let opening = format!("```{fence}\n");
    let block = after
        .split(&opening)
        .nth(1)
        .unwrap_or_else(|| panic!("「{marker}」之后应有 {opening:?} 开头的代码块"));
    block.split("```").next().unwrap_or("").to_string()
}

/// 教程「跑起来」里贴的输出（```text 代码块）。
fn output_block_after(chapter: &str, marker: &str) -> String {
    fenced_block_after(chapter, marker, "text")
}

/// 这段输出比对曾经「本地绿、CI 红」：GitHub 的 Windows runner 以 CRLF 检出，
/// 抽出来的文本开头多了一个换行。抽取函数因此必须与行尾无关——用两种行尾各测一次。
#[test]
fn output_block_extractor_is_line_ending_agnostic() {
    let marker = "最后一条命令的完整输出：";
    let lf = format!("前言\n\n{marker}\n\n```text\nline 1\nline 2\n```\n\n后记\n");
    let crlf = lf.replace('\n', "\r\n");
    assert_eq!(output_block_after(&lf, marker), "line 1\nline 2\n");
    assert_eq!(output_block_after(&crlf, marker), "line 1\nline 2\n");
    // 围栏语言是抽取条件的一部分：要 sql 就不能拿到 text 块；紧贴围栏的换行不算内容
    let mixed = format!("{marker}\n\n```sql\nSELECT 1;\n```\n\n```text\nnot this\n```\n");
    assert_eq!(fenced_block_after(&mixed, marker, "sql"), "SELECT 1;\n");
}

/// 「报告输出逐字一致」的章节：`docs/tutorial/` 下的文件名 → 它「跑起来」用的（故意写错的）演示文件。
/// 新增一章带编译报告的教程时，在这里登记一行。
const REPORT_CHAPTERS: &[(&str, &str)] = &[
    ("ch08-解析器错误恢复.md", "multi_error.pebble"),
    ("ch09-语义层错误恢复.md", "multi_semantic_error.pebble"),
];

/// 「代码引用逐字一致」的章节：这些章节里的每个 rust 代码块都必须出现在源码里。
const QUOTE_CHAPTERS: &[&str] = &[
    "ch08-解析器错误恢复.md",
    "ch09-语义层错误恢复.md",
    "ch10-codegen融合.md",
    "ch11-having与cast.md",
];

/// 「生成的 SQL 逐字一致」的条目：（章节文件名, 教程里代码块之前的标记文本, 演示的 Pebble 源文件）。
/// 一章可以登记多段：每段用自己的标记文本定位。
const SQL_CHAPTERS: &[(&str, &str, &str)] = &[
    (
        "ch10-codegen融合.md",
        "最后一条命令生成的 plan.sql（只调整了换行和缩进）：",
        "tests/fixtures/codegen/fusion_demo.pebble",
    ),
    (
        "ch11-having与cast.md",
        "having 程序生成的 plan.sql（只调整了换行和缩进）：",
        "tests/fixtures/codegen/having_demo.pebble",
    ),
    (
        "ch11-having与cast.md",
        "cast 程序生成的 plan.sql（只调整了换行和缩进）：",
        "tests/fixtures/codegen/cast_demo.pebble",
    ),
];

/// 教程贴的输出必须与实现逐字一致。
///
/// 章节里「最后一条命令的完整输出」展示的是 `pebble build` 对演示文件的完整报告。
/// 那段输出一旦与实现不一致，读者照着教程跑就会看到别的东西——所以让它成为被测对象：
/// 消息、提示、汇总行任何一处改动，都必须回来同步教程（或者反过来，教程改错了这里会红灯）。
fn assert_tutorial_output_matches(chapter: &str, fixture: &str) {
    let repo = repo();
    let text = read(repo.join("docs").join("tutorial").join(chapter));
    let shown = output_block_after(&text, "最后一条命令的完整输出：");

    let out = std::env::temp_dir()
        .join("pebble-it")
        .join("docs-gate")
        .join(fixture);
    let _ = std::fs::remove_dir_all(&out);
    let failure = match cmd_build(
        &repo.join("tests/fixtures/diagnostics").join(fixture),
        &repo.join("tests").join("locks"),
        &out,
        None,
        None,
    ) {
        Ok(_) => panic!("{fixture} 是故意写错的，不应编译成功"),
        Err(f) => f,
    };
    assert!(
        !out.exists(),
        "编译失败时不得写出任何工件：{}",
        out.display()
    );
    assert_eq!(
        failure.report.trim_end(),
        shown.trim_end(),
        "{chapter} 教程里的输出已与实现不一致：请同步两者"
    );
}

/// 读取 `dir` 下全部 `.rs` 源文件（跳过构建输出目录）；行尾已由 `read` 统一成 LF。
fn rust_sources(dir: &PathBuf, out: &mut Vec<String>) {
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("读取 {} 失败: {}", dir.display(), e))
    {
        let path = entry.expect("entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "target" {
                rust_sources(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(read(path));
        }
    }
}

/// 教程引用的代码必须与实现一致（教程纪律第 5 条：「过期即视为 bug」）。
///
/// 章节里的每一个 ```rust 代码块都必须逐字出现在某个源文件里。
/// 代码块里可以用含「（节选）」的行断开；断开之后的每一段仍然必须逐字出现——
/// 所以「节选」只能用来省略，不能用来改写。
fn assert_rust_quotes_are_verbatim(chapter: &str, min_blocks: usize) {
    let repo = repo();
    let chapter_text = read(repo.join("docs").join("tutorial").join(chapter));
    let mut sources = Vec::new();
    rust_sources(&repo.join("crates"), &mut sources);
    assert!(!sources.is_empty(), "没有读到任何 .rs 源文件");

    let mut blocks: Vec<&str> = Vec::new();
    let mut rest = chapter_text.as_str();
    while let Some((_, after)) = rest.split_once("```rust\n") {
        let Some((body, after)) = after.split_once("\n```") else {
            break;
        };
        blocks.push(body);
        rest = after;
    }
    assert!(
        blocks.len() >= min_blocks,
        "应能从 {chapter} 抽出至少 {min_blocks} 个 rust 代码块，实际 {}",
        blocks.len()
    );

    for (i, block) in blocks.iter().enumerate() {
        // 按「（节选）」所在的行切成若干段
        let mut pieces: Vec<String> = vec![String::new()];
        for line in block.lines() {
            if line.contains("（节选）") {
                pieces.push(String::new());
            } else {
                let last = pieces.last_mut().expect("至少一段");
                last.push_str(line);
                last.push('\n');
            }
        }
        assert!(
            pieces.len() <= 3,
            "{chapter} 第 {} 个代码块的节选标记过多（{} 处）——教程应当直接引用实现",
            i + 1,
            pieces.len()
        );
        for piece in &pieces {
            if piece.trim().is_empty() {
                continue;
            }
            assert!(
                sources.iter().any(|s| s.contains(piece.as_str())),
                "{chapter} 第 {} 个 rust 代码块里有一段在源码中找不到——实现变了，教程没跟上？\n   {}",
                i + 1,
                piece.lines().next().unwrap_or("").trim()
            );
        }
    }
}

/// 教程贴的**生成的 SQL** 必须与实现逐字一致（只允许换行与缩进不同）。
///
/// 读者是照着这段 SQL 学 codegen 的。它一旦与实现不一致，教程讲的就不是编译器真正生成的东西。
/// 为了可读性，教程里会手工折行、缩进；所以比较之前把两边的空白全部去掉。
fn assert_tutorial_sql_matches(chapter: &str, marker: &str, demo: &str) {
    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }
    let repo = repo();
    let text = read(repo.join("docs").join("tutorial").join(chapter));
    let shown = fenced_block_after(&text, marker, "sql");

    let out = std::env::temp_dir()
        .join("pebble-it")
        .join("docs-gate-sql")
        .join(PathBuf::from(demo).file_stem().unwrap());
    let _ = std::fs::remove_dir_all(&out);
    cmd_build(
        &repo.join(demo),
        &repo.join("tests").join("locks"),
        &out,
        None,
        None,
    )
    .unwrap_or_else(|e| panic!("{demo} 应当能编译: {e:?}"));
    let actual = read(out.join("plan.sql"));
    assert_eq!(
        squash(&shown),
        squash(&actual),
        "{chapter} 教程里贴的 plan.sql 已与实现不一致：请同步两者"
    );
}

#[test]
fn tutorial_output_matches_implementation() {
    for (chapter, fixture) in REPORT_CHAPTERS {
        assert_tutorial_output_matches(chapter, fixture);
    }
}

#[test]
fn tutorial_rust_code_quotes_are_verbatim() {
    for chapter in QUOTE_CHAPTERS {
        assert_rust_quotes_are_verbatim(chapter, 5);
    }
}

#[test]
fn tutorial_generated_sql_matches_implementation() {
    for (chapter, marker, demo) in SQL_CHAPTERS {
        assert_tutorial_sql_matches(chapter, marker, demo);
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
