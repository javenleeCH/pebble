//! Canonical HIR 序列化与内容寻址哈希（hir.md §11）。
//!
//! 规则：
//! - 节点按从源到终端的规范拓扑序编号；本实现按构造顺序（源先于消费者）。
//! - Schema 字段顺序、FieldId/SourceId、表达式结构、显式类型、字面量位模式、
//!   终端选项均进入序列化。
//! - 剔除 Span、注释、空白、源别名拼写、时间戳。
//! - 浮点以 IEEE-754 位模式编码；整数/字符串长度前缀编码。
//! - 格式与语义均版本化；语义版本进入哈希。

use crate::{
    AggFunc, BinOp, BuiltinFn, Hash, Hir, HirNode, Literal, SourceKind, TerminalKind,
    TransformKind, Type, UnaryOp,
};

pub fn canonical_bytes(hir: &Hir) -> Vec<u8> {
    let mut w = ByteWriter::default();
    w.tag(b"H");
    w.str(&hir.semantics_version);
    w.u32(hir.nodes.len() as u32);
    w.u32(hir.root);
    for node in &hir.nodes {
        match node {
            HirNode::Source { base, source, kind } => {
                w.tag(b"S");
                w.u32(*source);
                match kind {
                    SourceKind::Table {
                        locator,
                        schema_lock_hash,
                    } => {
                        w.tag(b"T");
                        w.str(locator);
                        w.hash(schema_lock_hash);
                    }
                    SourceKind::MemoryTable {
                        name,
                        schema_lock_hash,
                    } => {
                        w.tag(b"M");
                        w.str(name);
                        w.hash(schema_lock_hash);
                    }
                }
                encode_base(&mut w, base);
            }
            HirNode::Transform { base, kind } => {
                w.tag(b"R");
                match kind {
                    TransformKind::Filter { pred } => {
                        w.tag(b"F");
                        encode_expr(&mut w, pred);
                    }
                    TransformKind::Extend { bindings } => {
                        w.tag(b"E");
                        w.u32(bindings.len() as u32);
                        for b in bindings {
                            w.u32(b.output);
                            w.str(&b.name);
                            encode_expr(&mut w, &b.value);
                        }
                    }
                    TransformKind::Select { projections } => {
                        w.tag(b"P");
                        w.u32(projections.len() as u32);
                        for p in projections {
                            w.u32(p.output);
                            w.str(&p.name);
                            encode_expr(&mut w, &p.expr);
                        }
                    }
                    TransformKind::Distinct => w.tag(b"D"),
                    TransformKind::Sort { keys } => {
                        w.tag(b"O");
                        w.u32(keys.len() as u32);
                        for k in keys {
                            w.tag(if k.descending { b"d" } else { b"a" });
                            encode_expr(&mut w, &k.expr);
                        }
                    }
                    TransformKind::Take { n } => {
                        w.tag(b"K");
                        w.i64(*n);
                    }
                    TransformKind::Join { on } => {
                        w.tag(b"J");
                        encode_expr(&mut w, on);
                    }
                }
                encode_base(&mut w, base);
            }
            HirNode::Aggregate {
                base,
                group_keys,
                bindings,
            } => {
                w.tag(b"G");
                w.u32(group_keys.len() as u32);
                for k in group_keys {
                    w.u32(k.output);
                    w.str(&k.name);
                    encode_expr(&mut w, &k.value);
                }
                w.u32(bindings.len() as u32);
                for b in bindings {
                    w.u32(b.output);
                    w.str(&b.name);
                    encode_agg_call(&mut w, &b.call);
                }
                encode_base(&mut w, base);
            }
            HirNode::Terminal { base, kind } => {
                w.tag(b"X");
                match kind {
                    TerminalKind::Write { path } => {
                        w.tag(b"W");
                        w.str(path);
                    }
                    TerminalKind::Collect => w.tag(b"C"),
                    TerminalKind::Print => w.tag(b"N"),
                }
                encode_base(&mut w, base);
            }
        }
    }
    w.out
}

pub fn hir_hash(hir: &Hir) -> Hash {
    Hash::of_bytes("pebble-canonical-hir-v1", &canonical_bytes(hir))
}

