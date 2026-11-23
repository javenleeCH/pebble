//! pebble-duckdb：typed canonical HIR → DuckDB SQL BackendArtifact。
//!
//! 语义适配原则（不继承 SQL 默认行为）：
//! - `x == null` → `x IS NULL`（语义层已 lowering 为 is_null）；
//! - 整数算术 checked-null：HUGEINT 中转 + 范围守卫；除/模零 → NULL；
//! - f64 算术：结果非有限 → NULL；零规范化为 +0.0（`+ 0.0` 技巧）；
//! - 聚合：sum/min/max/avg 的可空性与确定性由显式 CASE 保证；
//! - sort 全键 `NULLS LAST`（asc/desc 只反转非空值顺序）；
//! - group/distinct 的值同一性由 DuckDB GROUP BY/DISTINCT 的 null 同组语义满足；
//! - `SET threads TO 1` 进入 canonical SQL，保证浮点聚合顺序确定性。
//!
//! 本后端不证明外部引擎物理计划等于 HIR；DuckDB 属执行信任基。

use pebble_hir::{
    BackendArtifact, BuiltinFn, BinOp, Hir, HirNode, Literal, SchemaLock, SourceKind, THirExpr,
    TransformKind, Type, UnaryOp,
};
use std::collections::BTreeSet;
use std::fmt::Write as _;

const I64_MIN: &str = "-9223372036854775808";
const I64_MAX: &str = "9223372036854775807";

/// 编译 HIR 为 DuckDB SQL 工件。lock_of 按 SourceKind 提供对应 Schema lock。
pub fn compile_sql(
    hir: &Hir,
    lock_of: &dyn Fn(&SourceKind) -> Option<SchemaLock>,
) -> Result<BackendArtifact, String> {
    let cg = Codegen { hir, lock_of };
    let terminal = &hir.nodes[hir.root as usize];
    let HirNode::Terminal { base, kind } = terminal else {
        return Err("root 必须是终端节点".into());
    };
    let input_id = base.inputs[0];
    let inner = cg.emit_node(input_id)?;
    let final_select = cg.final_select(&inner, base.inputs[0]);
    let statement = match kind {
        pebble_hir::TerminalKind::Write { path } => {
            format!(
                "COPY ({}) TO {} (HEADER, DELIMITER ',')",
                final_select,
                sql_str(path)
            )
        }
        _ => final_select,
    };
    let canonical_sql = format!(
        "-- pebble-duckdb artifact v1 (canonical)\nSET threads TO 1;\n{};\n",
        statement
    );
    Ok(BackendArtifact::DuckDbSql { canonical_sql })
}

/// RUN-001 预检：对每个源 schema 中的 f64 列生成有限值校验查询（subject, sql）。
pub fn f64_validation_queries(
    hir: &Hir,
    lock_of: &dyn Fn(&SourceKind) -> Option<SchemaLock>,
) -> Result<Vec<(String, String)>, String> {
    let cg = Codegen { hir, lock_of };
    let mut out = Vec::new();
    for node in &hir.nodes {
        if let HirNode::Source {
            base,
            source,
            kind,
        } = node
        {
            let scan = cg.emit_scan(base.id, *source, kind)?;
            for f in &base.output.fields {
                if matches!(f.ty.base(), Type::F64) {
                    let col = format!("\"f{}\"", f.id);
                    let sql = format!(
                        "SELECT CASE WHEN COUNT(*) = 0 THEN 'ok' ELSE 'bad' END FROM ({scan}) WHERE {col} IS NOT NULL AND NOT isfinite({col})",
                        scan = scan,
                        col = col
                    );
                    out.push((format!("f64 列 {}.{}", source_display(kind), f.name), sql));
                }
            }
        }
    }
    Ok(out)
}

fn source_display(kind: &SourceKind) -> String {
    match kind {
        SourceKind::Table { locator, .. } => locator.clone(),
        SourceKind::MemoryTable { name, .. } => format!("memory:{}", name),
    }
}

