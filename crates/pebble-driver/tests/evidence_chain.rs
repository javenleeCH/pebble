//! M0 证据链集成测试：
//! 1. 全部 examples 可编译，hash 确定性；
//! 2. manifest/receipt 绑定与篡改检测；
//! 3. 无 duckdb 时的失败语义（RUN-003 IoFailure receipt）；
//! 4. 有 duckdb 时的端到端：run → 输出核对 → verify（RUN-001/002 反例）。
//!
//! 注意：Pebble 的数据路径按调用进程 CWD 解析；测试统一将 CWD 固定为仓库根。

use pebble_driver::{cmd_build, cmd_run, cmd_verify};
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

fn locks_dir() -> PathBuf {
    repo().join("tests").join("locks")
}

fn out_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("pebble-it").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn example(name: &str) -> PathBuf {
    repo().join("examples").join(name)
}

#[test]
fn all_examples_compile() {
    set_repo_cwd();
    let examples = [
        "00_m0_revenue.pebble",
        "01_filter_extend_sort.pebble",
        "02_join_group_agg.pebble",
        "03_conditional_projection.pebble",
        "04_select_distinct.pebble",
        "05_aggregate_stats.pebble",
        "06_memory_table.pebble",
    ];
    for ex in examples {
        let out = out_dir(&format!("compile-{}", ex.trim_end_matches(".pebble")));
        let result = cmd_build(&example(ex), &locks_dir(), &out, None);
        assert!(result.is_ok(), "编译 {} 失败", ex);
    }
}

#[test]
fn build_is_deterministic() {
    set_repo_cwd();
    let a = out_dir("det-a");
    let b = out_dir("det-b");
    let r1 = cmd_build(&example("00_m0_revenue.pebble"), &locks_dir(), &a, None).expect("build 1");
    let r2 = cmd_build(&example("00_m0_revenue.pebble"), &locks_dir(), &b, None).expect("build 2");
    assert_eq!(r1.hir_hash, r2.hir_hash, "重复编译的 HIR hash 必须一致");
    assert_eq!(r1.artifact_hash, r2.artifact_hash);
    assert_eq!(r1.manifest_hash, r2.manifest_hash);
}

#[test]
fn verify_binds_receipt_to_manifest_and_detects_tamper() {
    set_repo_cwd();
    // 自包含：本测试自行 build，避免与其他测试的目录竞争
    let out = out_dir("verify-binding");
    cmd_build(&example("00_m0_revenue.pebble"), &locks_dir(), &out, None).expect("build");
    let manifest: pebble_hir::ExecutionManifest = serde_json::from_str(
        &std::fs::read_to_string(out.join("manifest.json")).expect("manifest"),
    )
    .expect("解析 manifest");

    // 一致 receipt → verify Ok
    let receipt = pebble_hir::ExecutionReceipt {
        receipt_version: 1,
        manifest_hash: format!("sha256:{}", manifest.compute_hash()),
        submitted_artifact_hash: manifest.artifact_hash.clone(),
        actual_engine: pebble_hir::BackendIdentity {
            name: "duckdb".into(),
            version: "1.4.1".into(),
        },
        input_validation: vec![],
        status: pebble_hir::ExecutionStatus::Ok,
        output_summary: None,
    };
    let receipt_path = out.join("receipt.json");
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    assert!(cmd_verify(&out.join("manifest.json"), &receipt_path).is_ok());

    // 篡改 receipt 的工件哈希 → verify Err
    let mut tampered = receipt.clone();
    tampered.submitted_artifact_hash = "sha256:tampered".into();
    let tampered_path = out.join("receipt-tampered.json");
    std::fs::write(
        &tampered_path,
        serde_json::to_vec_pretty(&tampered).unwrap(),
    )
    .unwrap();
    assert!(cmd_verify(&out.join("manifest.json"), &tampered_path).is_err());

    // 篡改 manifest 本体 → 与 receipt 绑定断裂
    let mut m2 = manifest.clone();
    m2.hir_hash = "sha256:evil".into();
    let m2_path = out.join("manifest-evil.json");
    std::fs::write(&m2_path, serde_json::to_vec_pretty(&m2).unwrap()).unwrap();
    assert!(cmd_verify(&m2_path, &receipt_path).is_err());
}

