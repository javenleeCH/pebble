//! pebble-driver：build / run / verify / lock 四幕证据链编排。
//!
//! build：源码 → canonical HIR + hash → DuckDB SQL 工件 → manifest。
//! run：TOCTOU 数据哈希核对 → RUN-001 f64 预检 → 提交工件 → ExecutionReceipt。
//! verify：核对「已审 manifest == 本次执行提交的工件」。
//! lock：生成 Schema lock（schema-as-code）。

use pebble_hir::{
    BackendArtifact, BackendIdentity, CanonicalConfig, CanonicalTerminalOptions, DatasetIdentity,
    ExecutionManifest, ExecutionReceipt, ExecutionStatus, Hash, InputValidation, LockField,
    LockSource, LockType, RunCode, SchemaLock,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const COMPILER_VERSION: &str = concat!("pebble ", env!("CARGO_PKG_VERSION"));

pub struct BuildSummary {
    pub out_dir: PathBuf,
    pub hir_hash: String,
    pub artifact_hash: String,
    pub manifest_hash: String,
}

/// 编译失败：诊断已打印到 stderr；u16 为建议退出码。
#[derive(Debug)]
pub struct CompileFailure(pub u16);

fn fail_msg(msg: &str) -> String {
    format!("[pebble] {}", msg)
}

/// `pebble build <file> --locks <dir> [--out <dir>] [--pin-duckdb <ver>]`
pub fn cmd_build(
    src: &Path,
    locks_dir: &Path,
    out_dir: &Path,
    pin_duckdb: Option<String>,
) -> Result<BuildSummary, CompileFailure> {
    let source = match std::fs::read_to_string(src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", fail_msg(&format!("读取源文件失败: {}", e)));
            return Err(CompileFailure(1));
        }
    };
    let filename = src
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("pipeline.pebble")
        .to_string();

    // 1. parse
    let mut diags = pebble_diagnostic::DiagBag::new();
    let program = match pebble_parser::parse(&source, &mut diags) {
        Some(p) => p,
        None => {
            for d in &diags.diags {
                eprintln!("{}", pebble_diagnostic::render(&source, &filename, d));
            }
            return Err(CompileFailure(2));
        }
    };

    // 2. catalog
    let catalog = match pebble_semantics::Catalog::load_dir(locks_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", fail_msg(&e));
            return Err(CompileFailure(1));
        }
    };

    // 3. semantics → HIR
    let hir = match pebble_semantics::compile(&program, &catalog, &mut diags) {
        Some(h) => h,
        None => {
            for d in &diags.diags {
                eprintln!("{}", pebble_diagnostic::render(&source, &filename, d));
            }
            return Err(CompileFailure(2));
        }
    };
    let hir_hash = pebble_semantics::hir_hash(&hir);

    // 4. codegen
    let lock_of = |kind: &pebble_hir::SourceKind| -> Option<SchemaLock> {
        let identity = match kind {
            pebble_hir::SourceKind::Table { locator, .. } => format!("table:{}", locator),
            pebble_hir::SourceKind::MemoryTable { name, .. } => format!("memory:{}", name),
        };
        catalog.get(&identity).cloned()
    };
    let artifact = match pebble_duckdb::compile_sql(&hir, &lock_of) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", fail_msg(&format!("DuckDB codegen 失败: {}", e)));
            return Err(CompileFailure(1));
        }
    };
    let artifact_hash = artifact.artifact_hash();

    // 5. dataset 身份 + 内容哈希（TOCTOU 证据）
    let mut schema_lock_hashes = Vec::new();
    let mut dataset_identities = Vec::new();
    for node in &hir.nodes {
        if let pebble_hir::HirNode::Source { kind, .. } = node {
            let (identity, data_file) = match kind {
                pebble_hir::SourceKind::Table { locator, .. } => {
                    (format!("table:{}", locator), Some(locator.clone()))
                }
                pebble_hir::SourceKind::MemoryTable { name, .. } => {
                    let df = lock_of(kind).and_then(|l| match &l.source {
                        LockSource::MemoryTable { data_file, .. } => data_file.clone(),
                        _ => None,
                    });
                    (format!("memory:{}", name), df)
                }
            };
            let lock = lock_of(kind).expect("语义层已校验 lock 存在");
            schema_lock_hashes.push(format!("sha256:{}", lock.validate().expect("已校验")));
            let content_hash = data_file
                .as_ref()
                .filter(|f| Path::new(f).exists())
                .map(|f| hash_file(Path::new(f)));
            dataset_identities.push(DatasetIdentity {
                identity,
                lock_hash: format!("sha256:{}", lock.validate().expect("已校验")),
                content_hash,
                data_file,
            });
        }
    }

    // 6. RUN-001 预检查询（build 时生成，run 时执行）
    let validations = pebble_duckdb::f64_validation_queries(&hir, &lock_of)
        .map_err(|e| {
            eprintln!("{}", fail_msg(&e));
            CompileFailure(1)
        })?
        .into_iter()
        .map(|(subject, sql)| serde_json::json!({"subject": subject, "sql": sql}))
        .collect::<Vec<_>>();

    // 7. manifest
    let terminal_options = match &hir.nodes[hir.root as usize] {
        pebble_hir::HirNode::Terminal { kind, .. } => match kind {
            pebble_hir::TerminalKind::Write { path } => {
                CanonicalTerminalOptions::Write { path: path.clone() }
            }
            pebble_hir::TerminalKind::Collect => CanonicalTerminalOptions::Collect,
            pebble_hir::TerminalKind::Print => CanonicalTerminalOptions::Print,
        },
        _ => unreachable!("root 是终端"),
    };
    let manifest = ExecutionManifest {
        manifest_version: ExecutionManifest::MANIFEST_VERSION,
        hir_hash: format!("sha256:{}", hir_hash),
        schema_lock_hashes,
        dataset_identities,
        semantics_version: hir.semantics_version.clone(),
        compiler_version: COMPILER_VERSION.to_string(),
        backend: artifact.backend_identity(),
        artifact_hash: format!("sha256:{}", artifact_hash),
        engine_config: CanonicalConfig { threads: 1 },
        terminal_options,
        engine_version_requirement: pin_duckdb.unwrap_or_default(),
    };
    let manifest_hash = manifest.compute_hash();

    // 8. 写出工件
    let BackendArtifact::DuckDbSql { canonical_sql } = &artifact;
    std::fs::create_dir_all(out_dir).map_err(|e| {
        eprintln!("{}", fail_msg(&format!("创建输出目录失败: {}", e)));
        CompileFailure(1)
    })?;
    let write = |name: &str, bytes: &[u8], note: &str| {
        std::fs::write(out_dir.join(name), bytes).map_err(|e| {
            eprintln!("{}", fail_msg(&format!("写出 {} 失败: {}", note, e)));
            CompileFailure(1)
        })
    };
    write(
        "canonical.hir",
        &pebble_semantics::canonical_bytes(&hir),
        "canonical HIR",
    )?;
    write("plan.sql", canonical_sql.as_bytes(), "canonical SQL")?;
    write(
        "manifest.json",
        serde_json::to_vec_pretty(&manifest).unwrap().as_slice(),
        "manifest",
    )?;
    write(
        "validations.json",
        serde_json::to_vec_pretty(&validations).unwrap().as_slice(),
        "validations",
    )?;
    write("plan.txt", hir.dump().as_bytes(), "HIR dump")?;
    write(
        "plan.algebra.tex",
        pebble_latex::latex_document(&hir).as_bytes(),
        "algebra view",
    )?;

    Ok(BuildSummary {
        out_dir: out_dir.to_path_buf(),
        hir_hash: format!("sha256:{}", hir_hash),
        artifact_hash: format!("sha256:{}", artifact_hash),
        manifest_hash: format!("sha256:{}", manifest_hash),
    })
}

