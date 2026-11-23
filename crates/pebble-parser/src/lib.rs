//! pebble-parser：词法 + 语法分析，产出 AST。
//!
//! 实现说明：原型阶段采用手写递归下降（错误码/诊断结构与规范一致）；
//! Chumsky/Ariadne 属技术路线草案，接入与否不改变本 crate 的对外契约。

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::*;
pub use parser::parse;

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use pebble_diagnostic::{render, DiagBag};

    fn expect_err(src: &str, code: &str) {
        let mut diags = DiagBag::new();
        let result = parse(src, &mut diags);
        assert!(result.is_none(), "expected failure for: {}", src);
        assert!(
            diags.diags.iter().any(|d| d.code.as_str() == code),
            "expected {}, got {:?} for: {}",
            code,
            diags.diags.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            src
        );
    }

    #[test]
    fn parses_m0_revenue_demo() {
        let src = r#"
MemoryTable("orders") as o
  .filter(o.status == "paid" && o.amount != null)
  .group_by(o.customer_id)
  .agg(
    order_count = count(),
    revenue = sum(o.amount),
  )
  .filter(revenue >= 10000)
  .select(customer_id, order_count, revenue,)
  .sort(revenue desc,)
  .take(20)
  .collect()
"#;
        let mut diags = DiagBag::new();
        let program = parse(src, &mut diags);
        assert!(program.is_some(), "diags: {:?}", diags.diags);
        let p = program.unwrap();
        assert_eq!(p.pipeline.ops.len(), 7);
        assert!(matches!(
            p.pipeline.terminal,
            Some(crate::ast::TerminalOp::Collect { .. })
        ));
    }

    #[test]
    fn parses_if_chain_and_extend() {
        let src = r#"
Table("scores.parquet") as s
  .filter(s.score != null)
  .extend({
    level = if s.score >= 90 { "A" }
            else if s.score >= 60 { "B" }
            else { "C" },
    rounded = round(s.score),
  })
  .select(s.id, level, rounded)
  .print()
"#;
        let mut diags = DiagBag::new();
        let program = parse(src, &mut diags);
        assert!(program.is_some(), "diags: {:?}", diags.diags);
    }

    #[test]
    fn rejects_lambda_pipe() {
        expect_err(
            "Table(\"a.csv\")\n  .filter(|u| u.age >= 1)\n  .collect()\n",
            "LEX-001",
        );
    }

    #[test]
    fn rejects_on_colon() {
        expect_err(
            "Table(\"a.csv\") as a\n  .join(Table(\"b.csv\") as b, on: a.x == b.x)\n  .collect()\n",
            "PAR-007",
        );
    }

    #[test]
    fn rejects_unnamed_select_expr() {
        expect_err(
            "Table(\"a.csv\")\n  .select(u.a + 1)\n  .collect()\n",
            "PAR-006",
        );
    }

    #[test]
    fn rejects_if_without_else() {
        expect_err(
            "Table(\"a.csv\")\n  .extend({ x = if u.a { 1 } })\n  .collect()\n",
            "PAR-004",
        );
    }

    #[test]
    fn rejects_agg_binding_not_call() {
        expect_err(
            "Table(\"a.csv\")\n  .agg(x = 1 + 1)\n  .collect()\n",
            "PAR-005",
        );
    }

    #[test]
    fn rejects_extend_without_record() {
        expect_err(
            "Table(\"a.csv\")\n  .extend(u.a + 1)\n  .collect()\n",
            "PAR-003",
        );
    }

    #[test]
    fn rejects_int_literal_overflow() {
        expect_err(
            "Table(\"a.csv\")\n  .take(99999999999999999999999)\n  .collect()\n",
            "SEM-016",
        );
    }

    #[test]
    fn rejects_op_after_terminal() {
        expect_err(
            "Table(\"a.csv\")\n  .collect()\n  .filter(u.a > 0)\n",
            "PAR-002",
        );
    }

    #[test]
    fn lexer_and_render_pipeline() {
        // LEX 错误应被渲染为带行号/提示的文本
        let src = "Table(\"a.csv\")\n  .filter(u.a ^ 2 > 1)\n  .collect()\n";
        let mut diags = DiagBag::new();
        assert!(parse(src, &mut diags).is_none());
        let text = render(src, "demo.pebble", &diags.diags[0]);
        assert!(text.contains("LEX-001"));
        assert!(text.contains("幂运算"));
    }
}
