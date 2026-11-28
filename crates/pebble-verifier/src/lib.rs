//! pebble-verifier：独立证据链核验器（M2，教程第 12 章）。
//!
//! **独立性声明**（这是本 crate 的立身之本，写进第一段）：
//! - 运行时依赖面 = pebble-hir（数据模型 + canonical 解析 + 签名原语）+ JSON，仅此而已；
//! - **不加载** parser / semantics / duckdb codegen / driver——验证编译器产物，
//!   不需要（也不应当）信任编译器主路径；
//! - 只读工件（manifest / receipt / plan.sql / canonical.hir / 数据文件），
//!   不执行任何流水线，不写任何文件。
//!
//! 诚实边界：canonical 解析与签名原语与编译器共享 pebble-hir——「完全独立的
//! 第二套实现」是未来的加固项（见 ch12 练习），当前独立性指**独立于编译管线**。
//!
//! verify 核的是什么（与「执行成功」无关）：
//! 1. 审批真实性——ed25519 签名（挡自洽伪造）；
//! 2. 工件一致性——plan.sql 字节哈希 ↔ manifest.artifact_hash；
//! 3. 计划一致性——canonical.hir 解析重算哈希 ↔ manifest.hir_hash（roundtrip）；
//! 4. 回执绑定——receipt ↔ manifest（manifest_hash / 工件哈希 / 引擎版本 pin）；
//! 5. 数据快照——dataset 内容哈希复核（TOCTOU 审计侧）；
//! 6. 结构不变量——反序列化出的 HIR 通过 validate()。
//!
//! receipt 的执行状态（Ok/Failed）是**证据内容**而非核验失败：一次被 RUN-002
//! 拒绝的执行同样是合法证据。报告里如实转述，不据此判红绿灯。

use pebble_hir::{
    BackendArtifact, ExecutionManifest, ExecutionReceipt, canonical::roundtrip_hash,
    verify_manifest_signature,
};
use std::path::Path;

/// 单项核验结果。
#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// 核验报告：全部检查项 + 总判定。
#[derive(Clone, Debug)]
pub struct Report {
    pub checks: Vec<Check>,
    pub ok: bool,
}

