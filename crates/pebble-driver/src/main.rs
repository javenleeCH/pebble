//! pebble CLI：build / run / verify / lock。

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pebble",
    version,
    about = "Pebble 可信数据变换编译器（M0）：canonical HIR + DuckDB SQL + 证据链"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 编译：源码 → canonical HIR/hash + DuckDB SQL 工件 + manifest
    Build {
        /// Pebble 源文件
        file: PathBuf,
        /// Schema lock 目录（默认 locks）
        #[arg(long, default_value = "locks")]
        locks: PathBuf,
        /// 输出目录（默认 target/pebble/<stem>）
        #[arg(long)]
        out: Option<PathBuf>,
        /// pin DuckDB 精确版本（run/verify 校验实际引擎版本）
        #[arg(long)]
        pin_duckdb: Option<String>,
    },
    /// 执行：数据哈希核对 + f64 预检 + 提交工件 → receipt
    Run {
        /// build 输出目录（含 manifest.json / plan.sql / validations.json）
        build_dir: PathBuf,
        /// duckdb CLI 路径（默认 PATH 或 tools/duckdb.exe）
        #[arg(long)]
        duckdb: Option<PathBuf>,
    },
    /// 核对：已审 manifest == 本次执行提交的工件？
    Verify {
        /// manifest.json
        manifest: PathBuf,
        /// receipt.json
        receipt: PathBuf,
    },
    /// 生成 Schema lock：pebble lock <table|memory> <locator|name> [--data-file f] name:type[?]...
    Lock {
        /// 输出 lock 文件路径
        #[arg(long, default_value = "lock.json")]
        out: PathBuf,
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Build {
            file,
            locks,
            out,
            pin_duckdb,
        } => {
            let out = out.unwrap_or_else(|| pebble_driver::default_out_dir(&file));
            match pebble_driver::cmd_build(&file, &locks, &out, pin_duckdb) {
                Ok(s) => {
                    println!("pebble build 完成");
                    println!("  out_dir        = {}", s.out_dir.display());
                    println!("  hir_hash       = {}", s.hir_hash);
                    println!("  artifact_hash  = {}", s.artifact_hash);
                    println!("  manifest_hash  = {}", s.manifest_hash);
                    println!(
                        "  审计视图        = {}",
                        s.out_dir.join("plan.algebra.tex").display()
                    );
                    0
                }
                Err(pebble_driver::CompileFailure(c)) => c as i32,
            }
        }
        Cmd::Run { build_dir, duckdb } => {
            match pebble_driver::cmd_run(&build_dir, duckdb.as_deref()) {
                Ok(out) => {
                    if let Some(summary) = &out.output_summary {
                        println!("{}", summary);
                    }
                    println!("receipt = {}", out.receipt_path.display());
                    if out.ok {
                        println!("run: OK");
                        0
                    } else {
                        println!("run: FAILED（详见 receipt.json）");
                        3
                    }
                }
                Err(e) => {
                    eprintln!("[pebble] {}", e);
                    1
                }
            }
        }
        Cmd::Verify { manifest, receipt } => match pebble_driver::cmd_verify(&manifest, &receipt) {
            Ok(()) => {
                println!("APPROVED PLAN == EXECUTED PLAN");
                0
            }
            Err(problems) => {
                for p in &problems {
                    eprintln!("[verify] {}", p);
                }
                println!("VERIFY FAILED");
                4
            }
        },
        Cmd::Lock { out, args } => match pebble_driver::cmd_lock(&args, &out) {
            Ok(()) => {
                println!("lock 写出: {}", out.display());
                0
            }
            Err(e) => {
                eprintln!("[pebble] {}", e);
                1
            }
        },
    };
    std::process::exit(code);
}
