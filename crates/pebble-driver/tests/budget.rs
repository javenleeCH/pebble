//! ch13（M3 资源配额）集成测试：静态成本模型 + 配额拒绝 + 超时杀引擎 + OOM 归类。
//!
//! 攻防视角（恶意查询测试集的一角）：
//! - **静态防线**（build 时拒绝）：cross join 预算、缺 max_rows 源
//! - **动态防线**（run 时取消）：超时杀引擎（RUN-004）、OOM 归类（RUN-004）
//! - **运行时资源上限**（进 canonical SQL 前言，被审内容）：memory_limit

use pebble_driver::{cmd_build, cmd_run};
use pebble_hir::{ExecutionManifest, LockField, LockSource, LockType, SchemaLock};
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
    CWD.call_once(|| std::env::set_current_dir(repo()).expect("CWD"));
}

fn sandbox(name: &str) -> PathBuf {
    set_repo_cwd();
    let dir = repo().join("target").join("pebble-it-ch13").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn small_lock(locator: &str, max_rows: u64) -> SchemaLock {
    SchemaLock::build_with_max_rows(
        LockSource::Table {
            locator: locator.into(),
        },
        vec![
            LockField {
                name: "id".into(),
                ty: LockType::I64,
                optional: false,
            },
            LockField {
                name: "name".into(),
                ty: LockType::Str,
                optional: false,
            },
        ],
        Some(max_rows),
    )
}

fn write_lock(dir: &Path, lock: &SchemaLock) {
    // 固定文件名：不从 identity 派生（绝对路径太长/含保留字符）
    let n = dir.read_dir().map(|it| it.count()).unwrap_or(0);
    std::fs::write(dir.join(format!("lock{n}.json")), lock.to_json()).unwrap();
}

fn cross_join_program() -> String {
    "Table(\"a.csv\") as x\n  .join(Table(\"b.csv\") as y, on = x.id == y.id)\n  .collect()\n"
        .to_string()
}

#[test]
fn budget_rejects_cross_join_over_quota() {
    // a.csv 100 行 × b.csv 100 行 = 最坏 10000 行 > 配额 50 → SEM-018
    let dir = sandbox("quota-reject");
    write_lock(&dir, &small_lock("a.csv", 100));
    write_lock(&dir, &small_lock("b.csv", 100));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, cross_join_program()).unwrap();

    let failure = cmd_build(&src, &dir, &dir.join("out"), None, Some(50))
        .expect_err("cross join 超配额必须被拒绝");
    assert!(
        failure.report.contains("SEM-018"),
        "实际输出:\n{}",
        failure.report
    );
    assert!(
        failure.report.contains("10000"),
        "报告应包含最坏情况行数:\n{}",
        failure.report
    );
}

#[test]
fn budget_passes_when_quota_sufficient() {
    // 同一程序，配额 10000 → 通过，manifest 记录最坏情况
    let dir = sandbox("quota-pass");
    write_lock(&dir, &small_lock("a.csv", 100));
    write_lock(&dir, &small_lock("b.csv", 100));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, cross_join_program()).unwrap();

    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, Some(10000)).expect("配额恰好足够应通过");
    let manifest: ExecutionManifest =
        serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.estimated_max_output_rows, Some(10000));
    assert_eq!(manifest.quota_rows, Some(10000));
}

#[test]
fn budget_requires_all_sources_have_max_rows() {
    // 配额要求全部源声明 max_rows——缺了就拒绝（不静默降级）
    let dir = sandbox("quota-missing");
    write_lock(&dir, &small_lock("a.csv", 100));
    let no_rows = SchemaLock::build(
        LockSource::Table {
            locator: "b.csv".into(),
        },
        vec![
            LockField {
                name: "id".into(),
                ty: LockType::I64,
                optional: false,
            },
            LockField {
                name: "name".into(),
                ty: LockType::Str,
                optional: false,
            },
        ],
    );
    write_lock(&dir, &no_rows);
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, cross_join_program()).unwrap();

    let failure = cmd_build(&src, &dir, &dir.join("out"), None, Some(50))
        .expect_err("缺 max_rows 时配额必须拒绝");
    assert!(
        failure.report.contains("max_rows"),
        "报告应指出缺 max_rows:\n{}",
        failure.report
    );
}

#[test]
fn take_shrinks_bound_group_agg_is_rows() {
    // take(n)：min(输入, n)；全局聚合：= 1
    let dir = sandbox("take-bound");
    write_lock(&dir, &small_lock("a.csv", 1000));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, "Table(\"a.csv\") as a\n  .take(3)\n  .collect()\n").unwrap();
    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, Some(3)).expect("take(3) 使预算 ≤ 3");

    let src2 = dir.join("global.pebble");
    std::fs::write(
        &src2,
        "Table(\"a.csv\") as a\n  .agg(total = count(),)\n  .collect()\n",
    )
    .unwrap();
    let out2 = dir.join("out2");
    cmd_build(&src2, &dir, &out2, None, Some(1)).expect("全局聚合预算 = 1");
}

#[test]
fn without_quota_budget_is_informative_not_enforced() {
    // 不设配额：预算照样进 manifest（给审批者看），但不做拒绝
    let dir = sandbox("no-quota");
    write_lock(&dir, &small_lock("a.csv", 7));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, "Table(\"a.csv\") as a\n  .collect()\n").unwrap();
    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, None).expect("无配额正常构建");
    let manifest: ExecutionManifest =
        serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.estimated_max_output_rows, Some(7));
    assert_eq!(manifest.quota_rows, None);
}