/// 核验一个 build 目录的全部证据。
/// `pub_key_hex`：审批公钥（64 位 hex；必须来自可信渠道，不得取自被验目录）。
pub fn verify_build_dir(dir: &Path, pub_key_hex: &str) -> Report {
    let mut checks = Vec::new();
    let load = |path: &Path| -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {}", path.display(), e))
    };

    // manifest 是所有核验的锚点；它本身不存在则全线失败
    let manifest_result: Result<ExecutionManifest, String> = load(&dir.join("manifest.json"))
        .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("manifest 解析失败: {}", e)));
    let manifest = match manifest_result {
        Ok(m) => m,
        Err(e) => {
            return Report {
                checks: vec![Check {
                    name: "manifest 可读",
                    ok: false,
                    detail: e,
                }],
                ok: false,
            };
        }
    };

    // 1. 审批真实性：签名核验（pubkey 来自可信外部渠道）
    checks.push(check(
        "审批签名（ed25519）",
        verify_manifest_signature(&manifest, pub_key_hex)
            .map(|_| "签名与主体匹配，审批者身份成立".to_string()),
    ));

    // 2. 工件一致性：plan.sql 字节 ↔ manifest.artifact_hash
    let artifact_check = load(&dir.join("plan.sql")).and_then(|sql| {
        let artifact = BackendArtifact::DuckDbSql { canonical_sql: sql };
        let computed = format!("sha256:{}", artifact.artifact_hash());
        if computed == manifest.artifact_hash {
            Ok("plan.sql 哈希与 manifest 工件哈希一致".into())
        } else {
            Err(format!(
                "工件哈希不一致：manifest 记录 {}，磁盘 plan.sql 计算为 {}",
                manifest.artifact_hash, computed
            ))
        }
    });
    checks.push(check("工件一致性（plan.sql）", artifact_check));

    // 3. 计划一致性：canonical.hir 解析重算 ↔ manifest.hir_hash
    let hir_check = std::fs::read(dir.join("canonical.hir"))
        .map_err(|e| format!("读取 canonical.hir 失败: {}", e))
        .and_then(|bytes| match roundtrip_hash(&bytes) {
            Ok(h) if format!("sha256:{}", h) == manifest.hir_hash => {
                Ok("canonical HIR roundtrip 哈希与 manifest 一致".to_string())
            }
            Ok(h) => Err(format!(
                "HIR 哈希不一致：manifest 记录 {}，roundtrip 计算为 sha256:{}",
                manifest.hir_hash, h
            )),
            Err(e) => Err(format!("canonical.hir 无法解析：{}", e)),
        });
    checks.push(check("计划一致性（canonical HIR roundtrip）", hir_check));

    // 4. 回执绑定：receipt ↔ manifest（manifest_hash / 工件 / 引擎 pin）
    let receipt_check = load(&dir.join("receipt.json"))
        .and_then(|t| serde_json::from_str::<ExecutionReceipt>(&t).map_err(|e| e.to_string()))
        .and_then(|receipt| {
            let status_note = match &receipt.status {
                pebble_hir::ExecutionStatus::Ok => "（执行状态：Ok）".to_string(),
                pebble_hir::ExecutionStatus::Failed { code, message } => format!(
                    "（执行状态：Failed {} {}——失败执行同样是合法证据）",
                    code.as_str(),
                    message
                ),
            };
            match pebble_hir::manifest::verify(&manifest, &receipt) {
                Ok(()) => Ok(format!("回执与已审 manifest 绑定成立 {}", status_note)),
                Err(problems) => Err(problems.join("；")),
            }
        });
    checks.push(check("回执绑定（receipt ↔ manifest）", receipt_check));

    // 5. 数据快照复核（TOCTOU 审计侧）：对存在的数据文件重算内容哈希
    let mut data_problems = Vec::new();
    let mut data_n = 0usize;
    for ds in &manifest.dataset_identities {
        let Some(file) = &ds.data_file else {
            data_problems.push(format!("{} 无数据定位，跳过复核", ds.identity));
            continue;
        };
        let Some(expected) = &ds.content_hash else {
            data_problems.push(format!("{} 无内容快照（build 时数据缺失？）", file));
            continue;
        };
        data_n += 1;
        match std::fs::read(file) {
            Ok(bytes) => {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&bytes);
                let actual = format!("sha256:{}", pebble_hir::Hash(h.finalize().into()));
                if &actual != expected {
                    data_problems.push(format!(
                        "{} 内容与 build 快照不一致：build={} now={}",
                        file, expected, actual
                    ));
                }
            }
            Err(e) => data_problems.push(format!("{} 无法读取：{}", file, e)),
        }
    }
    checks.push(if data_problems.is_empty() {
        Check {
            name: "数据快照复核（TOCTOU）",
            ok: true,
            detail: format!("{} 个数据文件与 build 快照一致", data_n),
        }
    } else {
        Check {
            name: "数据快照复核（TOCTOU）",
            ok: false,
            detail: data_problems.join("；"),
        }
    });

    // 6. 结构不变量：反序列化出的 HIR 通过 validate()
    let invariant_check = std::fs::read(dir.join("canonical.hir"))
        .map_err(|e| format!("读取 canonical.hir 失败: {}", e))
        .and_then(|bytes| {
            pebble_hir::canonical::canonical_hir(&bytes)
                .and_then(|hir| hir.validate())
                .map(|_| "HIR 结构不变量全部成立".to_string())
        });
    checks.push(check("HIR 结构不变量（validate）", invariant_check));

    // 7. 预算↔配额一致性（ch13）：配额存在时预算必须存在且不超
    let budget_check = match (&manifest.estimated_max_output_rows, &manifest.quota_rows) {
        (Some(bound), Some(quota)) if *bound <= *quota => Ok(format!(
            "最坏情况输出 {} ≤ 配额 {}（ch13 静态成本模型）",
            bound, quota
        )),
        (Some(bound), Some(quota)) => Err(format!(
            "预算 {} 超过配额 {}——manifest 被篡改或 build 时未执行配额检查",
            bound, quota
        )),
        (Some(bound), None) => Ok(format!("最坏情况输出 {}（未设配额）", bound)),
        (None, Some(_)) => Err(
            "配额存在但预算缺失——manifest 被篡改（build 的配额检查不会产出这种组合）".to_string(),
        ),
        (None, None) => Ok("无预算（源缺 max_rows）且未设配额".to_string()),
    };
    checks.push(check("预算↔配额一致性（ch13）", budget_check));

    let ok = checks.iter().all(|c| c.ok);
    Report { checks, ok }
}

fn check(name: &'static str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check {
            name,
            ok: true,
            detail,
        },
        Err(detail) => Check {
            name,
            ok: false,
            detail,
        },
    }
}

/// 渲染报告为人类可读文本。
pub fn render_report(report: &Report) -> String {
    let mut out = String::new();
    for c in &report.checks {
        let mark = if c.ok { "[ok]  " } else { "[FAIL]" };
        out.push_str(&format!("{} {}\n      {}\n", mark, c.name, c.detail));
    }
    out.push_str(if report.ok {
        "EVIDENCE VERIFIED：证据链（真实性 + 一致性 + 绑定）全部成立"
    } else {
        "EVIDENCE REJECTED：见上方 FAIL 项"
    });
    out.push('\n');
    out
}
