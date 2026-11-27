//! pebble-hir：typed Logical HIR、Schema lock、BackendArtifact、Manifest/Receipt、
//! canonical 序列化与内容寻址哈希。
//!
//! HIR 是所有后端（DuckDB SQL / LaTeX / 未来原生）的唯一输入边界。
//! Span 仅用于诊断；canonical 序列化剔除 Span、注释与别名拼写。

pub mod canonical;
pub mod lock;
pub mod manifest;

pub use lock::{LockField, LockSource, LockType, SchemaLock};
pub use manifest::{
    BackendArtifact, BackendIdentity, CanonicalConfig, CanonicalTerminalOptions, DatasetIdentity,
    ExecutionManifest, ExecutionReceipt, ExecutionStatus, InputValidation, RunCode,
};

use pebble_diagnostic::Span;
use sha2::{Digest, Sha256};
use std::fmt;

/// 内容寻址哈希（SHA-256）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of_bytes(domain: &str, body: &[u8]) -> Hash {
        let mut h = Sha256::new();
        h.update(domain.as_bytes());
        h.update([0u8]);
        h.update(body);
        let mut out = [0u8; 32];
        out.copy_from_slice(&h.finalize());
        Hash(out)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// 标量类型；可空性只由 `Opt` 表达，无平行 nullable 标志。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Type {
    I64,
    F64,
    Bool,
    Str,
    Opt(Box<Type>),
}

impl Type {
    pub fn opt(self) -> Type {
        match self {
            Type::Opt(_) => self,
            other => Type::Opt(Box::new(other)),
        }
    }

    /// 去除一层可选标记。
    pub fn base(&self) -> &Type {
        match self {
            Type::Opt(inner) => inner,
            other => other,
        }
    }

    pub fn is_opt(&self) -> bool {
        matches!(self, Type::Opt(_))
    }

    pub fn render(&self) -> String {
        match self {
            Type::I64 => "i64".into(),
            Type::F64 => "f64".into(),
            Type::Bool => "bool".into(),
            Type::Str => "string".into(),
            Type::Opt(inner) => format!("?{}", inner.render()),
        }
    }
}

pub type SourceId = u32;
pub type FieldId = u32;
pub type NodeId = u32;

/// 字段来源：源字段（含来源身份）或某个节点的派生绑定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldOrigin {
    Source {
        source: SourceId,
        source_field: String,
    },
    Derived {
        node: NodeId,
        binding: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldSchema {
    /// 单次 canonical HIR 内稳定且唯一的语义身份。
    pub id: FieldId,
    /// 对外显示/绑定名。
    pub name: String,
    pub ty: Type,
    pub origin: FieldOrigin,
}

impl FieldSchema {
    pub fn origin_alias(&self) -> Option<&str> {
        match &self.origin {
            FieldOrigin::Source { source_field, .. } => Some(source_field),
            FieldOrigin::Derived { .. } => None,
        }
    }
}

/// 有序字段序列；顺序即用户可见输出列顺序。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Schema {
    pub fields: Vec<FieldSchema>,
}

impl Schema {
    pub fn field(&self, id: FieldId) -> Option<&FieldSchema> {
        self.fields.iter().find(|f| f.id == id)
    }

    pub fn by_name(&self, name: &str) -> Vec<&FieldSchema> {
        self.fields.iter().filter(|f| f.name == name).collect()
    }
}

// ---------- 表达式 ----------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    And,
    Or,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Negate,
    Not,
}

/// 内置标量函数白名单（04-expressions §4.8）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinFn {
    Abs,
    Round,
    Lower,
    Upper,
    Trim,
    Length,
    Contains,
    /// 总判空函数：x == null / x != null 的 lowering 目标。
    IsNull,
}

impl BuiltinFn {
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinFn::Abs => "abs",
            BuiltinFn::Round => "round",
            BuiltinFn::Lower => "lower",
            BuiltinFn::Upper => "upper",
            BuiltinFn::Trim => "trim",
            BuiltinFn::Length => "length",
            BuiltinFn::Contains => "contains",
            BuiltinFn::IsNull => "is_null",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// 带 `ty = Opt(T)` 的 null 字面量。
    Null,
}

