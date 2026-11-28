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

// ===========================================================================
// 反序列化（ch12：M2 独立 verifier 的前置件）
//
// 与写入器同文件维护：**格式的规范只有一个归属**——写、读、改格式必须三处同步。
// 用途不是「从文件恢复编译器状态」，而是 verifier 的最小信任路径：
// 解析 canonical 字节 → 重建结构 → 重算哈希，**绕开 parser/semantics/codegen**。
// Span 不进编码，反序列化一律以 Span::new(0,0) 占位——哈希不覆盖 Span，
// 所以占位不影响 roundtrip 哈希。
// ===========================================================================

use crate::{
    AggBindingH, AggCallH, FieldOrigin, FieldSchema, NodeBase, Schema, SortKeyH, THirExpr,
};
use pebble_diagnostic::Span;

/// 解析 canonical 字节为 Hir（占位 Span）。格式不符即 Err（带字节偏移）。
pub fn canonical_hir(bytes: &[u8]) -> Result<Hir, String> {
    let mut r = ByteReader { bytes, pos: 0 };
    let hir = read_hir(&mut r)?;
    if r.pos != bytes.len() {
        return Err(format!(
            "canonical 字节有尾随垃圾：期望在 {} 结束，总长 {}",
            r.pos,
            bytes.len()
        ));
    }
    Ok(hir)
}

/// 独立 verifier 的核心检查：解析 canonical 字节并重算哈希。
/// `bytes` 与 manifest.hir_hash 出自同一来源才有意义——verifier 拿磁盘上的
/// canonical.hir 与 manifest 里的哈希对（见 pebble-verifier）。
pub fn roundtrip_hash(bytes: &[u8]) -> Result<Hash, String> {
    Ok(hir_hash(&canonical_hir(bytes)?))
}

struct ByteReader<'b> {
    bytes: &'b [u8],
    pos: usize,
}

impl ByteReader<'_> {
    fn take(&mut self) -> Result<u8, String> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| format!("canonical 字节意外结束（偏移 {}）", self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn expect(&mut self, want: u8, what: &str) -> Result<(), String> {
        let got = self.take()?;
        if got != want {
            return Err(format!(
                "{}：期望标签 `{:?}`，实际 `{:?}`（偏移 {}）",
                what,
                want as char,
                got as char,
                self.pos - 1
            ));
        }
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut b = [0u8; 4];
        for slot in &mut b {
            *slot = self.take()?;
        }
        Ok(u32::from_le_bytes(b))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let mut b = [0u8; 8];
        for slot in &mut b {
            *slot = self.take()?;
        }
        Ok(i64::from_le_bytes(b))
    }

    fn f64(&mut self) -> Result<f64, String> {
        let mut b = [0u8; 8];
        for slot in &mut b {
            *slot = self.take()?;
        }
        Ok(f64::from_bits(u64::from_le_bytes(b)))
    }

    fn str(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| format!("字符串长度溢出（偏移 {}）", self.pos))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| format!("字符串字节不足（偏移 {}，长度 {}）", self.pos, len))?;
        let s = String::from_utf8(slice.to_vec())
            .map_err(|_| format!("字符串不是合法 UTF-8（偏移 {}）", self.pos))?;
        self.pos = end;
        Ok(s)
    }

    fn hash(&mut self) -> Result<Hash, String> {
        let mut b = [0u8; 32];
        for slot in &mut b {
            *slot = self.take()?;
        }
        Ok(Hash(b))
    }

    /// 计数防御：拒绝明显不合理的长度，避免被恶意字节撑爆内存。
    fn count(&mut self, what: &str) -> Result<usize, String> {
        let n = self.u32()? as usize;
        if n > self.bytes.len() {
            return Err(format!("{} 数量 {} 超过剩余字节，拒绝解析", what, n));
        }
        Ok(n)
    }
}

const PLACEHOLDER: fn() -> Span = || Span::new(0, 0);

fn read_hir(r: &mut ByteReader) -> Result<Hir, String> {
    r.expect(b'H', "文件头")?;
    let semantics_version = r.str()?;
    let n = r.count("节点")?;
    let root = r.u32()?;
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        nodes.push(read_node(r)?);
    }
    Ok(Hir {
        nodes,
        root,
        semantics_version,
    })
}