struct Codegen<'a> {
    hir: &'a Hir,
    lock_of: &'a dyn Fn(&SourceKind) -> Option<SchemaLock>,
}

impl<'a> Codegen<'a> {
    fn input_schema_ids(&self, node_id: pebble_hir::NodeId) -> BTreeSet<pebble_hir::FieldId> {
        let mut ids = BTreeSet::new();
        for &input in &self.hir.nodes[node_id as usize].base().inputs {
            for f in &self.hir.nodes[input as usize].base().output.fields {
                ids.insert(f.id);
            }
        }
        ids
    }

    fn emit_node(&self, id: pebble_hir::NodeId) -> Result<String, String> {
        match &self.hir.nodes[id as usize] {
            HirNode::Source {
                base,
                source,
                kind,
            } => self.emit_scan(base.id, *source, kind),
            HirNode::Transform { base, kind } => self.emit_transform(base, kind),
            HirNode::Aggregate {
                base,
                group_keys,
                bindings,
            } => self.emit_aggregate(base, group_keys, bindings),
            HirNode::Terminal { .. } => Err("终端节点不是可发射的中间阶段".into()),
        }
    }

    fn emit_scan(
        &self,
        node_id: pebble_hir::NodeId,
        source: pebble_hir::SourceId,
        kind: &SourceKind,
    ) -> Result<String, String> {
        let lock = (self.lock_of)(kind);
        let base = &self.hir.nodes[node_id as usize].base();
        let mut col_list = String::new();
        for (i, f) in base.output.fields.iter().enumerate() {
            if i > 0 {
                col_list.push_str(", ");
            }
            let _ = write!(col_list, "\"t{}\".\"{}\" AS \"f{}\"", source, f.name, f.id);
        }
        // 数据定位：Table → locator；MemoryTable → lock 的 data_file（宿主嵌入时无 → 引用注册表名）
        let (path, is_host_registered) = match kind {
            SourceKind::Table { locator, .. } => (locator.clone(), false),
            SourceKind::MemoryTable { name, .. } => {
                let data_file = lock.as_ref().and_then(|l| match &l.source {
                    pebble_hir::LockSource::MemoryTable { data_file, .. } => data_file.clone(),
                    _ => None,
                });
                match data_file {
                    Some(f) => (f, false),
                    None => (name.clone(), true),
                }
            }
        };
        if is_host_registered {
            return Ok(format!(
                "SELECT {} FROM {} AS \"t{}\"",
                col_list,
                sql_ident(&path),
                source
            ));
        }
        let is_parquet = path.to_ascii_lowercase().ends_with(".parquet");
        let from = if is_parquet {
            format!("read_parquet({})", sql_str(&path))
        } else {
            // CSV：显式 columns 规约，读取时按 lock 类型校验/转换（隐式 widening 被禁止）
            let mut spec = String::new();
            if let Some(lock) = &lock {
                for (i, f) in lock.fields.iter().enumerate() {
                    if i > 0 {
                        spec.push_str(", ");
                    }
                    let _ = write!(spec, "'{}': '{}'", f.name, lock_sql_type(f.ty));
                }
            }
            format!(
                "read_csv({}, header = true, columns = {{{}}})",
                sql_str(&path),
                spec
            )
        };
        Ok(format!("SELECT {} FROM {} AS \"t{}\"", col_list, from, source))
    }