/// 类型化表达式：字段引用只保存 FieldId；类型在 lowering 时固化。
#[derive(Clone, Debug)]
pub enum THirExpr {
    Literal {
        value: Literal,
        ty: Type,
        span: Span,
    },
    Field {
        field: FieldId,
        ty: Type,
        span: Span,
    },
    Binary {
        op: BinOp,
        left: Box<THirExpr>,
        right: Box<THirExpr>,
        ty: Type,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        expr: Box<THirExpr>,
        ty: Type,
        span: Span,
    },
    /// 单条件 If：条件仅 `true` 选择 then；`false/null` 选择 else。
    If {
        condition: Box<THirExpr>,
        then: Box<THirExpr>,
        else_: Box<THirExpr>,
        ty: Type,
        span: Span,
    },
    Call {
        name: BuiltinFn,
        args: Vec<THirExpr>,
        ty: Type,
        span: Span,
    },
    /// 受限显式转换 `cast(expr as T)`（v0.1.1，04 §4.12）。
    /// `to` 是目标**基础**类型（i64 / f64 / string）；结果的可空性在 `ty` 里：
    /// 可能失败的转换即使操作数非空，结果也是可空的（checked-null）。
    Cast {
        expr: Box<THirExpr>,
        to: Type,
        ty: Type,
        span: Span,
    },
}

impl THirExpr {
    pub fn ty(&self) -> &Type {
        match self {
            THirExpr::Literal { ty, .. }
            | THirExpr::Field { ty, .. }
            | THirExpr::Binary { ty, .. }
            | THirExpr::Unary { ty, .. }
            | THirExpr::If { ty, .. }
            | THirExpr::Call { ty, .. }
            | THirExpr::Cast { ty, .. } => ty,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            THirExpr::Literal { span, .. }
            | THirExpr::Field { span, .. }
            | THirExpr::Binary { span, .. }
            | THirExpr::Unary { span, .. }
            | THirExpr::If { span, .. }
            | THirExpr::Call { span, .. }
            | THirExpr::Cast { span, .. } => *span,
        }
    }

    pub fn is_const(&self) -> bool {
        match self {
            THirExpr::Literal { .. } => true,
            THirExpr::Field { .. } => false,
            THirExpr::Binary { left, right, .. } => left.is_const() && right.is_const(),
            THirExpr::Unary { expr, .. } => expr.is_const(),
            THirExpr::If {
                condition,
                then,
                else_,
                ..
            } => condition.is_const() && then.is_const() && else_.is_const(),
            THirExpr::Call { args, .. } => args.iter().all(|a| a.is_const()),
            THirExpr::Cast { expr, .. } => expr.is_const(),
        }
    }
}

// ---------- 节点 ----------

pub struct Binding<T> {
    pub output: FieldId,
    pub name: String,
    pub value: T,
}

pub struct Projection {
    pub expr: THirExpr,
    pub output: FieldId,
    pub name: String,
}

pub struct SortKeyH {
    pub expr: THirExpr,
    pub descending: bool,
}

pub struct GroupKeyH {
    pub value: THirExpr,
    pub output: FieldId,
    pub name: String,
}

pub struct AggBindingH {
    pub output: FieldId,
    pub name: String,
    pub call: AggCallH,
}

#[derive(Clone, Debug)]
pub struct AggCallH {
    pub func: AggFunc,
    pub arg: Option<THirExpr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

pub enum SourceKind {
    Table {
        locator: String,
        schema_lock_hash: Hash,
    },
    MemoryTable {
        name: String,
        schema_lock_hash: Hash,
    },
}

pub enum TerminalKind {
    Write { path: String },
    Collect,
    Print,
}

pub enum TransformKind {
    Filter {
        pred: THirExpr,
    },
    Extend {
        bindings: Vec<Binding<THirExpr>>,
    },
    Select {
        projections: Vec<Projection>,
    },
    Distinct,
    Sort {
        keys: Vec<SortKeyH>,
    },
    Take {
        n: i64,
    },
    /// inputs = [左, 右]；MVP 右输入是源节点。on=true 才保留行对。
    Join {
        on: THirExpr,
    },
}

pub enum HirNode {
    Source {
        base: NodeBase,
        source: SourceId,
        kind: SourceKind,
    },
    Transform {
        base: NodeBase,
        kind: TransformKind,
    },
    /// group_keys 为空 = 全局聚合。
    Aggregate {
        base: NodeBase,
        group_keys: Vec<GroupKeyH>,
        bindings: Vec<AggBindingH>,
    },
    Terminal {
        base: NodeBase,
        kind: TerminalKind,
    },
}

impl HirNode {
    pub fn base(&self) -> &NodeBase {
        match self {
            HirNode::Source { base, .. }
            | HirNode::Transform { base, .. }
            | HirNode::Aggregate { base, .. }
            | HirNode::Terminal { base, .. } => base,
        }
    }

