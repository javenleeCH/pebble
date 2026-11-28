//! pebble-verifier 集成测试：M2 证据链的攻防矩阵。
//!
//! 每个用例都从「制造证据」开始（dev-dep pebble-driver），再从 verifier 视角核验。
//! 并行纪律：每个测试使用独立的目录与密钥文件，不修改共享 fixture。
//!
//! 覆盖：
//! 1. 完整签批流程 happy path（build → sign → run → verifier 全绿）；
//! 2. 篡改矩阵：plan.sql / canonical.hir / manifest 主体 / 数据文件 / 公钥；
//! 3. 未签名 manifest → 真实性检查失败（其余项仍应通过——定位精确）；
//! 4. 全部 examples 的 canonical.hir roundtrip 与 manifest.hir_hash 一致。

use pebble_driver::{cmd_build, cmd_run, cmd_sign};
use pebble_hir::{ApproverKey, ExecutionManifest, LockField, LockSource, LockType, SchemaLock};
use pebble_verifier::{Check, verify_build_dir};
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

/// 每个测试一个隔离沙箱：target/pebble-it-verifier/<name>/
fn sandbox(name: &str) -> PathBuf {
    set_repo_cwd();
    let base = repo().join("target").join("pebble-it-verifier").join(name);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn locks_dir() -> PathBuf {
    repo().join("tests").join("locks")
}

fn demo_src() -> PathBuf {
    repo().join("examples").join("00_m0_revenue.pebble")
}

fn key_from_file(path: &std::path::Path) -> ApproverKey {
    ApproverKey::from_json(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// 完整签批流程：build → sign → run（无 duckdb 也能出回执）。
/// 返回 (build 目录, 密钥文件)。
fn make_signed_evidence(name: &str) -> (PathBuf, PathBuf) {
    let base = sandbox(name);
    let out = base.join("build");
    cmd_build(&demo_src(), &locks_dir(), &out, None, None).expect("build");
    // 固定 seed 的测试密钥（可复现；真密钥必须用 keygen 的 OS 随机源）
    let key = ApproverKey::from_seed([42u8; 32]);
    let key_file = base.join("approver.json");
    std::fs::write(&key_file, key.to_json()).unwrap();
    cmd_sign(&out.join("manifest.json"), &key_file).expect("sign");
    // run 用不存在的 duckdb 路径：执行失败但回执绑定成立（失败也是证据）
    let missing = out.join("no-duckdb.exe");
    cmd_run(&out, Some(&missing), None).expect("run 产出回执");
    (out, key_file)
}

fn check_named<'a>(report: &'a [&'a Check], name: &str) -> &'a Check {
    report
        .iter()
        .find(|c| c.name.contains(name))
        .unwrap_or_else(|| panic!("报告中没有检查项 {}", name))
}

fn checks_of(report: &pebble_verifier::Report) -> Vec<&Check> {
    report.checks.iter().collect()
}

#[test]
fn happy_path_all_checks_pass() {
    let (dir, key_file) = make_signed_evidence("happy");
    let key = key_from_file(&key_file);
    let report = verify_build_dir(&dir, &key.public);
    let rendered = pebble_verifier::render_report(&report);
    assert!(report.ok, "报告:\n{}", rendered);
    assert_eq!(report.checks.len(), 7, "七项检查一个不少:\n{}", rendered);
}

#[test]
fn tamper_plan_sql_fails_artifact_check() {
    let (dir, key_file) = make_signed_evidence("tamper-sql");
    let sql = std::fs::read_to_string(dir.join("plan.sql")).unwrap();
    std::fs::write(dir.join("plan.sql"), sql.replace("10000", "9999")).unwrap();
    let key = key_from_file(&key_file);
    let report = verify_build_dir(&dir, &key.public);
    let checks = checks_of(&report);
    assert!(!report.ok);
    assert!(
        !check_named(&checks, "工件一致性").ok,
        "工件哈希必须抓到 plan.sql 篡改"
    );
    // 签名检查不受影响：签名覆盖 manifest，plan.sql 是独立工件
    assert!(check_named(&checks, "审批签名").ok);
    assert!(check_named(&checks, "回执绑定").ok);
}

#[test]
fn tamper_canonical_hir_fails_roundtrip_check() {
    let (dir, key_file) = make_signed_evidence("tamper-hir");
    let bytes = std::fs::read(dir.join("canonical.hir")).unwrap();
    let mut tampered = bytes.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0x01;
    std::fs::write(dir.join("canonical.hir"), tampered).unwrap();
    let key = key_from_file(&key_file);
    let report = verify_build_dir(&dir, &key.public);
    let checks = checks_of(&report);
    assert!(!report.ok);
    // roundtrip 或结构不变量二选一必须抓到（取决于翻转的字节位置）
    let hir_caught =
        !check_named(&checks, "计划一致性").ok || !check_named(&checks, "结构不变量").ok;
    assert!(hir_caught, "canonical.hir 篡改必须被抓到");
}

#[test]
fn tamper_manifest_body_fails_signature_check() {
    let (dir, key_file) = make_signed_evidence("tamper-manifest");
    let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    // 篡改主体字段（不动 signature）：自洽伪造的雏形——签名必须拦住
    let tampered = text.replace("\"hir_hash\": \"sha256:", "\"hir_hash\": \"sha256:0");
    std::fs::write(dir.join("manifest.json"), tampered).unwrap();
    let key = key_from_file(&key_file);
    let report = verify_build_dir(&dir, &key.public);
    let checks = checks_of(&report);
    assert!(!report.ok);
    assert!(
        !check_named(&checks, "审批签名").ok,
        "M1 哈希自洽核验拦不住这种篡改，签名必须拦住"
    );
}

#[test]
fn tamper_data_file_fails_toctou_check() {
    // 私有数据副本：不改共享 fixture（并行测试纪律）
    let base = sandbox("tamper-data");
    let data = base.join("orders.csv");
    std::fs::copy(repo().join("tests").join("data").join("orders.csv"), &data).unwrap();
    let data_path = data.to_string_lossy().replace('\\', "/");
    let lock = SchemaLock::build(
        LockSource::Table {
            locator: data_path.clone(),
        },
        vec![
            LockField {
                name: "order_id".into(),
                ty: LockType::I64,
                optional: false,
            },
            LockField {
                name: "status".into(),
                ty: LockType::Str,
                optional: false,
            },
            LockField {
                name: "amount".into(),
                ty: LockType::I64,
                optional: true,
            },
            LockField {
                name: "score".into(),
                ty: LockType::F64,
                optional: false,
            },
            LockField {
                name: "customer_id".into(),
                ty: LockType::Str,
                optional: false,
            },
        ],
    );
    std::fs::write(base.join("demo.lock.json"), lock.to_json()).unwrap();
    let src = base.join("pipeline.pebble");
    std::fs::write(&src, format!("Table(\"{data_path}\") as o\n  .collect()\n")).unwrap();
    let out = base.join("build");
    cmd_build(&src, &base, &out, None, None).expect("build");
    let key = ApproverKey::from_seed([42u8; 32]);
    let key_file = base.join("approver.json");
    std::fs::write(&key_file, key.to_json()).unwrap();
    cmd_sign(&out.join("manifest.json"), &key_file).expect("sign");
    let missing = out.join("no-duckdb.exe");
    cmd_run(&out, Some(&missing), None).expect("run 产出回执");

    // 篡改私有数据 → TOCTOU 检查失败
    let rows = std::fs::read_to_string(&data).unwrap().replace("c2", "c9");
    std::fs::write(&data, &rows).unwrap();
    let report = verify_build_dir(&out, &key.public);
    let checks = checks_of(&report);
    assert!(
        !check_named(&checks, "数据快照").ok,
        "数据被篡改必须被 TOCTOU 复核抓到"
    );
}

#[test]
fn wrong_pubkey_fails_signature_check() {
    let (dir, _key_file) = make_signed_evidence("wrong-key");
    let attacker_key = ApproverKey::from_seed([13u8; 32]);
    let report = verify_build_dir(&dir, &attacker_key.public);
    let checks = checks_of(&report);
    assert!(!report.ok);
    assert!(
        !check_named(&checks, "审批签名").ok,
        "攻击者的公钥不能通过别人的签名"
    );
}

#[test]
fn unsigned_manifest_fails_only_authenticity() {
    // 未签名 build：只有真实性失败，其余五项应通过——失败必须可定位
    let base = sandbox("unsigned");
    let out = base.join("build");
    cmd_build(&demo_src(), &locks_dir(), &out, None, None).expect("build");
    let missing = out.join("no-duckdb.exe");
    cmd_run(&out, Some(&missing), None).expect("run 产出回执");
    let key = ApproverKey::from_seed([42u8; 32]);
    let report = verify_build_dir(&out, &key.public);
    let rendered = pebble_verifier::render_report(&report);
    assert!(!report.ok, "未签名必须不通过:\n{}", rendered);
    assert!(!check_named(&checks_of(&report), "审批签名").ok);
    for c in checks_of(&report) {
        if !c.name.contains("审批签名") {
            assert!(
                c.ok,
                "未签名只影响真实性检查，{} 不应失败：{}",
                c.name, c.detail
            );
        }
    }
}

#[test]
fn every_example_hir_roundtrips_against_manifest() {
    // 全部样例：build → canonical.hir 解析重算哈希 ↔ manifest.hir_hash
    let examples = [
        "00_m0_revenue.pebble",
        "01_filter_extend_sort.pebble",
        "02_join_group_agg.pebble",
        "03_conditional_projection.pebble",
        "04_select_distinct.pebble",
        "05_aggregate_stats.pebble",
        "06_memory_table.pebble",
    ];
    for ex in &examples {
        let base = sandbox(&format!("roundtrip-{}", ex.trim_end_matches(".pebble")));
        let out = base.join("build");
        let src = repo().join("examples").join(ex);
        cmd_build(&src, &locks_dir(), &out, None, None).expect("build");
        let bytes = std::fs::read(out.join("canonical.hir")).unwrap();
        let h = pebble_hir::canonical::roundtrip_hash(&bytes)
            .unwrap_or_else(|e| panic!("{} roundtrip 失败: {}", ex, e));
        let manifest: ExecutionManifest =
            serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            format!("sha256:{}", h),
            manifest.hir_hash,
            "{} 的 canonical.hir 与 manifest.hir_hash 不一致",
            ex
        );
    }
}
