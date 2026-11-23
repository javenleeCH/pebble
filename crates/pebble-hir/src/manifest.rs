//! BackendArtifact、ExecutionManifest、ExecutionReceipt 与 CanonicalConfig。
//!
//! 承诺边界（design-decisions §1.2）：
//! - manifest/receipt 证明“本次执行请求与已审工件严格绑定”；
//! - 不证明外部引擎物理计划等于 HIR，也不证明业务结果正确；
//! - LaTeX 是同一 HIR 的可读视图，不是执行正确性证明。

use crate::Hash;
use serde::{Deserialize, Serialize};

/// 后端身份。version 为实际/要求引擎版本（空串 = 不 pin）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendIdentity {
    pub name: String,
    pub version: String,
}

/// 后端工件：编译产物，可冻结、哈希、复核。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum BackendArtifact {
    /// DuckDB SQL 首发（M0/M1）。
    DuckDbSql {
        /// canonical SQL 文本（含 SET threads TO 1 前言）。
        canonical_sql: String,
    },
}

impl BackendArtifact {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match self {
            BackendArtifact::DuckDbSql { canonical_sql } => {
                let mut b = b"pebble-backend-artifact/duckdb-sql/v1\0".to_vec();
                b.extend_from_slice(canonical_sql.as_bytes());
                b
            }
        }
    }

    pub fn artifact_hash(&self) -> Hash {
        Hash::of_bytes("pebble-backend-artifact-v1", &self.canonical_bytes())
    }

    pub fn backend_identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "duckdb-sql".into(),
            version: String::new(),
        }
    }
}

/// 规范引擎配置（M0：单线程保证聚合顺序确定性）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalConfig {
    pub threads: u32,
}

impl Default for CanonicalConfig {
    fn default() -> Self {
        CanonicalConfig { threads: 1 }
    }
}

/// 终端选项快照：进入 manifest，不得在 codegen 后另加未审批目标。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalTerminalOptions {
    Write { path: String },
    Collect,
    Print,
}

/// 数据集身份：定位符 + lock 哈希 + （可选）内容哈希。
/// 内容哈希在 build 时对存在的数据文件计算；run 时重算比对以防 TOCTOU。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DatasetIdentity {
    pub identity: String,
    pub lock_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// 数据文件路径（Table locator 或 MemoryTable data_file）；无则不能 run。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_file: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionManifest {
    pub manifest_version: u32,
    pub hir_hash: String,
    pub schema_lock_hashes: Vec<String>,
    pub dataset_identities: Vec<DatasetIdentity>,
    pub semantics_version: String,
    pub compiler_version: String,
    pub backend: BackendIdentity,
    pub artifact_hash: String,
    pub engine_config: CanonicalConfig,
    pub terminal_options: CanonicalTerminalOptions,
    /// 如非空：run 时实际引擎版本必须与其一致，否则 verify 失败。
    #[serde(default)]
    pub engine_version_requirement: String,
}

impl ExecutionManifest {
    pub const MANIFEST_VERSION: u32 = 1;

    /// manifest 主体的规范字节（不含 manifest_hash 自身）。
    fn canonical_bytes(&self) -> Vec<u8> {
        // serde 结构体字段顺序稳定；BTree 确保集合顺序稳定。
        let json = serde_json::to_vec(self).expect("manifest 序列化");
        json
    }

    pub fn compute_hash(&self) -> Hash {
        Hash::of_bytes("pebble-execution-manifest-v1", &self.canonical_bytes())
    }
}

/// RUN 诊断码（08-diagnostics §8.3）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCode {
    /// f64 输入含 NaN/±∞（RUN-001）
    NonFiniteInput,
    /// 输入 Schema 校验失败（RUN-002）
    SchemaValidation,
    /// 终端 IO / 宿主输出失败（RUN-003）
    IoFailure,
    /// 取消 / OOM / 配额耗尽（RUN-004）
    ResourceExhausted,
}

