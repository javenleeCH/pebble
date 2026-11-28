//! 回归：DuckDB codegen 不再沿着流水线递归，也不再给每个算子套一层子查询。
//!
//! 两万个 `.filter` 曾经两次让东西崩溃：先是编译器（每个算子一层子查询，`emit_node` 为了取内层
//! 调用自己——栈溢出）；修好之后换成 DuckDB（几百层嵌套它就解析失败，第 10 章）。
//!
//! 两个检查，同一个输入：
//! - 在一个只有 256 KiB 栈的线程里给 2 000 个算子的流水线做 codegen。
//!   递归实现 2 000 层必然溢出（线程崩溃，测试进程退出，CI 变红）；迭代实现根本不需要栈。
//! - 生成的 SQL 只有两层嵌套：连续的 filter 并进了同一个 WHERE，与个数无关。

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
fn codegen_of_a_long_pipeline_needs_no_call_stack_and_two_layers() {
    const N: usize = 2000;
    const TINY_STACK: usize = 256 * 1024;

    let hir = long_pipeline(N);
    let handle = std::thread::Builder::new()
        .stack_size(TINY_STACK)
        .spawn(move || {
            let lock_of = |_: &SourceKind| Some(orders_lock());
            let sql = match pebble_duckdb::compile_sql(&hir, &lock_of, 512).expect("codegen") {
                BackendArtifact::DuckDbSql { canonical_sql } => canonical_sql,
            };
            // 谓词一个不少
            assert_eq!(
                sql.matches("((\"f0\") > (1))").count(),
                N,
                "每个 filter 一个谓词"
            );
            // 但它们都在同一层的同一个 WHERE 里
            assert_eq!(sql.matches(" WHERE ").count(), 1, "{sql}");
            assert_eq!(sql.matches("FROM (").count(), 2, "源 + final：{sql}");
        })
        .expect("spawn");
    handle.join().expect("codegen 线程不应 panic");
}