fn hash_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut h = Sha256::new();
            h.update(&bytes);
            format!("sha256:{}", Hash(h.finalize().into()))
        }
        Err(_) => "sha256:unavailable".to_string(),
    }
}

// ---------- run ----------

pub struct RunOutcome {
    pub receipt_path: PathBuf,
    pub ok: bool,
    pub output_summary: Option<String>,
}

/// `pebble run <build_dir> [--duckdb <path>]`
pub fn cmd_run(build_dir: &Path, duckdb: Option<&Path>) -> Result<RunOutcome, String> {
    let manifest: ExecutionManifest = load_json(&build_dir.join("manifest.json"))?;
    let plan_sql = std::fs::read_to_string(build_dir.join("plan.sql"))
        .map_err(|e| format!("读取 plan.sql 失败: {}", e))?;
    let validations: Vec<serde_json::Value> = load_json(&build_dir.join("validations.json"))?;

    let manifest_hash = format!("sha256:{}", manifest.compute_hash());
    let artifact = BackendArtifact::DuckDbSql {
        canonical_sql: plan_sql.clone(),
    };

    // 1. 工件完整性：plan.sql 的哈希必须等于已审 manifest 的工件哈希
    let actual_artifact_hash = format!("sha256:{}", artifact.artifact_hash());
    if actual_artifact_hash != manifest.artifact_hash {
        let receipt = failed_receipt(
            &manifest,
            &manifest_hash,
            &actual_artifact_hash,
            RunCode::IoFailure,
            "plan.sql 与 manifest 工件哈希不一致（工件被篡改或损坏）",
        );
        return write_receipt(build_dir, receipt, false, None);
    }

    // 2. TOCTOU：数据内容哈希核对（RUN-002）
    for ds in &manifest.dataset_identities {
        if let (Some(_expected), Some(file)) = (&ds.content_hash, &ds.data_file) {
            let current = hash_file(Path::new(file));
            if current != *ds.content_hash.as_ref().unwrap() {
                let receipt = failed_receipt(
                    &manifest,
                    &manifest_hash,
                    &actual_artifact_hash,
                    RunCode::SchemaValidation,
                    &format!(
                        "数据文件 {} 内容与 build 时快照不一致（TOCTOU）：build={} run={}",
                        file,
                        ds.content_hash.as_ref().unwrap(),
                        current
                    ),
                );
                return write_receipt(build_dir, receipt, false, None);
            }
        } else if ds.data_file.is_none() {
            let receipt = failed_receipt(
                &manifest,
                &manifest_hash,
                &actual_artifact_hash,
                RunCode::IoFailure,
                &format!(
                    "数据集 {} 无 CLI 数据定位（宿主注册内存表），无法在 CLI 执行",
                    ds.identity
                ),
            );
            return write_receipt(build_dir, receipt, false, None);
        } else {
            // build 时数据文件不存在 → 无法做内容核对，直接失败（RUN-002）
            let receipt = failed_receipt(
                &manifest,
                &manifest_hash,
                &actual_artifact_hash,
                RunCode::SchemaValidation,
                &format!(
                    "数据文件 {} 在 build 时不存在，无法建立内容快照（SCH-005/RUN-002）",
                    ds.data_file.clone().unwrap_or_default()
                ),
            );
            return write_receipt(build_dir, receipt, false, None);
        }
    }

    // 3. 定位 DuckDB CLI
    let duckdb_path = match resolve_duckdb(duckdb) {
        Some(p) => p,
        None => {
            let receipt = failed_receipt(
                &manifest,
                &manifest_hash,
                &actual_artifact_hash,
                RunCode::IoFailure,
                "找不到 duckdb CLI（--duckdb 指定路径，或安装到 PATH / tools/duckdb.exe）",
            );
            return write_receipt(build_dir, receipt, false, None);
        }
    };

    // 4. 引擎版本
    let engine_version = run_duckdb_capture(&duckdb_path, "SELECT version;")
        .map(|s| s.trim().trim_matches('"').to_string())
        .unwrap_or_default();

    // 5. RUN-001 f64 预检（表达式求值前）
    let mut input_validation = Vec::new();
    let mut validation_failed = false;
    if !validations.is_empty() {
        let mut script = String::from("SET threads TO 1;\n");
        for v in &validations {
            let subject = v["subject"].as_str().unwrap_or("?");
            let sql = v["sql"].as_str().unwrap_or("SELECT 0");
            script.push_str(&format!(
                "SELECT '{}' AS subject, * FROM ({}) AS v;\n",
                subject.replace('\'', "''"),
                sql
            ));
        }
        let out = run_duckdb_capture(&duckdb_path, &script).unwrap_or_default();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.split(',');
            let subject = parts.next().unwrap_or("?").to_string();
            let result = parts.next().unwrap_or("").to_string();
            let ok = result.contains("ok");
            if !ok {
                validation_failed = true;
            }
            input_validation.push(InputValidation {
                subject,
                ok,
                detail: result,
            });
        }
    }
    if validation_failed {
        let receipt = failed_receipt(
            &manifest,
            &manifest_hash,
            &actual_artifact_hash,
            RunCode::NonFiniteInput,
            "f64 输入含 NaN 或 ±∞，违反 MVP 有限值输入约束（RUN-001）",
        );
        // 保留已完成的 input_validation
        let mut r = receipt;
        r.input_validation = input_validation;
        return write_receipt(build_dir, r, false, None);
    }

    // 6. 提交工件执行
    let (status, output) = match run_duckdb_capture_status(&duckdb_path, &plan_sql) {
        RunResult::Ok(stdout) => (ExecutionStatus::Ok, Some(stdout)),
        RunResult::Err(stderr) => (
            ExecutionStatus::Failed {
                code: RunCode::IoFailure,
                message: stderr,
            },
            None,
        ),
    };

    // 7. 输出摘要
    let output_summary = match (&status, &manifest.terminal_options) {
        (ExecutionStatus::Ok, CanonicalTerminalOptions::Write { path }) => {
            let written = Path::new(path).exists();
            Some(format!(
                "write → {} {}",
                path,
                if written {
                    "(已写出)"
                } else {
                    "(未发现输出文件！)"
                }
            ))
        }
        (ExecutionStatus::Ok, _) => output.as_ref().map(|csv| {
            let lines: Vec<&str> = csv.lines().filter(|l| !l.trim().is_empty()).collect();
            let body = lines
                .iter()
                .take(11)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            if lines.len() > 11 {
                format!("{} 行结果（前 11 行）：\n{}", lines.len(), body)
            } else {
                format!("{} 行结果：\n{}", lines.len(), body)
            }
        }),
        _ => None,
    };

    let receipt = ExecutionReceipt {
        receipt_version: ExecutionReceipt::RECEIPT_VERSION,
        manifest_hash: manifest_hash.clone(),
        submitted_artifact_hash: actual_artifact_hash,
        actual_engine: BackendIdentity {
            name: "duckdb".into(),
            version: engine_version,
        },
        input_validation,
        status: status.clone(),
        output_summary: output_summary.clone(),
    };
    let ok = matches!(status, ExecutionStatus::Ok);
    write_receipt(build_dir, receipt, ok, output_summary)
}