    fn emit_transform(&self, base: &pebble_hir::NodeBase, kind: &TransformKind) -> Result<String, String> {
        let inner = self.emit_node(base.inputs[0])?;
        let passthrough = |ids: &BTreeSet<pebble_hir::FieldId>| -> String {
            base.output
                .fields
                .iter()
                .filter(|f| ids.contains(&f.id))
                .map(|f| format!("\"f{}\"", f.id))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match kind {
            TransformKind::Filter { pred } => {
                let pred_sql = self.emit_expr(pred);
                Ok(format!(
                    "SELECT {} FROM ({}) AS \"s{}\" WHERE ({})",
                    self.all_cols(base),
                    inner,
                    base.id,
                    pred_sql
                ))
            }
            TransformKind::Extend { bindings } => {
                let input_ids = self.input_schema_ids(base.inputs[0]);
                let mut select_list = Vec::new();
                for f in &base.output.fields {
                    if input_ids.contains(&f.id) {
                        select_list.push(format!("\"f{}\"", f.id));
                    } else if let Some(b) = bindings.iter().find(|b| b.output == f.id) {
                        select_list.push(format!(
                            "{} AS \"f{}\"",
                            self.emit_expr(&b.value),
                            f.id
                        ));
                    } else {
                        return Err(format!("extend 输出字段 f{} 无来源", f.id));
                    }
                }
                Ok(format!(
                    "SELECT {} FROM ({}) AS \"s{}\"",
                    select_list.join(", "),
                    inner,
                    base.id
                ))
            }
            TransformKind::Select { projections } => {
                let mut select_list = Vec::new();
                for p in projections {
                    match &p.expr {
                        THirExpr::Field { field, .. } if *field == p.output => {
                            select_list.push(format!("\"f{}\"", p.output));
                        }
                        _ => select_list.push(format!(
                            "{} AS \"f{}\"",
                            self.emit_expr(&p.expr),
                            p.output
                        )),
                    }
                }
                Ok(format!(
                    "SELECT {} FROM ({}) AS \"s{}\"",
                    select_list.join(", "),
                    inner,
                    base.id
                ))
            }
            TransformKind::Distinct => Ok(format!(
                "SELECT DISTINCT {} FROM ({}) AS \"s{}\"",
                self.all_cols(base),
                inner,
                base.id
            )),
            TransformKind::Sort { keys } => {
                let mut order = Vec::new();
                for k in keys {
                    order.push(format!(
                        "{} {} NULLS LAST",
                        self.emit_expr(&k.expr),
                        if k.descending { "DESC" } else { "ASC" }
                    ));
                }
                Ok(format!(
                    "SELECT {} FROM ({}) AS \"s{}\" ORDER BY {}",
                    self.all_cols(base),
                    inner,
                    base.id,
                    order.join(", ")
                ))
            }
            TransformKind::Take { n } => Ok(format!(
                "SELECT {} FROM ({}) AS \"s{}\" LIMIT {}",
                self.all_cols(base),
                inner,
                base.id,
                n
            )),
            TransformKind::Join { on } => {
                let right = self.emit_node(base.inputs[1])?;
                let left_ids = self.input_schema_ids(base.inputs[0]);
                let left_cols = passthrough(&left_ids);
                Ok(format!(
                    "SELECT {}, {} FROM ({}) AS \"l{}\" JOIN ({}) AS \"r{}\" ON ({})",
                    left_cols,
                    self.all_cols(base),
                    inner,
                    base.id,
                    right,
                    base.id,
                    self.emit_expr(on)
                ))
            }
        }
    }

    fn emit_aggregate(
        &self,
        base: &pebble_hir::NodeBase,
        group_keys: &[pebble_hir::GroupKeyH],
        bindings: &[pebble_hir::AggBindingH],
    ) -> Result<String, String> {
        let inner = self.emit_node(base.inputs[0])?;
        let mut select_list = Vec::new();
        let mut group_by = Vec::new();
        for k in group_keys {
            let key_sql = self.emit_expr(&k.value);
            select_list.push(format!("{} AS \"f{}\"", key_sql, k.output));
            group_by.push(key_sql);
        }
        for b in bindings {
            select_list.push(format!(
                "{} AS \"f{}\"",
                self.emit_agg_call(&b.call),
                b.output
            ));
        }
        if group_by.is_empty() {
            Ok(format!(
                "SELECT {} FROM ({}) AS \"s{}\"",
                select_list.join(", "),
                inner,
                base.id
            ))
        } else {
            Ok(format!(
                "SELECT {} FROM ({}) AS \"s{}\" GROUP BY {}",
                select_list.join(", "),
                inner,
                base.id,
                group_by.join(", ")
            ))
        }
    }

    fn emit_agg_call(&self, call: &pebble_hir::AggCallH) -> String {
        use pebble_hir::AggFunc::*;
        match call.func {
            Count => match &call.arg {
                None => "COUNT(*)".to_string(),
                Some(a) => format!("COUNT({})", self.emit_expr(a)),
            },
            Sum => {
                let x = self.emit_expr(call.arg.as_ref().expect("arity 已校验"));
                match call.arg.as_ref().unwrap().ty().base() {
                    Type::I64 => {
                        let s = format!("SUM(CAST(({}) AS HUGEINT))", x);
                        format!(
                            "CASE WHEN ({s}) IS NULL THEN NULL WHEN ({s}) BETWEEN {I64_MIN} AND {I64_MAX} THEN CAST(({s}) AS BIGINT) ELSE NULL END",
                            s = s
                        )
                    }
                    _ => {
                        let s = format!("SUM({})", x);
                        norm_f64(&s)
                    }
                }
            }
            Min | Max => {
                let x = self.emit_expr(call.arg.as_ref().expect("arity 已校验"));
                let m = if matches!(call.func, Min) {
                    format!("MIN({})", x)
                } else {
                    format!("MAX({})", x)
                };
                match call.arg.as_ref().unwrap().ty().base() {
                    Type::F64 => format!(
                        "CASE WHEN ({m}) = 0 THEN 0.0 ELSE ({m}) END",
                        m = m
                    ),
                    _ => m,
                }
            }
            Avg => {
                let a = call.arg.as_ref().expect("arity 已校验");
                let x = self.emit_expr(a);
                let raw = match a.ty().base() {
                    Type::I64 => format!("(SUM(CAST(({}) AS DOUBLE))) / (COUNT({}))", x, x),
                    _ => format!("(SUM({})) / (COUNT({}))", x, x),
                };
                norm_f64(&raw)
            }
        }
    }

    fn all_cols(&self, base: &pebble_hir::NodeBase) -> String {
        base.output
            .fields
            .iter()
            .map(|f| format!("\"f{}\"", f.id))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn final_select(&self, inner: &str, input_id: pebble_hir::NodeId) -> String {
        let schema = &self.hir.nodes[input_id as usize].base().output;
        let mut select_list = Vec::new();
        for f in &schema.fields {
            select_list.push(format!("\"f{}\" AS {}", f.id, sql_ident(&f.name)));
        }
        format!("SELECT {} FROM ({}) AS \"final\"", select_list.join(", "), inner)
    }

    // ---------- 表达式 ----------

    fn emit_expr(&self, e: &THirExpr) -> String {
        match e {
            THirExpr::Literal { value, ty, .. } => match value {
                Literal::Int(v) => v.to_string(),
                Literal::Float(v) => format!("CAST({:?} AS DOUBLE)", v),
                Literal::Str(s) => sql_str(s),
                Literal::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
                Literal::Null => format!("CAST(NULL AS {})", sql_type(ty.base())),
            },
            THirExpr::Field { field, .. } => format!("\"f{}\"", field),
            THirExpr::Binary {
                op,
                left,
                right,
                ty,
                ..
            } => self.emit_binary(*op, left, right, ty),
            THirExpr::Unary { op, expr, ty, .. } => self.emit_unary(*op, expr, ty),
            THirExpr::If {
                condition,
                then,
                else_,
                ..
            } => format!(
                "CASE WHEN ({}) THEN ({}) ELSE ({}) END",
                self.emit_expr(condition),
                self.emit_expr(then),
                self.emit_expr(else_)
            ),
            THirExpr::Call { name, args, .. } => self.emit_call(*name, args),
        }
    }

    fn emit_binary(&self, op: BinOp, left: &THirExpr, right: &THirExpr, _ty: &Type) -> String {
        use BinOp::*;
        match op {
            And => format!("(({}) AND ({}))", self.emit_expr(left), self.emit_expr(right)),
            Or => format!("(({}) OR ({}))", self.emit_expr(left), self.emit_expr(right)),
            Eq | Neq | Gt | Lt | Gte | Lte => {
                let o = match op {
                    Eq => "=",
                    Neq => "<>",
                    Gt => ">",
                    Lt => "<",
                    Gte => ">=",
                    Lte => "<=",
                    _ => unreachable!(),
                };
                format!("(({}) {} ({}))", self.emit_expr(left), o, self.emit_expr(right))
            }
            Add | Sub | Mul | Div | Mod => {
                let lb = left.ty().base();
                let rb = right.ty().base();
                if matches!(lb, Type::Str) && matches!(rb, Type::Str) {
                    // 字符串拼接（严格传播 null）
                    return format!("(({}) || ({}))", self.emit_expr(left), self.emit_expr(right));
                }
                if matches!(lb, Type::I64) && matches!(rb, Type::I64) {
                    self.emit_int_arith(op, &self.emit_expr(left), &self.emit_expr(right))
                } else {
                    let l = cast_f64(&self.emit_expr(left), lb);
                    let r = cast_f64(&self.emit_expr(right), rb);
                    self.emit_f64_arith(op, &l, &r)
                }
            }
        }
    }

    /// i64 checked 算术：HUGEINT 中转 + 范围守卫 + 除/模零 → NULL。
    fn emit_int_arith(&self, op: BinOp, l: &str, r: &str) -> String {
        use BinOp::*;
        let hl = format!("CAST(({}) AS HUGEINT)", l);
        let hr = format!("CAST(({}) AS HUGEINT)", r);
        match op {
            Add | Sub | Mul => {
                let o = match op {
                    Add => "+",
                    Sub => "-",
                    _ => "*",
                };
                let hexpr = format!("({}) {} ({})", hl, o, hr);
                format!(
                    "CASE WHEN ({l}) IS NULL THEN NULL WHEN ({r}) IS NULL THEN NULL WHEN {hexpr} BETWEEN {I64_MIN} AND {I64_MAX} THEN CAST({hexpr} AS BIGINT) ELSE NULL END",
                    l = l,
                    r = r,
                    hexpr = hexpr
                )
            }
            Div => {
                let hexpr = format!("({}) / ({})", hl, hr);
                format!(
                    "CASE WHEN ({l}) IS NULL THEN NULL WHEN ({r}) IS NULL THEN NULL WHEN ({r}) = 0 THEN NULL WHEN {hexpr} BETWEEN {I64_MIN} AND {I64_MAX} THEN CAST({hexpr} AS BIGINT) ELSE NULL END",
                    l = l,
                    r = r,
                    hexpr = hexpr
                )
            }
            Mod => format!(
                "CASE WHEN ({l}) IS NULL THEN NULL WHEN ({r}) IS NULL THEN NULL WHEN ({r}) = 0 THEN NULL ELSE MOD(({l}), ({r})) END",
                l = l,
                r = r
            ),
            _ => unreachable!(),
        }
    }

    /// f64 算术：除/模零与非有限结果 → NULL；零规范化 +0.0。
    fn emit_f64_arith(&self, op: BinOp, l: &str, r: &str) -> String {
        use BinOp::*;
        match op {
            Add | Sub | Mul => {
                let o = match op {
                    Add => "+",
                    Sub => "-",
                    _ => "*",
                };
                norm_f64(&format!("(({}) {} ({}))", l, o, r))
            }
            Div => norm_f64(&format!("(({}) / ({}))", l, r)),
            Mod => {
                // fmod：x - y*trunc(x/y)；除零 → NULL；非有限 → NULL；零规范化
                let raw = format!(
                    "(({}) - (({}) * trunc((({}) / ({})))))",
                    l, r, l, r
                );
                let normed = norm_f64(&raw);
                format!(
                    "CASE WHEN ({}) IS NULL THEN NULL WHEN ({}) = 0 THEN NULL ELSE {} END",
                    r, r, normed
                )
            }
            _ => unreachable!(),
        }
    }

    fn emit_unary(&self, op: UnaryOp, expr: &THirExpr, _ty: &Type) -> String {
        match op {
            UnaryOp::Not => {
                // NOT (x IS NULL) → x IS NOT NULL（判空否定形式的规范输出）
                if let THirExpr::Call {
                    name: BuiltinFn::IsNull,
                    args,
                    ..
                } = expr
                {
                    return format!("({}) IS NOT NULL", self.emit_expr(&args[0]));
                }
                format!("(NOT ({}))", self.emit_expr(expr))
            }
            UnaryOp::Negate => {
                let x = self.emit_expr(expr);
                match expr.ty().base() {
                    Type::I64 => format!(
                        "CASE WHEN ({x}) IS NULL THEN NULL WHEN ({x}) = {I64_MIN} THEN NULL ELSE -({x}) END",
                        x = x
                    ),
                    _ => format!("(({}) + 0.0)", format!("-({})", x)),
                }
            }
        }
    }

    fn emit_call(&self, name: BuiltinFn, args: &[THirExpr]) -> String {
        use BuiltinFn::*;
        match name {
            IsNull => format!("({}) IS NULL", self.emit_expr(&args[0])),
            Abs => {
                let x = self.emit_expr(&args[0]);
                match args[0].ty().base() {
                    Type::I64 => format!(
                        "CASE WHEN ({x}) IS NULL THEN NULL WHEN ({x}) = {I64_MIN} THEN NULL ELSE ABS(({x})) END",
                        x = x
                    ),
                    _ => format!("ABS(({}))", x),
                }
            }
            Round => {
                let x = self.emit_expr(&args[0]);
                // ties-to-even + 零规范化
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN (({x}) - FLOOR(({x}))) < 0.5 THEN (FLOOR(({x})) + 0.0) \
                     WHEN (({x}) - FLOOR(({x}))) > 0.5 THEN (FLOOR(({x})) + 1.0) \
                     WHEN MOD(CAST(FLOOR(({x})) AS BIGINT), 2) = 0 THEN (FLOOR(({x})) + 0.0) \
                     ELSE (FLOOR(({x})) + 1.0) END",
                    x = x
                )
            }
            Lower => format!("lower(({}))", self.emit_expr(&args[0])),
            Upper => format!("upper(({}))", self.emit_expr(&args[0])),
            Trim => format!("trim(({}))", self.emit_expr(&args[0])),
            Length => format!(
                "CAST(length(({})) AS BIGINT)",
                self.emit_expr(&args[0])
            ),
            Contains => format!(
                "contains(({}), ({}))",
                self.emit_expr(&args[0]),
                self.emit_expr(&args[1])
            ),
        }
    }
}

/// f64 规范化：非有限 → NULL；-0.0 → +0.0（v + 0.0）。
fn norm_f64(e: &str) -> String {
    format!(
        "CASE WHEN NOT isfinite(({})) THEN NULL ELSE (({}) + 0.0) END",
        e, e
    )
}

fn cast_f64(x: &str, base: &Type) -> String {
    if matches!(base, Type::I64) {
        format!("CAST(({}) AS DOUBLE)", x)
    } else {
        x.to_string()
    }
}

fn sql_type(ty: &Type) -> &'static str {
    match ty {
        Type::I64 => "BIGINT",
        Type::F64 => "DOUBLE",
        Type::Bool => "BOOLEAN",
        Type::Str => "VARCHAR",
        Type::Opt(_) => unreachable!("sql_type 接收基础类型"),
    }
}