#[test]
fn run_without_duckdb_writes_failed_receipt() {
    set_repo_cwd();
    let out = out_dir("run-no-duckdb");
    cmd_build(&example("00_m0_revenue.pebble"), &locks_dir(), &out, None).expect("build");
    let missing = out.join("no-such-duckdb.exe");
    let outcome = cmd_run(&out, Some(&missing)).expect("run 返回 outcome 而非 panic");
    assert!(!outcome.ok);
    let receipt: pebble_hir::ExecutionReceipt =
        serde_json::from_str(&std::fs::read_to_string(out.join("receipt.json")).expect("receipt"))
            .unwrap();
    match receipt.status {
        pebble_hir::ExecutionStatus::Failed { code, message } => {
            assert_eq!(code.as_str(), "RUN-003", "message: {}", message);
        }
        pebble_hir::ExecutionStatus::Ok => panic!("无 duckdb 时必须失败"),
    }
    // 即使 status 失败，工件绑定仍应成立
    assert!(cmd_verify(&out.join("manifest.json"), &out.join("receipt.json")).is_ok());
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

#[test]
fn end_to_end_demo_when_duckdb_present() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过端到端：未找到 duckdb CLI");
        return;
    }
    let out = out_dir("e2e");
    cmd_build(&example("00_m0_revenue.pebble"), &locks_dir(), &out, None).expect("build");
    let outcome = cmd_run(&out, None).expect("run");
    assert!(
        outcome.ok,
        "端到端执行必须成功: {:?}",
        outcome.output_summary
    );
    let summary_text = outcome.output_summary.clone().expect("collect 输出摘要");
    // 预期：c2 = 9000+3000 = 12000（2 单），c6 = 11000（1 单）；
    // c3 只有 open、c5 只有 refund 被过滤；c2 的 null amount 行被过滤；c1 收入 450 < 10000
    assert!(
        summary_text.contains("c2,2,12000"),
        "实际输出:\n{}",
        summary_text
    );
    assert!(
        summary_text.contains("c6,1,11000"),
        "实际输出:\n{}",
        summary_text
    );
    assert!(
        !summary_text.contains("c1"),
        "c1 的收入 450 < 10000，不得出现"
    );
    assert!(!summary_text.contains("c3"), "c3 只有 open 订单，不得出现");

    // verify：APPROVED PLAN == EXECUTED PLAN
    assert!(cmd_verify(&out.join("manifest.json"), &outcome.receipt_path).is_ok());
}