enum RunResult {
    Ok(String),
    Err(String),
}

fn run_duckdb_capture(duckdb: &Path, script: &str) -> Option<String> {
    match run_duckdb_capture_status(duckdb, script) {
        RunResult::Ok(s) => Some(s),
        RunResult::Err(_) => None,
    }
}

fn run_duckdb_capture_status(duckdb: &Path, script: &str) -> RunResult {
    use std::io::Write as _;
    let mut child = match Command::new(duckdb)
        .arg("-batch")
        .arg("-csv")
        .arg("-noheader")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunResult::Err(format!("启动 duckdb 失败: {}", e)),
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(script.as_bytes()).is_err() {
            return RunResult::Err("写入 duckdb stdin 失败".into());
        }
    }
    match child.wait_with_output() {
        Ok(out) => {
            if out.status.success() {
                RunResult::Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                RunResult::Err(if stderr.is_empty() {
                    format!("duckdb 退出码 {}", out.status.code().unwrap_or(-1))
                } else {
                    stderr
                })
            }
        }
        Err(e) => RunResult::Err(format!("等待 duckdb 失败: {}", e)),
    }
}

fn resolve_duckdb(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        return None;
    }
    // PATH
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
    // tools/
    let local = PathBuf::from("tools").join(if cfg!(windows) {
        "duckdb.exe"
    } else {
        "duckdb"
    });
    if local.exists() {
        return Some(local);
    }
    None
}