    pub fn base_mut(&mut self) -> &mut NodeBase {
        match self {
            HirNode::Source { base, .. }
            | HirNode::Transform { base, .. }
            | HirNode::Aggregate { base, .. }
            | HirNode::Terminal { base, .. } => base,
        }
    }
}

pub struct NodeBase {
    pub id: NodeId,
    pub inputs: Vec<NodeId>,
    pub output: Schema,
    /// 仅用于诊断；canonical 序列化剔除。
    pub span: Span,
}

/// HIR 主体：节点列表 + 唯一终端根。
pub struct Hir {
    pub nodes: Vec<HirNode>,
    pub root: NodeId,
    pub semantics_version: String,
}

pub const SEMANTICS_VERSION: &str = "pebble-semantics-0.1";

impl Hir {
    /// 校验 HIR 不变量：终端可达、输入签名、字段身份创建唯一与引用闭合。
    /// 字段身份允许跨节点复用（passthrough）；只有「新创建」要求全局唯一。
    pub fn validate(&self) -> Result<(), String> {
        if self.root as usize >= self.nodes.len() {
            return Err("root 指向不存在的节点".into());
        }
        // 从终端反向可达
        let mut seen = vec![false; self.nodes.len()];
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if seen[id as usize] {
                continue;
            }
            seen[id as usize] = true;
            for &input in &self.nodes[id as usize].base().inputs {
                stack.push(input);
            }
        }
        for (i, s) in seen.iter().enumerate() {
            if !s {
                return Err(format!("节点 {} 从终端不可达", i));
            }
        }
        // 输入签名 + 字段身份
        let mut created: std::collections::BTreeSet<FieldId> = std::collections::BTreeSet::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let base = node.base();
            if base.id as usize != i {
                return Err(format!("节点 {} 的 id 与位置不一致", i));
            }
            // 输入 schema 的字段 id（引用合法域）
            let mut input_ids = std::collections::BTreeSet::new();
            for &input in &base.inputs {
                for f in &self.nodes[input as usize].base().output.fields {
                    input_ids.insert(f.id);
                }
            }
            match node {
                HirNode::Source { .. } => {
                    if !base.inputs.is_empty() {
                        return Err(format!("源节点 {} 不得有输入", i));
                    }
                    for f in &base.output.fields {
                        if !created.insert(f.id) {
                            return Err(format!("FieldId {} 被重复创建", f.id));
                        }
                    }
                }
                _ => {
                    if base.inputs.is_empty() {
                        return Err(format!("非源节点 {} 缺少输入", i));
                    }
                    for f in &base.output.fields {
                        if input_ids.contains(&f.id) {
                            continue; // passthrough 复用
                        }
                        if !created.insert(f.id) {
                            return Err(format!("FieldId {} 被重复创建", f.id));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 人类可读的调试转储（诊断用途；非 canonical）。
    pub fn dump(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "HIR (root={}, semantics={})\n",
            self.root, self.semantics_version
        ));
        for node in &self.nodes {
            let base = node.base();
            let kind = match node {
                HirNode::Source { source, kind, .. } => match kind {
                    SourceKind::Table { locator, .. } => {
                        format!("Source#{} Table({})", source, locator)
                    }
                    SourceKind::MemoryTable { name, .. } => {
                        format!("Source#{} MemoryTable({})", source, name)
                    }
                },
                HirNode::Transform { kind, .. } => match kind {
                    TransformKind::Filter { .. } => "Filter".to_string(),
                    TransformKind::Extend { .. } => "Extend".to_string(),
                    TransformKind::Select { .. } => "Select".to_string(),
                    TransformKind::Distinct => "Distinct".to_string(),
                    TransformKind::Sort { .. } => "Sort".to_string(),
                    TransformKind::Take { n } => format!("Take({})", n),
                    TransformKind::Join { .. } => "Join".to_string(),
                },
                HirNode::Aggregate { group_keys, .. } => {
                    if group_keys.is_empty() {
                        "Aggregate(global)".to_string()
                    } else {
                        format!("Aggregate(keys={})", group_keys.len())
                    }
                }
                HirNode::Terminal { kind, .. } => match kind {
                    TerminalKind::Write { path } => format!("Terminal::Write({})", path),
                    TerminalKind::Collect => "Terminal::Collect".to_string(),
                    TerminalKind::Print => "Terminal::Print".to_string(),
                },
            };
            out.push_str(&format!(
                "  node {} <- {:?} | {} | {}\n",
                base.id,
                base.inputs,
                kind,
                schema_repr(&base.output)
            ));
        }
        out
    }
}

fn schema_repr(schema: &Schema) -> String {
    schema
        .fields
        .iter()
        .map(|f| format!("f{}:{}:{}", f.id, f.name, f.ty.render()))
        .collect::<Vec<_>>()
        .join(", ")
}
