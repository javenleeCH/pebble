//! 语义对拍：语言规则的「参考实现」与真实 DuckDB 上执行的 SQL，必须给出相同的结果。
//!
//! 两份实现是相互独立的：参考实现是语义层里的几十行 Rust（`pebble_semantics::cast`、
//! 标准库的字符串函数）；被 DuckDB 执行的是后端生成的、带守卫的 SQL。
//! 规范（04、07 章）是它们共同的依据——哪一边偏离了规范，这里都会红。
//! 缺 duckdb 时跳过。

use pebble_driver::{cmd_build, cmd_run, cmd_verify};
use pebble_semantics::cast;
use std::collections::BTreeMap;
use std::path::PathBuf;
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

fn locks_dir() -> PathBuf {
    repo().join("tests").join("locks")
}

fn duckdb_available() -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let exe = dir.join(if cfg!(windows) {
                "duckdb.exe"
            } else {
                "duckdb"
            });
            if exe.exists() {
                return true;
            }
        }
    }
    let tools = repo().join("tools");
    tools.join("duckdb.exe").exists() || tools.join("duckdb").exists()
}

/// 最小 CSV 读取：字段可以被双引号包住，字段内的双引号写成两个。
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' && cur.is_empty() {
            in_quotes = true;
        } else if c == ',' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

fn read_data(name: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(repo().join("tests").join("data").join(name))
        .unwrap()
        .lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(split_csv)
        .collect()
}

/// 编译并执行一个以 `write` 结尾的程序，返回写出的 CSV 的数据行（已去掉表头）。
/// 源码里的 `{csv}` 会被替换成输出文件的路径。
fn run_to_csv(name: &str, src: &str) -> Vec<String> {
    // 每个测试用自己的子目录：测试是并行运行的，共用目录会互相删掉对方刚写出的文件
    let csv_rel = format!("target/pebble-it-diff/{name}/out.csv");
    let tmp = repo().join("target").join("pebble-it-diff").join(name);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("program.pebble");
    std::fs::write(&path, src.replace("{csv}", &csv_rel)).unwrap();
    let out = tmp.join(name);
    cmd_build(&path, &locks_dir(), &out, None).expect("build");
    let outcome = cmd_run(&out, None).expect("run");
    assert!(outcome.ok, "{name}: {:?}", outcome.output_summary);
    assert!(cmd_verify(&out.join("manifest.json"), &outcome.receipt_path).is_ok());
    std::fs::read_to_string(repo().join(&csv_rel))
        .unwrap()
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// 一个期望的输出格。f64 按位比较：±0 在 Pebble 里是同一个数值（都规范化为 +0.0），
/// 参考实现给出的永远是 +0.0；NaN 不可能出现。
#[derive(Debug, Clone)]
enum Cell {
    Null,
    I(i64),
    F(f64),
    S(String),
}

fn opt_i(v: Option<i64>) -> Cell {
    v.map(Cell::I).unwrap_or(Cell::Null)
}

fn opt_f(v: Option<f64>) -> Cell {
    v.map(Cell::F).unwrap_or(Cell::Null)
}

fn matches_cell(got: &str, want: &Cell) -> bool {
    match want {
        Cell::Null => got.is_empty(),
        Cell::I(v) => got.parse::<i64>().ok() == Some(*v),
        Cell::F(v) => got
            .parse::<f64>()
            .ok()
            .is_some_and(|g| g.to_bits() == v.to_bits()),
        Cell::S(s) => got == s,
    }
}

fn check_row(context: &str, line: &str, want: &[Cell]) {
    let got = split_csv(line);
    assert_eq!(got.len(), want.len(), "{context}: 列数不对: {line:?}");
    check_cells(context, &got, want);
}

fn check_cells(context: &str, got: &[String], want: &[Cell]) {
    assert_eq!(got.len(), want.len(), "{context}: 列数不对: {got:?}");
    for (col, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            matches_cell(g, w),
            "{context}: 第 {} 列，DuckDB 给出 {g:?}，参考实现给出 {w:?}（整行 {got:?}）",
            col + 1
        );
    }
}

