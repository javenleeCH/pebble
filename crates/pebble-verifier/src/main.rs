//! pebble-verifier CLI：独立核验一个 build 目录的证据链。
//!
//! 用法：
//! ```text
//! pebble-verifier <BUILD_DIR> --pubkey <HEX | 密钥文件路径>
//! ```
//! 公钥必须来自可信渠道（审批方分发），不得取自被验目录本身。

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pebble-verifier",
    version,
    about = "Pebble 独立证据链核验器（M2）：签名 + 工件 + HIR roundtrip + 回执绑定 + 数据快照"
)]
struct Cli {
    /// build 输出目录（含 manifest.json / receipt.json / plan.sql / canonical.hir）
    build_dir: PathBuf,
    /// 审批公钥：64 位 hex，或 `pebble keygen` 生成的密钥文件路径
    #[arg(long)]
    pubkey: String,
}

fn main() {
    let cli = Cli::parse();
    let pubkey = match resolve_pubkey(&cli.pubkey) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[verifier] {}", e);
            std::process::exit(1);
        }
    };
    let report = pebble_verifier::verify_build_dir(&cli.build_dir, &pubkey);
    print!("{}", pebble_verifier::render_report(&report));
    if report.ok {
        std::process::exit(0);
    }
    std::process::exit(4);
}

/// 公钥参数：64 位 hex 直接收；否则按密钥文件（JSON 的 public 字段）。
fn resolve_pubkey(arg: &str) -> Result<String, String> {
    let a = arg.trim();
    if a.len() == 64 && a.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(a.to_ascii_lowercase());
    }
    let path = std::path::Path::new(a);
    if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读取公钥文件失败: {}", e))?;
        let key: pebble_hir::ApproverKey =
            serde_json::from_str(&text).map_err(|e| format!("公钥文件不是合法密钥 JSON: {}", e))?;
        return Ok(key.public);
    }
    Err("--pubkey 需要 64 位 hex 或密钥文件路径".into())
}