#[test]
fn toctou_data_tamper_fails_run002_when_duckdb_present() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过 TOCTOU：未找到 duckdb CLI");
        return;
    }
    let tmp = repo().join("target").join("pebble-it-toctou");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let data = tmp.join("orders_t.csv");
    std::fs::copy(repo().join("tests/data/orders.csv"), &data).unwrap();
    // Windows 路径统一为正斜杠：既是 Pebble 字符串合法内容，也保证 lock/pipeline 身份一致
    let data_path = data.to_string_lossy().replace('\\', "/");
    let lock = pebble_hir::SchemaLock::build(
        pebble_hir::LockSource::Table {
            locator: data_path.clone(),
        },
        vec![
            pebble_hir::LockField {
                name: "order_id".into(),
                ty: pebble_hir::LockType::I64,
                optional: false,
            },
            pebble_hir::LockField {
                name: "status".into(),
                ty: pebble_hir::LockType::Str,
                optional: false,
            },
            pebble_hir::LockField {
                name: "amount".into(),
                ty: pebble_hir::LockType::I64,
                optional: true,
            },
            pebble_hir::LockField {
                name: "score".into(),
                ty: pebble_hir::LockType::F64,
                optional: false,
            },
            pebble_hir::LockField {
                name: "customer_id".into(),
                ty: pebble_hir::LockType::Str,
                optional: false,
            },
        ],
    );
    std::fs::write(tmp.join("orders_t.lock.json"), lock.to_json()).unwrap();

    let src = tmp.join("pipeline.pebble");
    std::fs::write(
        &src,
        format!(
            "Table(\"{}\") as o\n  .filter(o.status == \"paid\")\n  .collect()\n",
            data_path
        ),
    )
    .unwrap();

    let out = tmp.join("build");
    cmd_build(&src, &tmp, &out, None).expect("build");
    let first = cmd_run(&out, None).expect("run 1");
    assert!(first.ok, "首次执行应成功");

    // 篡改数据 → RUN-002（TOCTOU 检测）
    let rows = std::fs::read_to_string(&data).unwrap().replace("c2", "c9");
    std::fs::write(&data, rows).unwrap();
    let second = cmd_run(&out, None).expect("run 2");
    assert!(!second.ok, "数据被篡改后必须失败");
    let receipt: pebble_hir::ExecutionReceipt =
        serde_json::from_str(&std::fs::read_to_string(out.join("receipt.json")).unwrap()).unwrap();
    match receipt.status {
        pebble_hir::ExecutionStatus::Failed { code, message } => {
            assert_eq!(code.as_str(), "RUN-002", "message: {}", message);
        }
        pebble_hir::ExecutionStatus::Ok => panic!("TOCTOU 检测失败"),
    }
}

#[test]
fn nonfinite_f64_input_fails_run001_when_duckdb_present() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过 RUN-001：未找到 duckdb CLI");
        return;
    }
    let tmp = repo().join("target").join("pebble-it-run001");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let data = tmp.join("scores.csv");
    std::fs::write(&data, "id,score\n1,1.5\n2,inf\n").unwrap();
    let data_path = data.to_string_lossy().replace('\\', "/");
    let lock = pebble_hir::SchemaLock::build(
        pebble_hir::LockSource::Table {
            locator: data_path.clone(),
        },
        vec![
            pebble_hir::LockField {
                name: "id".into(),
                ty: pebble_hir::LockType::I64,
                optional: false,
            },
            pebble_hir::LockField {
                name: "score".into(),
                ty: pebble_hir::LockType::F64,
                optional: false,
            },
        ],
    );
    std::fs::write(tmp.join("scores.lock.json"), lock.to_json()).unwrap();
    let src = tmp.join("pipeline.pebble");
    std::fs::write(
        &src,
        format!("Table(\"{}\") as s\n  .collect()\n", data_path),
    )
    .unwrap();
    let out = tmp.join("build");
    cmd_build(&src, &tmp, &out, None).expect("build");
    let result = cmd_run(&out, None).expect("run");
    assert!(!result.ok, "非有限 f64 输入必须失败（RUN-001）");
    let receipt: pebble_hir::ExecutionReceipt =
        serde_json::from_str(&std::fs::read_to_string(out.join("receipt.json")).unwrap()).unwrap();
    match receipt.status {
        pebble_hir::ExecutionStatus::Failed { code, .. } => {
            assert_eq!(code.as_str(), "RUN-001", "应为 RUN-001");
        }
        pebble_hir::ExecutionStatus::Ok => panic!("RUN-001 预检失效"),
    }
}