/// cast 的取值规则有两份实现（07 §7.8）：语义层的参考实现，和后端生成的守卫 SQL。
/// 让真实 DuckDB 跑完整的边界值矩阵，逐格对拍。
#[test]
fn cast_matrix_on_duckdb_matches_the_reference_implementation() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过 cast 对拍：未找到 duckdb CLI");
        return;
    }
    let rows = run_to_csv(
        "cast_matrix",
        r#"Table("tests/data/casts.csv") as c
  .extend({
    si = cast(c.s as i64),
    sf = cast(c.s as f64),
    xi = cast(c.x as i64),
    xf = cast(c.n as f64),
  })
  .select(s, si, sf, x, xi, n, xf)
  .write("{csv}")
"#,
    );
    let input = read_data("casts.csv");
    assert_eq!(rows.len(), input.len(), "输入有多少行，输出就该有多少行");
    assert!(
        rows.len() >= 25,
        "对拍矩阵应当覆盖全部边界值，实际 {} 行",
        rows.len()
    );

    for (i, (line, cells)) in rows.iter().zip(&input).enumerate() {
        assert_eq!(cells.len(), 3, "casts.csv 每行三列: {cells:?}");
        let s = cells[0].as_str();
        let x: Option<f64> = cells[1].parse().ok();
        let n: Option<i64> = cells[2].parse().ok();
        check_row(
            &format!("casts.csv 第 {} 行（s = {s:?}）", i + 2),
            line,
            &[
                Cell::S(s.to_string()),
                opt_i(cast::parse_i64_strict(s)),
                opt_f(cast::parse_f64_strict(s)),
                opt_f(x),
                opt_i(x.and_then(cast::f64_to_i64)),
                opt_i(n),
                opt_f(n.map(cast::i64_to_f64)),
            ],
        );
    }
}

/// 内置字符串函数在真实 DuckDB 上的结果，与规范（04 §4.8）逐格对拍。
///
/// `lower` / `upper` 有三处**已知的语义差集**：DuckDB 实现的是 Unicode 的「简单」大小写映射，
/// 规范写的是「全」映射。这里先把现状钉住，不假装它是对的——差集消掉的那一天，
/// 这三行 special case 就该删掉（见教程第 11 章）。
#[test]
fn string_builtins_on_duckdb_match_the_spec_except_known_gaps() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过字符串函数对拍：未找到 duckdb CLI");
        return;
    }
    let rows = run_to_csv(
        "strings",
        r#"Table("tests/data/strings.csv") as t
  .extend({
    trimmed = trim(t.s),
    len = length(t.s),
    low = lower(t.s),
    up = upper(t.s),
    has_b = contains(t.s, "b"),
  })
  .select(s, trimmed, len, low, up, has_b)
  .write("{csv}")
"#,
    );
    let input = read_data("strings.csv");
    assert_eq!(rows.len(), input.len());
    assert!(rows.len() >= 15);

    for (i, (line, cells)) in rows.iter().zip(&input).enumerate() {
        let s = cells[0].as_str();
        let context = format!("strings.csv 第 {} 行（s = {s:?}）", i + 2);
        // 已知差集：简单映射 ≠ 全映射
        let (low, up): (String, String) = match s {
            "İstanbul" => ("istanbul".into(), "İSTANBUL".into()), // 全映射的小写是 i + U+0307
            "ß" => ("ß".into(), "ẞ".into()),                      // 全映射的大写是 SS
            "ΣΑΣ" => ("σασ".into(), "ΣΑΣ".into()),                // 全映射的词尾 σ 是 ς
            other => (other.to_lowercase(), other.to_uppercase()),
        };
        check_row(
            &context,
            line,
            &[
                Cell::S(s.to_string()),
                Cell::S(s.trim().to_string()),
                Cell::I(s.chars().count() as i64),
                Cell::S(low),
                Cell::S(up),
                Cell::S(s.contains('b').to_string()),
            ],
        );
    }
}