fn encode_base(w: &mut ByteWriter, base: &crate::NodeBase) {
    w.u32(base.id);
    w.u32(base.inputs.len() as u32);
    for &i in &base.inputs {
        w.u32(i);
    }
    w.u32(base.output.fields.len() as u32);
    for f in &base.output.fields {
        w.u32(f.id);
        w.str(&f.name);
        encode_type(w, &f.ty);
        match &f.origin {
            crate::FieldOrigin::Source {
                source,
                source_field,
            } => {
                w.tag(b"s");
                w.u32(*source);
                w.str(source_field);
            }
            crate::FieldOrigin::Derived { node, binding } => {
                w.tag(b"v");
                w.u32(*node);
                w.str(binding);
            }
        }
    }
}

fn encode_type(w: &mut ByteWriter, ty: &Type) {
    match ty {
        Type::I64 => w.tag(b"i"),
        Type::F64 => w.tag(b"f"),
        Type::Bool => w.tag(b"b"),
        Type::Str => w.tag(b"c"),
        Type::Opt(inner) => {
            w.tag(b"?");
            encode_type(w, inner);
        }
    }
}

fn encode_agg_call(w: &mut ByteWriter, call: &crate::AggCallH) {
    match call.func {
        AggFunc::Count => w.tag(b"c"),
        AggFunc::Sum => w.tag(b"s"),
        AggFunc::Min => w.tag(b"m"),
        AggFunc::Max => w.tag(b"M"),
        AggFunc::Avg => w.tag(b"a"),
    }
    match &call.arg {
        Some(e) => {
            w.tag(b"1");
            encode_expr(w, e);
        }
        None => w.tag(b"0"),
    }
}

fn encode_expr(w: &mut ByteWriter, e: &crate::THirExpr) {
    match e {
        crate::THirExpr::Literal { value, ty, .. } => {
            w.tag(b"L");
            match value {
                Literal::Int(v) => {
                    w.tag(b"i");
                    w.i64(*v);
                }
                Literal::Float(v) => {
                    w.tag(b"f");
                    w.f64(*v);
                }
                Literal::Str(s) => {
                    w.tag(b"c");
                    w.str(s);
                }
                Literal::Bool(v) => {
                    w.tag(if *v { b"1" } else { b"0" });
                }
                Literal::Null => w.tag(b"n"),
            }
            encode_type(w, ty);
        }
        crate::THirExpr::Field { field, ty, .. } => {
            w.tag(b"F");
            w.u32(*field);
            encode_type(w, ty);
        }
        crate::THirExpr::Binary {
            op,
            left,
            right,
            ty,
            ..
        } => {
            w.tag(b"B");
            encode_binop(w, *op);
            encode_expr(w, left);
            encode_expr(w, right);
            encode_type(w, ty);
        }
        crate::THirExpr::Unary { op, expr, ty, .. } => {
            w.tag(b"U");
            match op {
                UnaryOp::Negate => w.tag(b"-"),
                UnaryOp::Not => w.tag(b"!"),
            }
            encode_expr(w, expr);
            encode_type(w, ty);
        }
        crate::THirExpr::If {
            condition,
            then,
            else_,
            ty,
            ..
        } => {
            w.tag(b"I");
            encode_expr(w, condition);
            encode_expr(w, then);
            encode_expr(w, else_);
            encode_type(w, ty);
        }
        crate::THirExpr::Call { name, args, ty, .. } => {
            w.tag(b"C");
            match name {
                BuiltinFn::Abs => w.tag(b"a"),
                BuiltinFn::Round => w.tag(b"r"),
                BuiltinFn::Lower => w.tag(b"l"),
                BuiltinFn::Upper => w.tag(b"u"),
                BuiltinFn::Trim => w.tag(b"t"),
                BuiltinFn::Length => w.tag(b"g"),
                BuiltinFn::Contains => w.tag(b"k"),
                BuiltinFn::IsNull => w.tag(b"z"),
            }
            w.u32(args.len() as u32);
            for a in args {
                encode_expr(w, a);
            }
            encode_type(w, ty);
        }
        // v0.1.1 新增。`V` 取「type conVersion」之意，是这个文件里此前从未用过的标签
        // （节点层的 `K` 已经表示 Take；让每个标签在全文件唯一，将来写解码器不用区分位置）。
        // 既有变体的编码一个字节都没变，所以不含 cast 的程序哈希不变（hir.md §6 第 8 条）
        crate::THirExpr::Cast { expr, to, ty, .. } => {
            w.tag(b"V");
            encode_type(w, to);
            encode_expr(w, expr);
            encode_type(w, ty);
        }
    }
}