#[test]
fn memory_limit_is_in_canonical_sql_preamble() {
    let dir = sandbox("memory-limit");
    write_lock(&dir, &small_lock("a.csv", 10));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, "Table(\"a.csv\") as a\n  .collect()\n").unwrap();
    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, None).expect("build");
    let sql = std::fs::read_to_string(out.join("plan.sql")).unwrap();
    assert!(
        sql.contains("SET memory_limit = '512MB';"),
        "memory_limit 必须进 canonical SQL 前言:\n{}",
        sql
    );
    let manifest: ExecutionManifest =
        serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.engine_config.memory_limit_mb, 512);
}

#[test]
fn timeout_kills_engine_writes_run004() {
    // 需要 duckdb；指向真实 duckdb，用极小的 timeout 让它被杀
    let duckdb = pebble_driver_test_utils::find_duckdb();
    let Some(duckdb) = duckdb else {
        eprintln!("跳过：未找到 duckdb CLI");
        return;
    };
    let dir = sandbox("timeout");
    // 写一个真实的数据文件（必须在 build 之前——内容哈希进 manifest）；
    // 绝对路径 + 正斜杠：Windows 反斜杠在 Pebble 字符串里是非法转义
    let data_path = dir.join("a.csv").to_string_lossy().replace('\\', "/");
    std::fs::write(&data_path, "id,name\n1,a\n2,b\n").unwrap();
    write_lock(&dir, &small_lock(&data_path, 10));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, format!("Table(\"{data_path}\") as a\n  .collect()\n")).unwrap();
    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, None).expect("build");

    // timeout = 0 秒：进程一启动就被杀
    let outcome = cmd_run(&out, Some(&duckdb), Some(0)).expect("run 返回 outcome");
    assert!(!outcome.ok, "0 秒超时必须被杀");
    let receipt: pebble_hir::ExecutionReceipt =
        serde_json::from_str(&std::fs::read_to_string(out.join("receipt.json")).unwrap()).unwrap();
    match receipt.status {
        pebble_hir::ExecutionStatus::Failed { code, message } => {
            assert_eq!(
                code.as_str(),
                "RUN-004",
                "超时必须归 RUN-004（资源），实际 code={} message: {}",
                code.as_str(),
                message
            );
        }
        pebble_hir::ExecutionStatus::Ok => panic!("超时被杀的执行不可能 Ok"),
    }
}

#[test]
fn oom_classified_as_run004() {
    // 分类函数单测：OOM 关键词 → RUN-004；其他引擎错误 → RUN-003
    let dir = sandbox("oom-classify");
    write_lock(&dir, &small_lock("a.csv", 10));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, "Table(\"a.csv\") as a\n  .collect()\n").unwrap();
    let out = dir.join("out");
    cmd_build(&src, &dir, &out, None, None).expect("build");
    // 直接测试分类函数（同 crate 内私有函数 → 通过公开路径绕过；这里从 lib 导出测试）
    assert_eq!(
        pebble_driver::classify_engine_failure("Out of Memory Error: Allocation failed"),
        pebble_hir::RunCode::ResourceExhausted
    );
    assert_eq!(
        pebble_driver::classify_engine_failure("memory limit exceeded"),
        pebble_hir::RunCode::ResourceExhausted
    );
    assert_eq!(
        pebble_driver::classify_engine_failure("file not found: nope.csv"),
        pebble_hir::RunCode::IoFailure
    );
}

#[test]
fn budget_is_deterministic_and_cached() {
    // 重复构建：预算一致（canonical 的一部分）
    let dir = sandbox("budget-det");
    write_lock(&dir, &small_lock("a.csv", 100));
    write_lock(&dir, &small_lock("b.csv", 200));
    let src = dir.join("pipeline.pebble");
    std::fs::write(&src, cross_join_program()).unwrap();
    let out1 = dir.join("out1");
    let out2 = dir.join("out2");
    cmd_build(&src, &dir, &out1, None, None).expect("build 1");
    cmd_build(&src, &dir, &out2, None, None).expect("build 2");
    let m1: ExecutionManifest =
        serde_json::from_str(&std::fs::read_to_string(out1.join("manifest.json")).unwrap())
            .unwrap();
    let m2: ExecutionManifest =
        serde_json::from_str(&std::fs::read_to_string(out2.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(m1.estimated_max_output_rows, Some(20000));
    assert_eq!(m1.estimated_max_output_rows, m2.estimated_max_output_rows);
}

mod pebble_driver_test_utils {
    use std::path::PathBuf;
    pub fn find_duckdb() -> Option<PathBuf> {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let exe = dir.join(if cfg!(windows) {
                    "duckdb.exe"
                } else {
                    "duckdb"
                });
                if exe.exists() {
                    return Some(exe);
                }
            }
        }
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("repo root");
        let tools = repo.join("tools");
        if tools.join("duckdb.exe").exists() {
            return Some(tools.join("duckdb.exe"));
        }
        if tools.join("duckdb").exists() {
            return Some(tools.join("duckdb"));
        }
        None
    }
}