fn lock_sql_type(ty: pebble_hir::LockType) -> &'static str {
    match ty {
        pebble_hir::LockType::I64 => "BIGINT",
        pebble_hir::LockType::F64 => "DOUBLE",
        pebble_hir::LockType::Bool => "BOOLEAN",
        pebble_hir::LockType::Str => "VARCHAR",
    }
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pebble_hir::{
        AggBindingH, AggCallH, AggFunc, FieldOrigin, FieldSchema, Hir, LockField, LockSource,
        LockType, SchemaLock, SEMANTICS_VERSION,
    };

    fn lock_for(kind: &SourceKind) -> Option<SchemaLock> {
        match kind {
            SourceKind::Table { locator, .. } => Some(SchemaLock::build(
                LockSource::Table {
                    locator: locator.clone(),
                },
                vec![
                    LockField { name: "status".into(), ty: LockType::Str, optional: false },
                    LockField { name: "amount".into(), ty: LockType::I64, optional: true },
                    LockField { name: "customer_id".into(), ty: LockType::Str, optional: false },
                ],
            )),
            SourceKind::MemoryTable { .. } => None,
        }
    }

    fn build_m0_hir() -> Hir {
        let src = r#"
Table("orders.csv") as o
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
        let mut diags = pebble_diagnostic::DiagBag::new();
        let program = pebble_parser::parse(src, &mut diags).expect("parse");
        let catalog = pebble_semantics::Catalog::from_locks(vec![lock_for(&SourceKind::Table {
            locator: "orders.csv".into(),
            schema_lock_hash: pebble_hir::Hash([0u8; 32]),
        })
        .expect("lock")]);
        pebble_semantics::compile(&program, &catalog, &mut diags).expect("lower")
    }

    #[test]
    fn emits_canonical_sql_with_p0_guards() {
        let hir = build_m0_hir();
        let artifact = compile_sql(&hir, &lock_for).expect("codegen");
        let BackendArtifact::DuckDbSql { canonical_sql } = artifact;
        // 前言：确定性引擎配置
        assert!(canonical_sql.starts_with("-- pebble-duckdb artifact v1 (canonical)\nSET threads TO 1;"));
        // 判空 → IS NULL / IS NOT NULL（经语义层 is_null lowering）
        assert!(canonical_sql.contains("IS NOT NULL"), "判空必须生成 IS NOT NULL");
        // sort nulls last
        assert!(canonical_sql.contains("NULLS LAST"));
        // sum 的 checked 守卫
        assert!(canonical_sql.contains("HUGEINT"));
        assert!(canonical_sql.contains("BETWEEN -9223372036854775808 AND 9223372036854775807"));
        // 有限值校验查询
        let queries = f64_validation_queries(&hir, &lock_for).expect("queries");
        // orders.csv lock 无 f64 列 → 无查询
        assert!(queries.is_empty());
    }

    #[test]
    fn int_arith_guards_div_and_overflow() {
        // 直接构造表达式级 SQL：1/0 与 i64 边界
        let hir = build_m0_hir();
        let cg = Codegen {
            hir: &hir,
            lock_of: &lock_for,
        };
        // 用 Binary 表达式单元测试 helper：构造简单 HIR 太重，直接检查字符串生成函数
        let sql = cg.emit_int_arith(
            pebble_hir::BinOp::Div,
            "CAST(1 AS BIGINT)",
            "CAST(0 AS BIGINT)",
        );
        assert!(sql.contains("= 0 THEN NULL"));
        let sql2 = cg.emit_int_arith(
            pebble_hir::BinOp::Add,
            "\"f1\"",
            "\"f2\"",
        );
        assert!(sql2.contains("HUGEINT"));
        assert!(sql2.contains("IS NULL THEN NULL"));
    }

    #[test]
    fn f64_norm_and_round_ties_to_even() {
        let hir = build_m0_hir();
        let cg = Codegen {
            hir: &hir,
            lock_of: &lock_for,
        };
        let n = norm_f64("((\"f1\") / (\"f2\"))");
        assert!(n.contains("isfinite"));
        assert!(n.contains("+ 0.0"));
        let r = cg.emit_call(
            pebble_hir::BuiltinFn::Round,
            &[pebble_hir::THirExpr::Field {
                field: 1,
                ty: Type::F64,
                span: pebble_diagnostic::Span::new(0, 0),
            }],
        );
        assert!(r.contains("MOD"));
        assert!(r.contains("0.5"));
    }

    #[test]
    fn agg_binding_h_smoke() {
        // 结构存在性冒烟：确保聚合 HIR 类型可用（防意外破坏 API）
        let _ = AggBindingH {
            output: 0,
            name: "x".into(),
            call: AggCallH {
                func: AggFunc::Sum,
                arg: None,
            },
        };
        let _ = FieldOrigin::Source {
            source: 0,
            source_field: "a".into(),
        };
    }
}
