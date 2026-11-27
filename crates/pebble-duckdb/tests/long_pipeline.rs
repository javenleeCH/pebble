//! 回归：DuckDB codegen 不再沿着流水线递归。
//!
//! 两万个 `.filter` 曾让编译器栈溢出，一条诊断都没有。原因：每个算子在 SQL 里是包住内层的
//! 一层子查询，`emit_node` 为了拿到「内层」去调用自己——n 个算子就是 n 层调用栈。
//! 现在 HIR 按拓扑序自底向上逐个生成，不再递归。
//!
//! 怎么让测试抓得住「递归」？在一个只有 256 KiB 栈的线程里给 2 000 个算子的流水线做 codegen。
//! 递归实现 2 000 层必然溢出（线程崩溃，测试进程退出，CI 变红）；迭代实现根本不需要栈。

use pebble_hir::{BackendArtifact, LockField, LockSource, LockType, SchemaLock, SourceKind};

fn orders_lock() -> SchemaLock {
    SchemaLock::build(
        LockSource::Table {
            locator: "orders.csv".into(),
        },
        vec![LockField {
            name: "amount".into(),
            ty: LockType::I64,
            optional: true,
        }],
    )
}

/// n 个 `.filter` 串成的流水线。
fn long_pipeline(n: usize) -> pebble_hir::Hir {
    let src = format!(
        "Table(\"orders.csv\") as o{}  .collect()\n",
        "  .filter(o.amount > 1)\n".repeat(n)
    );
    let mut diags = pebble_diagnostic::DiagBag::new();
    let program = pebble_parser::parse(&src, &mut diags).expect("parse");
    let catalog = pebble_semantics::Catalog::from_locks(vec![orders_lock()]);
    pebble_semantics::compile(&program, &catalog, &mut diags).expect("lower")
}

#[test]
fn codegen_of_a_long_pipeline_needs_no_call_stack() {
    const N: usize = 2000;
    const TINY_STACK: usize = 256 * 1024;

    let hir = long_pipeline(N);
    let handle = std::thread::Builder::new()
        .stack_size(TINY_STACK)
        .spawn(move || {
            let lock_of = |_: &SourceKind| Some(orders_lock());
            let sql = match pebble_duckdb::compile_sql(&hir, &lock_of).expect("codegen") {
                BackendArtifact::DuckDbSql { canonical_sql } => canonical_sql,
            };
            assert_eq!(
                sql.matches("WHERE").count(),
                N,
                "每个 filter 对应一层 WHERE"
            );
        })
        .expect("spawn");
    handle.join().expect("codegen 线程不应 panic");
}