/// `round`（取偶）与 `abs`：后端用 FLOOR/MOD 拼出来的公式，对拍 Rust 标准库。
#[test]
fn round_and_abs_on_duckdb_match_rust() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过 round/abs 对拍：未找到 duckdb CLI");
        return;
    }
    // .5 处的取偶、正负两侧、刚好小于 .5 的最大 f64、绝对值超过 2^53 的 .5（走取偶分支）、整数
    let xs: Vec<f64> = vec![
        0.5,
        1.5,
        2.5,
        3.5,
        4.5,
        -0.5,
        -1.5,
        -2.5,
        -0.4,
        0.49999999999999994,
        2.675,
        4503599627370492.5,
        10000000000000000.0,
    ];
    let norm = |v: f64| if v == 0.0 { 0.0 } else { v };
    // Pebble 的字面量不支持科学计数法（`1e16` 会被读成 `1` 和标识符 `e16`）；
    // Rust 的 `{:?}` 对大的 f64 恰恰会打印成 `1e16`，这里换成普通十进制
    let literal = |x: f64| {
        let s = format!("{x:?}");
        if s.contains(['e', 'E']) {
            format!("{x:.1}")
        } else {
            s
        }
    };
    let mut bindings = String::new();
    for (i, x) in xs.iter().enumerate() {
        bindings.push_str(&format!(
            "    r{i} = round({}),\n    a{i} = abs({}),\n",
            literal(*x),
            literal(*x)
        ));
    }
    let src = format!(
        "Table(\"tests/data/orders.csv\") as o\n  .extend({{\n{bindings}  }})\n  .take(1)\n  .write(\"{{csv}}\")\n"
    );
    let rows = run_to_csv("round_abs", &src);
    assert_eq!(rows.len(), 1);
    let want: Vec<Cell> = xs
        .iter()
        .flat_map(|x| [Cell::F(norm(x.round_ties_even())), Cell::F(norm(x.abs()))])
        .collect();
    // 前 5 列是 orders.csv 透传过来的，不属于这次对拍
    let got = split_csv(&rows[0]);
    assert_eq!(got.len(), 5 + want.len());
    check_cells("round / abs", &got[5..], &want);
}

/// having 在真实 DuckDB 上的结果：与「紧跟 agg 的 filter」相同，并与直接从 CSV 数出来的一致。
#[test]
fn having_keeps_the_same_groups_as_a_filter_after_agg() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过 having 端到端：未找到 duckdb CLI");
        return;
    }
    let program = |op: &str| {
        format!(
            r#"Table("tests/data/orders.csv") as o
  .group_by(o.customer_id)
  .agg(n = count(), total = sum(o.amount))
  .{op}(total > 1000 && n >= 2)
  .select(customer_id, n, total)
  .sort(customer_id)
  .write("{{csv}}")
"#
        )
    };
    let via_having = run_to_csv("having", &program("having"));
    let via_filter = run_to_csv("having_as_filter", &program("filter"));
    assert_eq!(
        via_having, via_filter,
        "having 与紧跟 agg 的 filter 结果必须相同"
    );

    // 第三种算法：直接从 CSV 数
    let mut groups: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for row in read_data("orders.csv") {
        let entry = groups.entry(row[4].clone()).or_insert((0, 0));
        entry.0 += 1;
        if !row[2].is_empty() {
            entry.1 += row[2].parse::<i64>().unwrap();
        }
    }
    let expected: Vec<String> = groups
        .iter()
        .filter(|(_, (n, total))| *total > 1000 && *n >= 2)
        .map(|(k, (n, total))| format!("{k},{n},{total}"))
        .collect();
    assert!(!expected.is_empty(), "这份数据里应当有满足条件的分组");
    assert_eq!(via_having, expected);
}

/// 给语言加新特性，不得改变既有程序的 HIR 哈希。样例 00 的哈希固定在这里：
/// 它变了，要么语义被有意地改了（那就要更新这里的常量，并当作一次新的审批），
/// 要么新特性意外地动了既有程序的 lowering（那是 bug）。
#[test]
fn existing_programs_keep_their_hir_hash() {
    set_repo_cwd();
    let out = std::env::temp_dir().join("pebble-it").join("hash-pin");
    let _ = std::fs::remove_dir_all(&out);
    let src = repo().join("examples").join("00_m0_revenue.pebble");
    let summary = cmd_build(&src, &locks_dir(), &out, None).expect("build");
    assert_eq!(
        summary.hir_hash,
        "sha256:4a895435e887280a4cdd4811b471b809329a505bc128b665de35b531c7026a69"
    );
}
