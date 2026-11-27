//! pebble-latex：同一 canonical HIR 的最小单向关系代数/LaTeX 审计视图。
//!
//! 边界声明：本视图是 HIR 的人类可读表示，不是执行正确性证明。
//! sort/take 是「代数外表示层算子」，以 τ/topN 标注并显式注明。

use pebble_hir::{
    AggFunc, BinOp, BuiltinFn, FieldId, Hir, HirNode, SourceKind, THirExpr, TransformKind, Type,
    UnaryOp,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// 生成关系代数单行公式（LaTeX 数学模式片段）。
pub fn algebra_view(hir: &Hir) -> String {
    let mut formula = algebra_formulas(hir)[hir.root as usize].clone();
    if let HirNode::Terminal {
        kind: pebble_hir::TerminalKind::Write { path },
        ..
    } = &hir.nodes[hir.root as usize]
    {
        let _ = write!(
            &mut formula,
            " \\;\\longrightarrow\\; \\texttt{{{}}}",
            escape(path)
        );
    }
    formula
}

/// 生成带语义注释的完整 LaTeX 文档。
pub fn latex_document(hir: &Hir) -> String {
    let formula = algebra_view(hir);
    let mut doc = String::new();
    let _ = writeln!(doc, "% pebble-latex artifact v1 (canonical view)");
    let _ = writeln!(doc, "% 语义版本: {}", hir.semantics_version);
    let _ = writeln!(doc, "\\[");
    let _ = writeln!(doc, "  {}", formula);
    let _ = writeln!(doc, "\\]");
    let _ = writeln!(
        doc,
        "% 说明：\\sigma 选择、\\pi 投影、\\varepsilon 扩展、\\delta 去重、\\gamma 分组聚合、\\bowtie 连接；\
         \\tau 排序与 topN 取前 N 为代数外表示层算子。"
    );
    doc
}

fn field_names(hir: &Hir) -> BTreeMap<FieldId, String> {
    let mut names = BTreeMap::new();
    for node in &hir.nodes {
        for f in &node.base().output.fields {
            names.insert(f.id, f.name.clone());
        }
    }
    names
}

/// 自底向上生成每个节点的代数式，按节点编号存放。
///
/// 与 DuckDB codegen 是同一个道理：HIR 的节点按拓扑序存放，生成某节点时它的输入早已就绪，
/// 直接从数组里取即可，不必「先递归地生成内层」——n 个算子的流水线不该在调用栈上叠 n 层。
fn algebra_formulas(hir: &Hir) -> Vec<String> {
    let owned = field_names(hir);
    let names = &owned;
    let mut out: Vec<String> = Vec::with_capacity(hir.nodes.len());
    for node in &hir.nodes {
        let text = match node {
            HirNode::Source { source, kind, .. } => match kind {
                SourceKind::Table { locator, .. } => {
                    format!("R_{{{}}}", escape(&locator.replace('\\', "")))
                }
                SourceKind::MemoryTable { name, .. } => {
                    let _ = source;
                    format!("M_{{{}}}", escape(name))
                }
            },
            HirNode::Transform { base, kind } => {
                let inner = &out[base.inputs[0] as usize];
                match kind {
                    TransformKind::Filter { pred } => {
                        format!(
                            "\\sigma_{{{}}}\\left({}\\right)",
                            emit_expr(pred, names),
                            inner
                        )
                    }
                    TransformKind::Extend { bindings } => {
                        let list: Vec<String> = bindings
                            .iter()
                            .map(|b| format!("{}={}", escape(&b.name), emit_expr(&b.value, names)))
                            .collect();
                        format!(
                            "\\varepsilon_{{{}}}\\left({}\\right)",
                            list.join(",\\; "),
                            inner
                        )
                    }
                    TransformKind::Select { projections } => {
                        let list: Vec<String> =
                            projections.iter().map(|p| escape(&p.name)).collect();
                        format!("\\pi_{{{}}}\\left({}\\right)", list.join(",\\; "), inner)
                    }
                    TransformKind::Distinct => format!("\\delta\\left({}\\right)", inner),
                    TransformKind::Sort { keys } => {
                        let list: Vec<String> = keys
                            .iter()
                            .map(|k| {
                                let name = emit_expr(&k.expr, names);
                                if k.descending {
                                    format!("{}\\downarrow", name)
                                } else {
                                    format!("{}\\uparrow", name)
                                }
                            })
                            .collect();
                        format!("\\tau_{{{}}}\\left({}\\right)", list.join(",\\; "), inner)
                    }
                    TransformKind::Take { n } => {
                        format!("\\mathrm{{top}}_{{{}}}\\left({}\\right)", n, inner)
                    }
                    TransformKind::Join { on } => {
                        let right = &out[base.inputs[1] as usize];
                        format!(
                            "\\left({}\\right) \\bowtie_{{{}}} \\left({}\\right)",
                            inner,
                            emit_expr(on, names),
                            right
                        )
                    }
                }
            }
            HirNode::Aggregate {
                base,
                group_keys,
                bindings,
            } => {
                let inner = &out[base.inputs[0] as usize];
                let keys: Vec<String> = group_keys.iter().map(|k| escape(&k.name)).collect();
                let aggs: Vec<String> = bindings
                    .iter()
                    .map(|b| {
                        let func = agg_symbol(&b.call.func);
                        let arg = b
                            .call
                            .arg
                            .as_ref()
                            .map(|a| emit_expr(a, names))
                            .unwrap_or_default();
                        format!("{}={}({})", escape(&b.name), func, arg)
                    })
                    .collect();
                let mut spec = keys.join(",\\; ");
                if !aggs.is_empty() {
                    if !spec.is_empty() {
                        spec.push_str(";\\; ");
                    }
                    spec.push_str(&aggs.join(",\\; "));
                }
                format!("\\gamma_{{{}}}\\left({}\\right)", spec, inner)
            }
            HirNode::Terminal { base, .. } => out[base.inputs[0] as usize].clone(),
        };
        out.push(text);
    }
    out
}

fn agg_symbol(f: &AggFunc) -> &'static str {
    match f {
        AggFunc::Count => "\\mathrm{count}",
        AggFunc::Sum => "\\mathrm{sum}",
        AggFunc::Min => "\\mathrm{min}",
        AggFunc::Max => "\\mathrm{max}",
        AggFunc::Avg => "\\mathrm{avg}",
    }
}