fn encode_binop(w: &mut ByteWriter, op: BinOp) {
    match op {
        BinOp::Add => w.tag(b"+"),
        BinOp::Sub => w.tag(b"-"),
        BinOp::Mul => w.tag(b"*"),
        BinOp::Div => w.tag(b"/"),
        BinOp::Mod => w.tag(b"%"),
        BinOp::Eq => w.tag(b"="),
        BinOp::Neq => w.tag(b"!"),
        BinOp::Gt => w.tag(b">"),
        BinOp::Lt => w.tag(b"<"),
        BinOp::Gte => w.tag(b"G"),
        BinOp::Lte => w.tag(b"L"),
        BinOp::And => w.tag(b"&"),
        BinOp::Or => w.tag(b"|"),
    }
}

#[derive(Default)]
struct ByteWriter {
    out: Vec<u8>,
}

impl ByteWriter {
    fn tag(&mut self, t: &[u8; 1]) {
        self.out.push(t[0]);
    }

    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    fn f64(&mut self, v: f64) {
        self.out.extend_from_slice(&v.to_bits().to_le_bytes());
    }

    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.out.extend_from_slice(s.as_bytes());
    }

    fn hash(&mut self, h: &Hash) {
        self.out.extend_from_slice(&h.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldOrigin, FieldSchema, NodeBase, Schema, THirExpr};
    use pebble_diagnostic::Span;

    /// 一个只有「源 → filter → collect」的程序，filter 的谓词由 `pred` 给出。
    /// 载荷都很小、不含大写字母：这样编码里出现的标签字节不会被载荷「碰巧」撞上。
    fn filter_over(pred: THirExpr) -> Hir {
        let span = Span::new(0, 0);
        let schema = Schema {
            fields: vec![FieldSchema {
                id: 0,
                name: "x".into(),
                ty: Type::F64,
                origin: FieldOrigin::Source {
                    source: 0,
                    source_field: "x".into(),
                },
            }],
        };
        Hir {
            nodes: vec![
                HirNode::Source {
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
                },
                HirNode::Transform {
                    base: NodeBase {
                        id: 1,
                        inputs: vec![0],
                        output: schema.clone(),
                        span,
                    },
                    kind: TransformKind::Filter { pred },
                },
                HirNode::Terminal {
                    base: NodeBase {
                        id: 2,
                        inputs: vec![1],
                        output: schema,
                        span,
                    },
                    kind: TerminalKind::Collect,
                },
            ],
            root: 2,
            semantics_version: crate::SEMANTICS_VERSION.to_string(),
        }
    }

    /// `cast(x as i64) > 1`
    fn cast_predicate() -> THirExpr {
        let span = Span::new(0, 0);
        THirExpr::Binary {
            op: BinOp::Gt,
            left: Box::new(THirExpr::Cast {
                expr: Box::new(THirExpr::Field {
                    field: 0,
                    ty: Type::F64,
                    span,
                }),
                to: Type::I64,
                // f64 → i64 可能失败：结果一律可空
                ty: Type::Opt(Box::new(Type::I64)),
                span,
            }),
            right: Box::new(THirExpr::Literal {
                value: Literal::Int(1),
                ty: Type::I64,
                span,
            }),
            ty: Type::Opt(Box::new(Type::Bool)),
            span,
        }
    }

    #[test]
    fn cast_is_encoded_with_a_tag_no_other_construct_uses() {
        let with_cast = canonical_bytes(&filter_over(cast_predicate()));
        let plain = canonical_bytes(&filter_over(THirExpr::Literal {
            value: Literal::Bool(true),
            ty: Type::Bool,
            span: Span::new(0, 0),
        }));
        let pos = with_cast
            .iter()
            .position(|&b| b == b'V')
            .expect("cast 应当以 V 编码");
        // V 之后依次是：目标基础类型（i64）、操作数、结果类型
        assert_eq!(with_cast[pos + 1], b'i', "V 之后是目标基础类型 i64");
        // 同一个程序去掉 cast 之后，整份编码里没有 V：这个标签没有别的构造在用
        assert!(!plain.contains(&b'V'));
        // 编码是确定的
        assert_eq!(with_cast, canonical_bytes(&filter_over(cast_predicate())));
        // 两个程序当然不同
        assert_ne!(with_cast, plain);
    }
}