fn failed_receipt(
    manifest: &ExecutionManifest,
    manifest_hash: &str,
    artifact_hash: &str,
    code: RunCode,
    message: &str,
) -> ExecutionReceipt {
    ExecutionReceipt {
        receipt_version: ExecutionReceipt::RECEIPT_VERSION,
        manifest_hash: manifest_hash.to_string(),
        submitted_artifact_hash: artifact_hash.to_string(),
        actual_engine: manifest.backend.clone(),
        input_validation: Vec::new(),
        status: ExecutionStatus::Failed {
            code,
            message: message.to_string(),
        },
        output_summary: None,
    }
}

fn write_receipt(
    build_dir: &Path,
    receipt: ExecutionReceipt,
    ok: bool,
    summary: Option<String>,
) -> Result<RunOutcome, String> {
    let receipt_path = build_dir.join("receipt.json");
    std::fs::write(
        &receipt_path,
        serde_json::to_vec_pretty(&receipt).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写出 receipt 失败: {}", e))?;
    Ok(RunOutcome {
        receipt_path,
        ok,
        output_summary: summary,
    })
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
    serde_json::from_str(&text).map_err(|e| format!("解析 {} 失败: {}", path.display(), e))
}

// ---------- verify ----------

/// `pebble verify <manifest.json> <receipt.json>`；Ok(()) = APPROVED。
pub fn cmd_verify(manifest_path: &Path, receipt_path: &Path) -> Result<(), Vec<String>> {
    let manifest: ExecutionManifest = load_json(manifest_path).map_err(|e| vec![e])?;
    let receipt: ExecutionReceipt = load_json(receipt_path).map_err(|e| vec![e])?;
    pebble_hir::manifest::verify(&manifest, &receipt)
}

// ---------- lock ----------

/// `pebble lock table <locator> <field>...` / `pebble lock memory <name> [--data-file f] <field>...`
/// 字段格式：`name:type` 或 `name:type?`（可选）。
pub fn cmd_lock(args: &[String], out: &Path) -> Result<(), String> {
    if args.len() < 2 {
        return Err(
            "用法: pebble lock <table|memory> <locator|name> [--data-file f] name:type[?]..."
                .into(),
        );
    }
    let kind = args[0].as_str();
    let rest = &args[1..];
    let (source, mut field_args) = match kind {
        "table" => {
            let locator = rest[0].clone();
            (LockSource::Table { locator }, rest[1..].to_vec())
        }
        "memory" => {
            let name = rest[0].clone();
            let mut data_file = None;
            let mut rest2: Vec<String> = Vec::new();
            let mut i = 1;
            while i < rest.len() {
                if rest[i] == "--data-file" {
                    data_file = rest.get(i + 1).cloned();
                    i += 2;
                } else {
                    rest2.push(rest[i].clone());
                    i += 1;
                }
            }
            (LockSource::MemoryTable { name, data_file }, rest2)
        }
        _ => return Err(format!("未知 lock 类型 `{}`（table|memory）", kind)),
    };
    field_args.retain(|a| !a.is_empty());
    if field_args.is_empty() {
        return Err("至少需要一个字段定义 name:type[?]".into());
    }
    let mut fields = Vec::new();
    for spec in &field_args {
        let optional = spec.ends_with('?');
        let base = spec.trim_end_matches('?');
        let Some((name, ty)) = base.split_once(':') else {
            return Err(format!("字段 `{}` 缺少类型（name:type[?]）", spec));
        };
        let ty = match ty {
            "i64" => LockType::I64,
            "f64" => LockType::F64,
            "bool" => LockType::Bool,
            "string" => LockType::Str,
            other => return Err(format!("未知类型 `{}`（i64|f64|bool|string）", other)),
        };
        fields.push(LockField {
            name: name.to_string(),
            ty,
            optional,
        });
    }
    let lock = SchemaLock::build(source, fields);
    std::fs::write(out, lock.to_json()).map_err(|e| format!("写出 lock 失败: {}", e))?;
    Ok(())
}

// 供测试使用的小工具
pub fn default_out_dir(src: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pipeline");
    src.parent()
        .unwrap_or(Path::new("."))
        .join("target")
        .join("pebble")
        .join(stem)
}

#[allow(dead_code)]
pub fn unused(_: &BTreeMap<String, String>) {}
