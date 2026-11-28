//! M2 证据签名（教程第 12 章）：ed25519 manifest 签名与验签。
//!
//! 解决的洞（详见 docs/tutorial/ch12-签名与verifier.md）：
//! 1. **自洽伪造**——manifest/receipt 谁都能重新生成一套，哈希自洽即通过；
//!    签名把「审批」从记哈希变成不可伪造的密码学行为。
//! 2. 主体与签名分离——签名覆盖 signature 置空的 manifest 主体；
//!    `manifest_hash` 覆盖含签名的整体，receipt 绑定的是已签 manifest。
//!
//! 纪律：公钥不入 manifest（写在被验对象里等于没验），验签公钥来自可信外部渠道。

use crate::ExecutionManifest;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// 审批密钥对文件（`pebble keygen` 生成）。
/// `seed` 即私钥种子——文件必须保密；`public` 派生自 seed，可公开分发。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApproverKey {
    pub version: u32,
    pub algorithm: String,
    pub seed: String,
    pub public: String,
}

impl ApproverKey {
    pub const KEY_VERSION: u32 = 1;
    pub const ALGORITHM: &str = "ed25519";

    /// 生成新的密钥对（OS 随机源）。
    pub fn generate() -> ApproverKey {
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let seed = hex_encode(&signing.to_bytes());
        let public = hex_encode(&signing.verifying_key().to_bytes());
        ApproverKey {
            version: Self::KEY_VERSION,
            algorithm: Self::ALGORITHM.to_string(),
            seed,
            public,
        }
    }

    /// 从 32 字节 seed 派生（供测试与导入）。
    pub fn from_seed(seed: [u8; 32]) -> ApproverKey {
        let signing = SigningKey::from_bytes(&seed);
        let public = hex_encode(&signing.verifying_key().to_bytes());
        ApproverKey {
            version: Self::KEY_VERSION,
            algorithm: Self::ALGORITHM.to_string(),
            seed: hex_encode(&seed),
            public,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("key 序列化")
    }

    pub fn from_json(text: &str) -> Result<ApproverKey, String> {
        let key: ApproverKey =
            serde_json::from_str(text).map_err(|e| format!("解析密钥文件失败: {}", e))?;
        if key.version != Self::KEY_VERSION || key.algorithm != Self::ALGORITHM {
            return Err(format!(
                "不支持的密钥版本/算法（version={}, algorithm={}）",
                key.version, key.algorithm
            ));
        }
        // 完整性：seed 派生的公钥必须与文件里的 public 一致
        let seed_bytes = hex_decode(&key.seed)?;
        let Ok(seed_bytes) = <[u8; 32]>::try_from(seed_bytes.as_slice()) else {
            return Err("seed 长度不是 32 字节".into());
        };
        let derived = Self::from_seed(seed_bytes);
        if derived.public != key.public {
            return Err("密钥文件自相矛盾：public 与 seed 不匹配".into());
        }
        Ok(key)
    }

    fn signing_key(&self) -> Result<SigningKey, String> {
        let seed = hex_decode(&self.seed)?;
        let Ok(seed) = <[u8; 32]>::try_from(seed.as_slice()) else {
            return Err("seed 长度不是 32 字节".into());
        };
        Ok(SigningKey::from_bytes(&seed))
    }
}

/// 计算 manifest 主体字节（signature 置空序列化——与未签名格式字节一致）。
fn manifest_body_bytes(manifest: &ExecutionManifest) -> Vec<u8> {
    let mut body = manifest.clone();
    body.signature = String::new();
    body.canonical_bytes()
}

/// 用审批私钥对 manifest 主体签名，返回 `ed25519:<hex>` 形态的签名字段值。
pub fn sign_manifest(manifest: &ExecutionManifest, key: &ApproverKey) -> Result<String, String> {
    let signing = key.signing_key()?;
    let sig = signing.sign(&manifest_body_bytes(manifest));
    Ok(format!("ed25519:{}", hex_encode(&sig.to_bytes())))
}

/// 签名状态：未签 / 已签 / 格式非法。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignatureStatus {
    Unsigned,
    Signed { algorithm: String, hex: String },
}

pub fn signature_status(manifest: &ExecutionManifest) -> SignatureStatus {
    if manifest.signature.is_empty() {
        return SignatureStatus::Unsigned;
    }
    match manifest.signature.split_once(':') {
        Some((algo, hex)) => SignatureStatus::Signed {
            algorithm: algo.to_string(),
            hex: hex.to_string(),
        },
        None => SignatureStatus::Signed {
            algorithm: String::new(),
            hex: manifest.signature.clone(),
        },
    }
}

/// 用公钥（hex，64 字符 = 32 字节）核验 manifest 签名。
pub fn verify_manifest_signature(
    manifest: &ExecutionManifest,
    pubkey_hex: &str,
) -> Result<(), String> {
    let sig = match signature_status(manifest) {
        SignatureStatus::Unsigned => {
            return Err("manifest 未签名：证据链只有一致性，没有真实性".into());
        }
        SignatureStatus::Signed { algorithm, hex } => {
            if algorithm != "ed25519" {
                return Err(format!("不支持的签名算法 `{}`", algorithm));
            }
            let bytes = hex_decode(&hex)?;
            let Ok(bytes) = <[u8; 64]>::try_from(bytes.as_slice()) else {
                return Err("签名长度不是 64 字节".into());
            };
            Signature::from_bytes(&bytes)
        }
    };
    let key_bytes = hex_decode(pubkey_hex)?;
    let Ok(key_bytes) = <[u8; 32]>::try_from(key_bytes.as_slice()) else {
        return Err("公钥长度不是 32 字节".into());
    };
    let verifying = VerifyingKey::from_bytes(&key_bytes).map_err(|e| format!("公钥非法: {}", e))?;
    verifying
        .verify(&manifest_body_bytes(manifest), &sig)
        .map_err(|_| "签名核验失败：主体与签名不匹配（manifest 被篡改或公钥不对）".to_string())
}

