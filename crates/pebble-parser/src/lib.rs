//! pebble-parser：词法 + 语法分析，产出 AST。
//!
//! 实现说明：原型阶段采用手写递归下降（错误码/诊断结构与规范一致）；
//! Chumsky/Ariadne 属技术路线草案，接入与否不改变本 crate 的对外契约。

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::*;
pub use parser::{PartialAst, parse};

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use pebble_diagnostic::{DiagBag, render};

    fn expect_err(src: &str, code: &str) {
        let mut diags = DiagBag::new();
        let result = parse(src, &mut diags);
        assert!(result.is_err(), "expected failure for: {}", src);
        assert!(
            diags.diags.iter().any(|d| d.code.as_str() == code),
            "expected {}, got {:?} for: {}",
            code,
            diags
                .diags
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>(),
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
        assert!(program.is_ok(), "diags: {:?}", diags.diags);
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
        assert!(program.is_ok(), "diags: {:?}", diags.diags);
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
        assert!(parse(src, &mut diags).is_err());
        let text = render(src, "demo.pebble", &diags.diags[0]);
        assert!(text.contains("LEX-001"));
        assert!(text.contains("幂运算"));
    }

    // ---------- 第 11 章：having 与 cast ----------

    #[test]
    fn parses_having_as_its_own_operator() {
        let src = "Table(\"a.csv\") as o\n  .group_by(o.a)\n  .agg(n = count())\n  .having(n > 1)\n  .collect()\n";
        let mut diags = DiagBag::new();
        let p = parse(src, &mut diags).expect("diags");
        assert_eq!(p.pipeline.ops.len(), 3);
        assert!(
            matches!(p.pipeline.ops[2], crate::ast::Op::Having { .. }),
            "having 应当解析成它自己的算子（语义层再把它当成 filter）"
        );
    }

    #[test]
    fn parses_cast_with_the_three_targets() {
        let src = "Table(\"a.csv\") as o\n  .extend({\n    a = cast(o.x as i64),\n    b = cast(o.y as f64),\n    c = cast(o.z as string),\n    d = cast(cast(o.x as f64) as i64) + 1,\n  })\n  .collect()\n";
        let mut diags = DiagBag::new();
        let p = parse(src, &mut diags).expect("diags");
        let crate::ast::Op::Extend { record, .. } = &p.pipeline.ops[0] else {
            panic!("第一个算子应当是 extend");
        };
        let targets: Vec<_> = record
            .bindings
            .iter()
            .map(|b| match &b.value {
                crate::ast::Expr::Cast { target, .. } => Some(*target),
                _ => None,
            })
            .collect();
        use crate::ast::CastTarget::*;
        assert_eq!(
            targets,
            [Some(I64), Some(F64), Some(Str), None],
            "前三个绑定是 cast，第四个是 cast 之上的加法"
        );
    }

    #[test]
    fn cast_needs_as_and_a_known_type_name() {
        for bad in [
            "cast(o.a i64)",     // 缺 as
            "cast(o.a as)",      // 缺类型名
            "cast(o.a as bool)", // 不在允许的三个之内
            "cast(o.a as int64)",
            "cast(o.a as o.b)",  // 字段引用不是类型名
            "cast(o.a as ?i64)", // 不可手写可空标记
            "cast(o.a as i64",   // 缺右括号
            "cast(o.a, i64)",
        ] {
            expect_err(
                &format!("Table(\"a.csv\") as o\n  .extend({{ x = {bad} }})\n  .collect()\n"),
                "PAR-001",
            );
        }
    }

    #[test]
    fn cast_without_parentheses_is_just_a_field_named_cast() {
        let src = "Table(\"a.csv\") as t\n  .select(cast)\n  .collect()\n";
        let mut diags = DiagBag::new();
        let p = parse(src, &mut diags).expect("`cast` 没跟括号时是普通的字段引用");
        let crate::ast::Op::Select { items, .. } = &p.pipeline.ops[0] else {
            panic!("第一个算子应当是 select");
        };
        assert!(matches!(
            &items[0].expr,
            crate::ast::Expr::Field { name, .. } if name == "cast"
        ));
    }

    #[test]
    fn a_broken_cast_damages_only_its_own_operator() {
        let src = "Table(\"a.csv\") as o\n  .filter(cast(o.a as bool))\n  .take(3)\n  .collect()\n";
        let mut diags = DiagBag::new();
        let result = parse(src, &mut diags);
        let partial = result.expect_err("期望解析失败");
        let codes: Vec<_> = diags.diags.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, ["PAR-001"], "一个根因只报一次");
        // 坏的是 filter；take 完好无损
        assert_eq!(partial.ops().len(), 1);
        assert!(matches!(
            partial.ops()[0],
            crate::ast::Op::Take { n: 3, .. }
        ));
    }
}