fn emit_expr(e: &THirExpr, names: &BTreeMap<FieldId, String>) -> String {
    match e {
        THirExpr::Literal { value, .. } => match value {
            pebble_hir::Literal::Int(v) => v.to_string(),
            pebble_hir::Literal::Float(v) => format!("{:?}", v),
            pebble_hir::Literal::Str(s) => format!("\\text{{{}}}", escape(s)),
            pebble_hir::Literal::Bool(b) => {
                format!("\\mathrm{{{}}}", if *b { "true" } else { "false" })
            }
            pebble_hir::Literal::Null => "\\mathrm{null}".to_string(),
        },
        THirExpr::Field { field, .. } => {
            format!(
                "\\mathrm{{{}}}",
                escape(names.get(field).map(|s| s.as_str()).unwrap_or("?"))
            )
        }
        THirExpr::Binary {
            op, left, right, ..
        } => {
            let (o, wrap) = match op {
                BinOp::Add => ("+", false),
                BinOp::Sub => ("-", false),
                BinOp::Mul => ("\\cdot", false),
                BinOp::Div => ("/", true),
                BinOp::Mod => ("\\bmod", true),
                BinOp::Eq => ("=", false),
                BinOp::Neq => ("\\ne", false),
                BinOp::Gt => (">", false),
                BinOp::Lt => ("<", false),
                BinOp::Gte => ("\\ge", false),
                BinOp::Lte => ("\\le", false),
                BinOp::And => ("\\wedge", true),
                BinOp::Or => ("\\vee", true),
            };
            let l = emit_expr(left, names);
            let r = emit_expr(right, names);
            if wrap {
                format!("\\left({} {} {}\\right)", l, o, r)
            } else {
                format!("{} {} {}", l, o, r)
            }
        }
        THirExpr::Unary { op, expr, .. } => match op {
            UnaryOp::Negate => format!("-{}", emit_expr(expr, names)),
            UnaryOp::Not => format!("\\neg {}", emit_expr(expr, names)),
        },
        THirExpr::If {
            condition,
            then,
            else_,
            ..
        } => format!(
            "\\begin{{cases}} {} & \\text{{if }} {} \\\\ {} & \\text{{else}} \\end{{cases}}",
            emit_expr(then, names),
            emit_expr(condition, names),
            emit_expr(else_, names)
        ),
        THirExpr::Call { name, args, .. } => {
            let fname = match name {
                BuiltinFn::Abs => "\\mathrm{abs}",
                BuiltinFn::Round => "\\mathrm{round}",
                BuiltinFn::Lower => "\\mathrm{lower}",
                BuiltinFn::Upper => "\\mathrm{upper}",
                BuiltinFn::Trim => "\\mathrm{trim}",
                BuiltinFn::Length => "\\mathrm{length}",
                BuiltinFn::Contains => "\\mathrm{contains}",
                BuiltinFn::IsNull => "\\mathrm{is\\_null}",
            };
            let _ = Type::I64;
            let list: Vec<String> = args.iter().map(|a| emit_expr(a, names)).collect();
            format!("{}\\left({}\\right)", fname, list.join(",\\; "))
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\textbackslash{}")
        .replace('_', "\\_")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('&', "\\&")
        .replace('#', "\\#")
        .replace('%', "\\%")
        .replace('$', "\\$")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pebble_diagnostic::Span;
    use pebble_hir::{
        FieldOrigin, FieldSchema, Hash, Literal, NodeBase, NodeId, Schema, TerminalKind,
    };

    #[test]
    fn escapes_latex_specials() {
        assert!(escape("a_b&c").contains("\\_"));
        assert!(escape("x&y").contains("\\&"));
    }

    /// n 个 filter 串成一条流水线的 HIR。手工搭建：本 crate 只依赖 pebble-hir。
    fn long_chain(n: usize) -> Hir {
        let span = Span::new(0, 0);
        let schema = Schema {
            fields: vec![FieldSchema {
                id: 0,
                name: "amount".into(),
                ty: Type::I64,
                origin: FieldOrigin::Source {
                    source: 0,
                    source_field: "amount".into(),
                },
            }],
        };
        let mut nodes = Vec::with_capacity(n + 2);
        nodes.push(HirNode::Source {
            base: NodeBase {
                id: 0,
                inputs: vec![],
                output: schema.clone(),
                span,
            },
            source: 0,
            kind: SourceKind::Table {
                locator: "t.csv".into(),
                schema_lock_hash: Hash([0u8; 32]),
            },
        });
        for i in 1..=n {
            let id = i as NodeId;
            nodes.push(HirNode::Transform {
                base: NodeBase {
                    id,
                    inputs: vec![id - 1],
                    output: schema.clone(),
                    span,
                },
                kind: TransformKind::Filter {
                    pred: THirExpr::Binary {
                        op: BinOp::Gt,
                        left: Box::new(THirExpr::Field {
                            field: 0,
                            ty: Type::I64,
                            span,
                        }),
                        right: Box::new(THirExpr::Literal {
                            value: Literal::Int(1),
                            ty: Type::I64,
                            span,
                        }),
                        ty: Type::Opt(Box::new(Type::Bool)),
                        span,
                    },
                },
            });
        }
        let root = (n + 1) as NodeId;
        nodes.push(HirNode::Terminal {
            base: NodeBase {
                id: root,
                inputs: vec![n as NodeId],
                output: schema.clone(),
                span,
            },
            kind: TerminalKind::Collect,
        });
        Hir {
            nodes,
            root,
            semantics_version: pebble_hir::SEMANTICS_VERSION.to_string(),
        }
    }

    /// 回归：代数视图曾沿着流水线递归（`emit_node` 为了取内层调用自己），
    /// 两万个算子的流水线会让编译器栈溢出。现在自底向上逐个生成。
    ///
    /// 在只有 256 KiB 栈的线程里给 2 000 个算子出视图：递归实现必然溢出，迭代实现不需要栈。
    #[test]
    fn algebra_view_of_a_long_pipeline_needs_no_call_stack() {
        const N: usize = 2000;
        const TINY_STACK: usize = 256 * 1024;
        let hir = long_chain(N);
        let handle = std::thread::Builder::new()
            .stack_size(TINY_STACK)
            .spawn(move || {
                let formula = algebra_view(&hir);
                assert_eq!(formula.matches("\\sigma_").count(), N, "每个 filter 一个 σ");
            })
            .expect("spawn");
        handle.join().expect("渲染线程不应 panic");
    }
}