// ---------- hex 小工具（与 lock.rs 各自独立实现：verify 侧零额外依赖） ----------

pub fn hex_encode(bytes: &[u8]) -> String {
    // clippy format_collect：fold 而非 map(format!)+collect（与 lock.rs 同一纪律）
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{:02x}", b);
            s
        })
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("hex 长度不是偶数".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let chars: Vec<char> = s.chars().collect();
    for pair in chars.chunks(2) {
        let hi = pair[0]
            .to_digit(16)
            .ok_or_else(|| format!("非法 hex 字符 `{}`", pair[0]))?;
        let lo = pair[1]
            .to_digit(16)
            .ok_or_else(|| format!("非法 hex 字符 `{}`", pair[1]))?;
        out.push((hi * 16 + lo) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ExecutionManifest {
        ExecutionManifest {
            manifest_version: 1,
            hir_hash: "aa".into(),
            schema_lock_hashes: vec!["bb".into()],
            dataset_identities: vec![crate::DatasetIdentity {
                identity: "table:x.csv".into(),
                lock_hash: "bb".into(),
                content_hash: Some("cc".into()),
                data_file: Some("x.csv".into()),
            }],
            semantics_version: "pebble-semantics-0.1".into(),
            compiler_version: "pebble 0.1.0".into(),
            backend: crate::BackendIdentity {
                name: "duckdb-sql".into(),
                version: String::new(),
            },
            artifact_hash: "dd".into(),
            engine_config: crate::CanonicalConfig { threads: 1 },
            terminal_options: crate::CanonicalTerminalOptions::Collect,
            engine_version_requirement: String::new(),
            signature: String::new(),
        }
    }

    #[test]
    fn unsigned_manifest_bytes_unchanged_vs_v011() {
        // 未签名 manifest 序列化不出现 signature 字段：旧 receipt 对旧 manifest 的核对不受影响
        let m = sample_manifest();
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("signature"), "空签名必须跳过序列化");
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let m = sample_manifest();
        let key = ApproverKey::from_seed([7u8; 32]);
        let mut signed = m.clone();
        signed.signature = sign_manifest(&m, &key).unwrap();
        // 验签通过
        assert!(verify_manifest_signature(&signed, &key.public).is_ok());
        // 篡改主体任一字段 → 验签失败
        let mut tampered = signed.clone();
        tampered.artifact_hash = "sha256:evil".into();
        assert!(verify_manifest_signature(&tampered, &key.public).is_err());
        // 换公钥 → 验签失败
        let other = ApproverKey::from_seed([9u8; 32]);
        assert!(verify_manifest_signature(&signed, &other.public).is_err());
        // 篡改签名本身 → 失败
        let mut bad_sig = signed.clone();
        bad_sig.signature = "ed25519:".to_string() + &"0".repeat(128);
        assert!(verify_manifest_signature(&bad_sig, &key.public).is_err());
        // 未签名 + 公钥 → 明确报「未签名」
        assert!(verify_manifest_signature(&m, &key.public).is_err());
    }

    #[test]
    fn signature_excluded_from_signed_bytes() {
        // 同一 manifest 的两个签名字段值不同，主体哈希应可区分（签名覆盖主体）
        let m = sample_manifest();
        let key = ApproverKey::from_seed([7u8; 32]);
        let mut signed = m.clone();
        signed.signature = sign_manifest(&m, &key).unwrap();
        // 对已签 manifest 再签一次（signature 置空后签名）结果一致：签名不覆盖自身
        let again = sign_manifest(&signed, &key).unwrap();
        assert_eq!(signed.signature, again);
        // manifest_hash 覆盖含签名的整体：签名后 hash 必变
        assert_eq!(
            m.compute_hash().to_string(),
            crate::Hash::of_bytes("pebble-execution-manifest-v1", &m.canonical_bytes()).to_string()
        );
        assert_ne!(m.compute_hash(), signed.compute_hash());
    }

    #[test]
    fn key_file_roundtrip_and_selfcheck() {
        let key = ApproverKey::from_seed([3u8; 32]);
        let json = key.to_json();
        let reloaded = ApproverKey::from_json(&json).unwrap();
        assert_eq!(reloaded.public, key.public);
        // 篡改 public → 密钥文件自检失败
        let bad = json.replace(&key.public, &"f".repeat(64));
        assert!(ApproverKey::from_json(&bad).is_err());
    }

    #[test]
    fn hex_tools() {
        assert_eq!(hex_encode(&[0xde, 0xad]), "dead");
        assert_eq!(hex_decode("dead").unwrap(), vec![0xde, 0xad]);
        assert!(hex_decode("abc").is_err());
    }
}