impl RunCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunCode::NonFiniteInput => "RUN-001",
            RunCode::SchemaValidation => "RUN-002",
            RunCode::IoFailure => "RUN-003",
            RunCode::ResourceExhausted => "RUN-004",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputValidation {
    /// 校验对象身份（如 “f64 列 orders.amount”）。
    pub subject: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecutionStatus {
    Ok,
    Failed {
        code: RunCode,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub receipt_version: u32,
    pub manifest_hash: String,
    pub submitted_artifact_hash: String,
    /// 实际执行引擎及其版本（来自引擎自报）。
    pub actual_engine: BackendIdentity,
    pub input_validation: Vec<InputValidation>,
    pub status: ExecutionStatus,
    /// 终端回显：collect/print 的行数与写出文件；失败时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
}

impl ExecutionReceipt {
    pub const RECEIPT_VERSION: u32 = 1;
}

/// verify：核对“已审 manifest == 本次执行提交的工件”。
/// 返回 Ok(()) 或失败原因列表。
pub fn verify(manifest: &ExecutionManifest, receipt: &ExecutionReceipt) -> Result<(), Vec<String>> {
    // 哈希比较统一去掉可选的 "sha256:" 前缀
    fn norm(s: &str) -> &str {
        s.trim_start_matches("sha256:")
    }
    let mut problems = Vec::new();
    let expected_manifest = manifest.compute_hash().to_string();
    if norm(&receipt.manifest_hash) != expected_manifest {
        problems.push(format!(
            "manifest_hash 不一致：receipt 记录 {}，当前 manifest 计算为 {}",
            receipt.manifest_hash, expected_manifest
        ));
    }
    let expected_artifact = manifest.artifact_hash.trim_start_matches("sha256:");
    if norm(&receipt.submitted_artifact_hash) != expected_artifact {
        problems.push(format!(
            "执行提交的工件哈希 {} 与已审 manifest 的工件哈希 {} 不一致",
            receipt.submitted_artifact_hash, manifest.artifact_hash
        ));
    }
    if !manifest.engine_version_requirement.is_empty()
        && receipt.actual_engine.version != manifest.engine_version_requirement
    {
        problems.push(format!(
            "实际引擎版本 {} 不满足 manifest 要求的 {}",
            receipt.actual_engine.version, manifest.engine_version_requirement
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ExecutionManifest {
        ExecutionManifest {
            manifest_version: 1,
            hir_hash: "aa".into(),
            schema_lock_hashes: vec!["bb".into()],
            dataset_identities: vec![DatasetIdentity {
                identity: "table:orders.csv".into(),
                lock_hash: "bb".into(),
                content_hash: Some("cc".into()),
                data_file: Some("orders.csv".into()),
            }],
            semantics_version: "pebble-semantics-0.1".into(),
            compiler_version: "pebble 0.1.0".into(),
            backend: BackendIdentity {
                name: "duckdb-sql".into(),
                version: String::new(),
            },
            artifact_hash: "dd".into(),
            engine_config: CanonicalConfig { threads: 1 },
            terminal_options: CanonicalTerminalOptions::Collect,
            engine_version_requirement: String::new(),
        }
    }

    #[test]
    fn verify_binds_receipt_to_manifest() {
        let m = sample_manifest();
        let r = ExecutionReceipt {
            receipt_version: 1,
            manifest_hash: m.compute_hash().to_string(),
            submitted_artifact_hash: m.artifact_hash.clone(),
            actual_engine: BackendIdentity {
                name: "duckdb-sql".into(),
                version: "1.4.1".into(),
            },
            input_validation: vec![],
            status: ExecutionStatus::Ok,
            output_summary: None,
        };
        assert!(verify(&m, &r).is_ok());

        // 篡改 artifact hash → verify 失败
        let mut bad = r.clone();
        bad.submitted_artifact_hash = "tampered".into();
        assert!(verify(&m, &bad).is_err());
    }

    #[test]
    fn artifact_hash_is_deterministic() {
        let a = BackendArtifact::DuckDbSql {
            canonical_sql: "SELECT 1".into(),
        };
        let b = BackendArtifact::DuckDbSql {
            canonical_sql: "SELECT 1".into(),
        };
        assert_eq!(a.artifact_hash(), b.artifact_hash());
        let c = BackendArtifact::DuckDbSql {
            canonical_sql: "SELECT 2".into(),
        };
        assert_ne!(a.artifact_hash(), c.artifact_hash());
    }
}