/// 回归（M0 遗留，第 8 章之后发现）：`extend` / `join` 直接接在源后面时 codegen 出错——
/// extend 编译失败；join 编译「成功」却生成 `SELECT , …`，到 DuckDB 上才报语法错误。
///
/// 现有的「全部样例可编译」只检查 build 成功，所以样例 02（join 是第一个算子）一直是坏的
/// 却没人发现。这条测试把两种写法真的跑起来，并核对输出的每一行。
#[test]
fn extend_and_join_directly_on_source_run_when_duckdb_present() {
    set_repo_cwd();
    if !duckdb_available() {
        eprintln!("跳过首算子回归：未找到 duckdb CLI");
        return;
    }
    let tmp = repo().join("target").join("pebble-it-first-op");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // 样例数据：每行 order_id,status,amount,score,customer_id
    let orders: Vec<Vec<String>> = std::fs::read_to_string(repo().join("tests/data/orders.csv"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .skip(1)
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect();
    assert!(orders.len() >= 2, "测试数据至少两行");
    let mut by_order = orders.clone();
    by_order.sort_by(|a, b| {
        a[0].parse::<i64>()
            .unwrap()
            .cmp(&b[0].parse::<i64>().unwrap())
    });

    // 输出摘要形如「N 行结果：\n<各行>」，最多带 11 行
    fn body(summary: &str) -> Vec<&str> {
        summary.lines().skip(1).collect()
    }
    let expected_summary_lines = format!("{} 行结果：", orders.len());

    // 1. extend 是第一个算子：常量列对每一行都成立
    let extend_src = tmp.join("extend_first.pebble");
    std::fs::write(
        &extend_src,
        "Table(\"tests/data/orders.csv\") as o\n  .extend({ tag = \"x\", one = 1 })\n  .select(order_id, tag, one)\n  .collect()\n",
    )
    .unwrap();
    let out = tmp.join("extend");
    cmd_build(&extend_src, &locks_dir(), &out, None)
        .expect("build：extend 作为第一个算子是合法程序");
    let outcome = cmd_run(&out, None).expect("run");
    assert!(
        outcome.ok,
        "extend 作为第一个算子必须能执行: {:?}",
        outcome.output_summary
    );
    let summary = outcome.output_summary.expect("collect 输出摘要");
    assert!(summary.starts_with(&expected_summary_lines), "{summary}");
    for line in body(&summary) {
        let parts: Vec<&str> = line.split(',').collect();
        assert_eq!(parts.len(), 3, "{line}");
        assert_eq!(parts[1], "x", "{line}");
        assert_eq!(parts[2], "1", "{line}");
    }
    assert!(cmd_verify(&out.join("manifest.json"), &outcome.receipt_path).is_ok());

    // 2. join 是第一个算子（按 order_id 自连接）：每行的 order_id 与 customer_id 来自同一行订单
    let join_src = tmp.join("join_first.pebble");
    std::fs::write(
        &join_src,
        "Table(\"tests/data/orders.csv\") as a\n  .join(Table(\"tests/data/orders.csv\") as b, on = a.order_id == b.order_id)\n  .select(a.order_id, b.customer_id)\n  .sort(a.order_id)\n  .collect()\n",
    )
    .unwrap();
    let out = tmp.join("join");
    cmd_build(&join_src, &locks_dir(), &out, None).expect("build");
    let outcome = cmd_run(&out, None).expect("run");
    assert!(
        outcome.ok,
        "join 作为第一个算子生成的 SQL 必须能被 DuckDB 执行: {:?}",
        outcome.output_summary
    );
    let summary = outcome.output_summary.expect("collect 输出摘要");
    assert!(summary.starts_with(&expected_summary_lines), "{summary}");
    let got = body(&summary);
    let shown = got.len().min(11);
    for (line, order) in got.iter().zip(by_order.iter()) {
        let expected = format!("{},{}", order[0], order[4]);
        assert_eq!(*line, expected, "自连接后每一行都应来自同一条订单");
    }
    assert_eq!(shown, orders.len().min(11), "{summary}");
    assert!(cmd_verify(&out.join("manifest.json"), &outcome.receipt_path).is_ok());
}

#[allow(dead_code)]
fn unused(_: &Path) {}
