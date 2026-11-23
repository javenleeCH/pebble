//! Schema lock：不可变、版本化、带哈希的 Schema 快照。
//!
//! 编译只信任 lock；运行前校验实际输入与快照一致（SCH-005 / RUN-002）。
//! `data_file` 是 M0 CLI 运行期的数据定位（Table 的 locator 即路径；
//! MemoryTable 在宿主内注册，CLI 测试时由 data_file 物化）。

use crate::{Hash, Type};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockType {
    I64,
    F64,
    Bool,
    Str,
}

impl LockType {
    pub fn to_type(&self, optional: bool) -> Type {
        let base = match self {
            LockType::I64 => Type::I64,
            LockType::F64 => Type::F64,
            LockType::Bool => Type::Bool,
            LockType::Str => Type::Str,
        };
        if optional {
            base.opt()
        } else {
            base
        }
    }

    pub fn from_type(ty: &Type) -> Option<LockType> {
        match ty.base() {
            Type::I64 => Some(LockType::I64),
            Type::F64 => Some(LockType::F64),
            Type::Bool => Some(LockType::Bool),
            Type::Str => Some(LockType::Str),
            Type::Opt(_) => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LockType::I64 => "i64",
            LockType::F64 => "f64",
            LockType::Bool => "bool",
            LockType::Str => "string",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LockSource {
    Table {
        locator: String,
    },
    MemoryTable {
        name: String,
        /// CLI 运行期数据定位（宿主嵌入时不需要）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_file: Option<String>,
    },
}

impl LockSource {
    /// lock 的查找键：Table 用 locator，MemoryTable 用注册名。
    pub fn identity(&self) -> String {
        match self {
            LockSource::Table { locator } => format!("table:{}", locator),
            LockSource::MemoryTable { name, .. } => format!("memory:{}", name),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LockField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: LockType,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaLock {
    pub lock_version: u32,
    pub source: LockSource,
    pub fields: Vec<LockField>,
    /// 对 lock 主体（不含本字段自身）的 SHA-256。
    pub schema_hash: String,
}

impl SchemaLock {
    pub const LOCK_VERSION: u32 = 1;

    /// 计算 lock 主体哈希（顺序、命名、类型、可空性全部进入哈希）。
    pub fn compute_body_hash(&self) -> Hash {
        let mut body = String::new();
        body.push_str(&format!("lock_version={}\n", self.lock_version));
        match &self.source {
            LockSource::Table { locator } => {
                body.push_str(&format!("source=table\nlocator={}\n", locator));
            }
            LockSource::MemoryTable { name, .. } => {
                body.push_str(&format!("source=memory\nname={}\n", name));
            }
        }
        for f in &self.fields {
            body.push_str(&format!(
                "field={}|{}|{}\n",
                f.name,
                f.ty.as_str(),
                if f.optional { "opt" } else { "req" }
            ));
        }
        Hash::of_bytes("pebble-schema-lock-v1", body.as_bytes())
    }

    /// 校验存储的 schema_hash 与主体一致。
    pub fn validate(&self) -> Result<Hash, String> {
        if self.lock_version != Self::LOCK_VERSION {
            return Err(format!(
                "lock_version {} 不受支持（当前支持 {}）",
                self.lock_version,
                Self::LOCK_VERSION
            ));
        }
        let computed = self.compute_body_hash();
        let stored = self.schema_hash.trim_start_matches("sha256:");
        if !computed.0.iter().map(|b| format!("{:02x}", b)).collect::<String>().eq_ignore_ascii_case(stored) {
            return Err(format!(
                "schema_hash 与 lock 主体不一致（stored={}, computed={}）",
                self.schema_hash, computed
            ));
        }
        Ok(computed)
    }

    /// 从字段定义生成 lock（自动计算哈希）。
    pub fn build(source: LockSource, fields: Vec<LockField>) -> SchemaLock {
        let mut lock = SchemaLock {
            lock_version: Self::LOCK_VERSION,
            source,
            fields,
            schema_hash: String::new(),
        };
        let h = lock.compute_body_hash();
        lock.schema_hash = format!("sha256:{}", h);
        lock
    }

    /// 从 JSON 文本加载并校验。
    pub fn from_json(text: &str) -> Result<SchemaLock, String> {
        let lock: SchemaLock =
            serde_json::from_str(text).map_err(|e| format!("lock JSON 解析失败: {}", e))?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("lock 序列化")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_hash_is_stable_and_validated() {
        let lock = SchemaLock::build(
            LockSource::MemoryTable {
                name: "orders".into(),
                data_file: Some("tests/data/orders.csv".into()),
            },
            vec![
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
            ],
        );
        let json = lock.to_json();
        let reloaded = SchemaLock::from_json(&json).expect("roundtrip");
        assert_eq!(reloaded.compute_body_hash(), lock.compute_body_hash());

        // 篡改字段后校验失败
        let mut bad: SchemaLock = serde_json::from_str(&json).unwrap();
        bad.fields[0].name = "tampered".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn identity_distinguishes_sources() {
        let a = LockSource::Table {
            locator: "a.csv".into(),
        };
        let b = LockSource::MemoryTable {
            name: "a.csv".into(),
            data_file: None,
        };
        assert_ne!(a.identity(), b.identity());
    }
}