fn read_node(r: &mut ByteReader) -> Result<HirNode, String> {
    let kind_tag = r.take()?;
    match kind_tag {
        b'S' => {
            let source = r.u32()?;
            let kind = match r.take()? {
                b'T' => SourceKind::Table {
                    locator: r.str()?,
                    schema_lock_hash: r.hash()?,
                },
                b'M' => SourceKind::MemoryTable {
                    name: r.str()?,
                    schema_lock_hash: r.hash()?,
                },
                other => {
                    return Err(format!(
                        "源种类标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let base = read_base(r)?;
            Ok(HirNode::Source { base, source, kind })
        }
        b'R' => {
            let kind = match r.take()? {
                b'F' => TransformKind::Filter {
                    pred: read_expr(r)?,
                },
                b'E' => {
                    let n = r.count("extend 绑定")?;
                    let mut bindings = Vec::with_capacity(n);
                    for _ in 0..n {
                        let output = r.u32()?;
                        let name = r.str()?;
                        let value = read_expr(r)?;
                        bindings.push(crate::Binding {
                            output,
                            name,
                            value,
                        });
                    }
                    TransformKind::Extend { bindings }
                }
                b'P' => {
                    let n = r.count("select 投影")?;
                    let mut projections = Vec::with_capacity(n);
                    for _ in 0..n {
                        let output = r.u32()?;
                        let name = r.str()?;
                        let expr = read_expr(r)?;
                        projections.push(crate::Projection { expr, output, name });
                    }
                    TransformKind::Select { projections }
                }
                b'D' => TransformKind::Distinct,
                b'O' => {
                    let n = r.count("sort 键")?;
                    let mut keys = Vec::with_capacity(n);
                    for _ in 0..n {
                        let descending = match r.take()? {
                            b'd' => true,
                            b'a' => false,
                            other => {
                                return Err(format!(
                                    "sort 方向标签非法 `{:?}`（偏移 {}）",
                                    other as char,
                                    r.pos - 1
                                ));
                            }
                        };
                        let expr = read_expr(r)?;
                        keys.push(SortKeyH { expr, descending });
                    }
                    TransformKind::Sort { keys }
                }
                b'K' => TransformKind::Take { n: r.i64()? },
                b'J' => TransformKind::Join { on: read_expr(r)? },
                other => {
                    return Err(format!(
                        "变换种类标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let base = read_base(r)?;
            Ok(HirNode::Transform { base, kind })
        }
        b'G' => {
            let n_keys = r.count("分组键")?;
            let mut group_keys = Vec::with_capacity(n_keys);
            for _ in 0..n_keys {
                let output = r.u32()?;
                let name = r.str()?;
                let value = read_expr(r)?;
                group_keys.push(crate::GroupKeyH {
                    value,
                    output,
                    name,
                });
            }
            let n_bindings = r.count("聚合绑定")?;
            let mut bindings = Vec::with_capacity(n_bindings);
            for _ in 0..n_bindings {
                let output = r.u32()?;
                let name = r.str()?;
                let call = read_agg_call(r)?;
                bindings.push(AggBindingH { output, name, call });
            }
            let base = read_base(r)?;
            Ok(HirNode::Aggregate {
                base,
                group_keys,
                bindings,
            })
        }
        b'X' => {
            let kind = match r.take()? {
                b'W' => TerminalKind::Write { path: r.str()? },
                b'C' => TerminalKind::Collect,
                b'N' => TerminalKind::Print,
                other => {
                    return Err(format!(
                        "终端种类标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let base = read_base(r)?;
            Ok(HirNode::Terminal { base, kind })
        }
        other => Err(format!(
            "节点种类标签非法 `{:?}`（偏移 {}）",
            other as char,
            r.pos - 1
        )),
    }
}

fn read_base(r: &mut ByteReader) -> Result<NodeBase, String> {
    let id = r.u32()?;
    let n_inputs = r.count("节点输入")?;
    let mut inputs = Vec::with_capacity(n_inputs);
    for _ in 0..n_inputs {
        inputs.push(r.u32()?);
    }
    let n_fields = r.count("schema 字段")?;
    let mut fields = Vec::with_capacity(n_fields);
    for _ in 0..n_fields {
        let id = r.u32()?;
        let name = r.str()?;
        let ty = read_type(r)?;
        let origin = match r.take()? {
            b's' => FieldOrigin::Source {
                source: r.u32()?,
                source_field: r.str()?,
            },
            b'v' => FieldOrigin::Derived {
                node: r.u32()?,
                binding: r.str()?,
            },
            other => {
                return Err(format!(
                    "字段来源标签非法 `{:?}`（偏移 {}）",
                    other as char,
                    r.pos - 1
                ));
            }
        };
        fields.push(FieldSchema {
            id,
            name,
            ty,
            origin,
        });
    }
    Ok(NodeBase {
        id,
        inputs,
        output: Schema { fields },
        span: PLACEHOLDER(),
    })
}

fn read_agg_call(r: &mut ByteReader) -> Result<AggCallH, String> {
    let func = match r.take()? {
        b'c' => AggFunc::Count,
        b's' => AggFunc::Sum,
        b'm' => AggFunc::Min,
        b'M' => AggFunc::Max,
        b'a' => AggFunc::Avg,
        other => {
            return Err(format!(
                "聚合函数标签非法 `{:?}`（偏移 {}）",
                other as char,
                r.pos - 1
            ));
        }
    };
    let arg = match r.take()? {
        b'1' => Some(read_expr(r)?),
        b'0' => None,
        other => {
            return Err(format!(
                "聚合参数标签非法 `{:?}`（偏移 {}）",
                other as char,
                r.pos - 1
            ));
        }
    };
    Ok(AggCallH { func, arg })
}

fn read_expr(r: &mut ByteReader) -> Result<THirExpr, String> {
    let tag = r.take()?;
    match tag {
        b'L' => {
            let value = match r.take()? {
                b'i' => Literal::Int(r.i64()?),
                b'f' => Literal::Float(r.f64()?),
                b'c' => Literal::Str(r.str()?),
                b'1' => Literal::Bool(true),
                b'0' => Literal::Bool(false),
                b'n' => Literal::Null,
                other => {
                    return Err(format!(
                        "字面量标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let ty = read_type(r)?;
            Ok(THirExpr::Literal {
                value,
                ty,
                span: PLACEHOLDER(),
            })
        }
        b'F' => {
            let field = r.u32()?;
            let ty = read_type(r)?;
            Ok(THirExpr::Field {
                field,
                ty,
                span: PLACEHOLDER(),
            })
        }
        b'B' => {
            let op = read_binop(r)?;
            let left = Box::new(read_expr(r)?);
            let right = Box::new(read_expr(r)?);
            let ty = read_type(r)?;
            Ok(THirExpr::Binary {
                op,
                left,
                right,
                ty,
                span: PLACEHOLDER(),
            })
        }
        b'U' => {
            let op = match r.take()? {
                b'-' => UnaryOp::Negate,
                b'!' => UnaryOp::Not,
                other => {
                    return Err(format!(
                        "一元运算标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let expr = Box::new(read_expr(r)?);
            let ty = read_type(r)?;
            Ok(THirExpr::Unary {
                op,
                expr,
                ty,
                span: PLACEHOLDER(),
            })
        }
        b'I' => {
            let condition = Box::new(read_expr(r)?);
            let then = Box::new(read_expr(r)?);
            let else_ = Box::new(read_expr(r)?);
            let ty = read_type(r)?;
            Ok(THirExpr::If {
                condition,
                then,
                else_,
                ty,
                span: PLACEHOLDER(),
            })
        }
        b'C' => {
            let name = match r.take()? {
                b'a' => BuiltinFn::Abs,
                b'r' => BuiltinFn::Round,
                b'l' => BuiltinFn::Lower,
                b'u' => BuiltinFn::Upper,
                b't' => BuiltinFn::Trim,
                b'g' => BuiltinFn::Length,
                b'k' => BuiltinFn::Contains,
                b'z' => BuiltinFn::IsNull,
                other => {
                    return Err(format!(
                        "内置函数标签非法 `{:?}`（偏移 {}）",
                        other as char,
                        r.pos - 1
                    ));
                }
            };
            let n = r.count("调用实参")?;
            let mut args = Vec::with_capacity(n);
            for _ in 0..n {
                args.push(read_expr(r)?);
            }
            let ty = read_type(r)?;
            Ok(THirExpr::Call {
                name,
                args,
                ty,
                span: PLACEHOLDER(),
            })
        }
        // v0.1.1 的 cast：V + 目标类型 + 操作数 + 结果类型
        b'V' => {
            let to = read_type(r)?;
            let expr = Box::new(read_expr(r)?);
            let ty = read_type(r)?;
            Ok(THirExpr::Cast {
                expr,
                to,
                ty,
                span: PLACEHOLDER(),
            })
        }
        other => Err(format!(
            "表达式标签非法 `{:?}`（偏移 {}）",
            other as char,
            r.pos - 1
        )),
    }
}

fn read_binop(r: &mut ByteReader) -> Result<BinOp, String> {
    match r.take()? {
        b'+' => Ok(BinOp::Add),
        b'-' => Ok(BinOp::Sub),
        b'*' => Ok(BinOp::Mul),
        b'/' => Ok(BinOp::Div),
        b'%' => Ok(BinOp::Mod),
        b'=' => Ok(BinOp::Eq),
        b'!' => Ok(BinOp::Neq),
        b'>' => Ok(BinOp::Gt),
        b'<' => Ok(BinOp::Lt),
        b'G' => Ok(BinOp::Gte),
        b'L' => Ok(BinOp::Lte),
        b'&' => Ok(BinOp::And),
        b'|' => Ok(BinOp::Or),
        other => Err(format!(
            "二元运算标签非法 `{:?}`（偏移 {}）",
            other as char,
            r.pos - 1
        )),
    }
}

fn read_type(r: &mut ByteReader) -> Result<Type, String> {
    match r.take()? {
        b'i' => Ok(Type::I64),
        b'f' => Ok(Type::F64),
        b'b' => Ok(Type::Bool),
        b'c' => Ok(Type::Str),
        b'?' => Ok(Type::Opt(Box::new(read_type(r)?))),
        other => Err(format!(
            "类型标签非法 `{:?}`（偏移 {}）",
            other as char,
            r.pos - 1
        )),
    }
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

    // ---------- ch12：反序列化 roundtrip ----------

    #[test]
    fn roundtrip_plain_pipeline() {
        let h = filter_over(THirExpr::Literal {
            value: Literal::Bool(true),
            ty: Type::Bool,
            span: Span::new(0, 0),
        });
        let bytes = canonical_bytes(&h);
        let back = canonical_hir(&bytes).expect("解析");
        assert_eq!(canonical_bytes(&back), bytes, "roundtrip 字节应逐位一致");
        assert_eq!(roundtrip_hash(&bytes).unwrap(), hir_hash(&h));
    }

    #[test]
    fn roundtrip_cast_pipeline() {
        let h = filter_over(cast_predicate());
        let bytes = canonical_bytes(&h);
        let back = canonical_hir(&bytes).expect("解析");
        assert_eq!(canonical_bytes(&back), bytes, "roundtrip 字节应逐位一致");
        assert_eq!(roundtrip_hash(&bytes).unwrap(), hir_hash(&h));
        // span 占位不影响哈希：原 HIR 与反序列化 HIR 的 canonical 哈希相同
        assert!(back.validate().is_ok());
    }

    #[test]
    fn roundtrip_rejects_corrupt_input() {
        let bytes = canonical_bytes(&filter_over(cast_predicate()));
        // 单字节翻转 → 要么解析失败，要么哈希必变（两者都必须被 verifier 抓住）
        let mut tampered = bytes.clone();
        let idx = tampered.len() / 2;
        tampered[idx] ^= 0x01;
        if let Ok(h) = canonical_hir(&tampered) {
            assert_ne!(hir_hash(&h), hir_hash(&canonical_hir(&bytes).unwrap()));
        }
        // 尾随垃圾 → 拒绝
        let mut trailing = bytes.clone();
        trailing.push(0xff);
        assert!(canonical_hir(&trailing).is_err());
        // 截断 → 拒绝
        assert!(canonical_hir(&bytes[..bytes.len() - 1]).is_err());
    }
}
