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
//! SQL 形态（artifact v2）：连续的行级算子并进同一层 SELECT，只在必须分层的地方开新的一层
//! （见 `Block`）。v1 给每个算子套一层子查询，DuckDB 解析到几百层就会失败。
//!
//! 本后端不证明外部引擎物理计划等于 HIR；DuckDB 属执行信任基。

use pebble_hir::{
    BackendArtifact, BinOp, BuiltinFn, Hir, HirNode, Literal, SchemaLock, SourceKind, THirExpr,
    TransformKind, Type, UnaryOp,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const I64_MIN: &str = "-9223372036854775808";
const I64_MAX: &str = "9223372036854775807";

/// 工件形态的版本标识，写进 canonical SQL 的第一行。
/// v1：每个算子一层子查询。v2：连续的行级算子融合进同一层 SELECT。
/// 它参与工件哈希，所以 v1 与 v2 的工件不可能得到相同的哈希——旧审批对不上新构建。
pub const ARTIFACT_HEADER: &str = "-- pebble-duckdb artifact v2 (canonical)";

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
    let sql = cg.emit_all()?;
    // 终端的输入是流水线上的最后一个节点；它开的（或并入的）那一层已在 emit_all 里封口
    let inner = sql[input_id as usize]
        .as_deref()
        .ok_or_else(|| format!("内部错误：节点 {input_id} 的 SQL 没有生成"))?;
    let final_select = cg.final_select(inner, base.inputs[0]);
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
    let canonical_sql = format!("{ARTIFACT_HEADER}\nSET threads TO 1;\n{};\n", statement);
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
        if let HirNode::Source { base, source, kind } = node {
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

impl Codegen<'_> {
    /// 节点 `node_id` 的输出字段 id——也就是它的消费者眼里的「输入 schema」。
    ///
    /// 这里曾经多套了一层（取的是「输入的输入的输出」）。后果是：`extend` / `join`
    /// 直接接在源后面时得到空集——extend 报「无来源」、join 生成 `SELECT , …`；
    /// 而它们前面隔着一个算子时又碰巧是对的，所以样例一直是绿的。
    fn output_ids(&self, node_id: pebble_hir::NodeId) -> BTreeSet<pebble_hir::FieldId> {
        self.hir.nodes[node_id as usize]
            .base()
            .output
            .fields
            .iter()
            .map(|f| f.id)
            .collect()
    }

    /// 自底向上生成每个节点的 SQL。
    ///
    /// 两件事让它不需要递归：HIR 的节点按拓扑序存放；每个节点的输入都排在它之前。
    ///
    /// 融合（v2）：连续的行级算子并进同一个 `Block`（一层 SELECT），只在必须分层的地方开新的一层。
    /// 当前链上「还没封口」的块是 `open`。并进来的节点自己不产生 SQL；直到下一个必须分层的节点
    /// 到来，才把 `open` 封口，作为**那个节点的输入**记下来。所以 `sql[i]` 为 `None` 只说明
    /// 节点 i 并进了别人开的层——它的输出在它的最后一个同层邻居那里。
    fn emit_all(&self) -> Result<Vec<Option<String>>, String> {
        let mut sql: Vec<Option<String>> = vec![None; self.hir.nodes.len()];
        let mut open: Option<Block> = None;
        for (i, node) in self.hir.nodes.iter().enumerate() {
            // 防御：若某个输入排在消费者之后（或不存在），下标取值就会错——明确报内部错误
            if let Some(bad) = node
                .base()
                .inputs
                .iter()
                .find(|&&input| input as usize >= i)
            {
                return Err(format!(
                    "内部错误：节点 {i} 的输入 {bad} 未排在它之前（HIR 不是拓扑序）"
                ));
            }
            match node {
                HirNode::Source { base, source, kind } => {
                    // 源是最里层：把表里的列重命名成 f<id>（第 5 章的身份直通后端）
                    sql[i] = Some(self.emit_scan(base.id, *source, kind)?);
                }
                HirNode::Terminal { base, .. } => {
                    // 终端不属于任何一层；它只负责把最后一个块封口
                    self.close(&mut open, &mut sql, base.inputs[0] as usize)?;
                }
                HirNode::Transform { base, kind } => {
                    self.lower_transform(i, base, kind, &mut sql, &mut open)?;
                }
                HirNode::Aggregate {
                    base,
                    group_keys,
                    bindings,
                } => {
                    let input = base.inputs[0] as usize;
                    let from = self.close(&mut open, &mut sql, input)?;
                    open = Some(self.aggregate_block(
                        i as pebble_hir::NodeId,
                        from,
                        self.output_ids(base.inputs[0]),
                        group_keys,
                        bindings,
                    ));
                }
            }
        }
        Ok(sql)
    }

    /// 取节点 `input` 的 SQL。当前链的块若还开着，先把它封口——
    /// `input` 一定是这个块的最后同层节点（流水线是线性的，节点按拓扑序处理）。
    fn close(
        &self,
        open: &mut Option<Block>,
        sql: &mut [Option<String>],
        input: usize,
    ) -> Result<String, String> {
        if let Some(block) = open.take() {
            let text = block.render();
            sql[input] = Some(text.clone());
            return Ok(text);
        }
        sql[input]
            .clone()
            .ok_or_else(|| format!("内部错误：节点 {input} 的 SQL 尚未生成"))
    }

    /// 一个行级算子：能并进当前层就并，不能就封口、开新的一层。
    ///
    /// 每条「能并」的规则都对应一句等价性论证；论证不了的（屏障之后的一律不并）就不并。
    fn lower_transform(
        &self,
        i: usize,
        base: &pebble_hir::NodeBase,
        kind: &TransformKind,
        sql: &mut [Option<String>],
        open: &mut Option<Block>,
    ) -> Result<(), String> {
        let me = i as pebble_hir::NodeId;
        let input = base.inputs[0] as usize;
        let input_ids = self.output_ids(base.inputs[0]);
        match kind {
            TransformKind::Filter { pred } => {
                // 聚合层是屏障的唯一例外：紧跟其后的 filter 是「对分组结果过滤」，并进它的 HAVING。
                // 论证：对外层关系（每个分组一行）做 WHERE，与对分组做 HAVING，保留的是同一批分组
                let mut into_having = false;
                if let Some(b) = open.as_mut() {
                    if b.group_by.is_some() {
                        let env = EmitEnv {
                            subst: b.subst.clone(),
                        };
                        b.having.push(self.emit_expr(pred, &env));
                        into_having = true;
                    }
                }
                if into_having {
                    return Ok(());
                }
                let pred_sql = self.emit_expr(pred, &NO_SUBST);
                // 能并进当前的 WHERE 吗？谓词引用的列必须都直接来自这一层的 FROM。
                // 引用了本层选择列表里新算出来的列就不行：标准 SQL 的 WHERE 看不到同一个 SELECT 里的别名。
                //（DuckDB 其实允许这种引用——那是它的方言扩展。不依赖它：生成的 SQL 只建立在标准语义上。）
                let mut refs = BTreeSet::new();
                field_refs(pred, &mut refs);
                let fusable = match open {
                    Some(b) if !b.barrier => refs.iter().all(|id| b.from_ids.contains(id)),
                    _ => false,
                };
                if !fusable {
                    let from = self.close(open, sql, input)?;
                    *open = Some(Block::passthrough(me, from, input_ids));
                }
                open.as_mut().expect("刚开的新层").predicates.push(pred_sql);
            }
            TransformKind::Extend { bindings } => {
                // (输出字段, 算出它的表达式)。表达式为 None 的是透传列
                let mut items = Vec::new();
                for f in &base.output.fields {
                    if input_ids.contains(&f.id) {
                        items.push((f.id, None));
                    } else if let Some(b) = bindings.iter().find(|b| b.output == f.id) {
                        items.push((f.id, Some(self.emit_expr(&b.value, &NO_SUBST))));
                    } else {
                        return Err(format!("extend 输出字段 f{} 无来源", f.id));
                    }
                }
                self.project(me, base, items, sql, open)?;
            }
            TransformKind::Select { projections } => {
                let mut items = Vec::new();
                for p in projections {
                    match &p.expr {
                        THirExpr::Field { field, .. } if *field == p.output => {
                            items.push((p.output, None));
                        }
                        _ => items.push((p.output, Some(self.emit_expr(&p.expr, &NO_SUBST)))),
                    }
                }
                self.project(me, base, items, sql, open)?;
            }
            TransformKind::Distinct => {
                let from = self.close(open, sql, input)?;
                let mut block = Block::passthrough(me, from, input_ids);
                block.distinct = true;
                block.barrier = true;
                *open = Some(block);
            }
            TransformKind::Sort { keys } => {
                let mut order = Vec::new();
                for k in keys {
                    order.push(format!(
                        "{} {} NULLS LAST",
                        self.emit_expr(&k.expr, &NO_SUBST),
                        if k.descending { "DESC" } else { "ASC" }
                    ));
                }
                let fusable = matches!(open, Some(b) if !b.barrier && b.order_by.is_empty() && b.limit.is_none());
                if !fusable {
                    let from = self.close(open, sql, input)?;
                    *open = Some(Block::passthrough(me, from, input_ids));
                }
                open.as_mut().expect("刚开的新层").order_by = order;
            }
            TransformKind::Take { n } => {
                // 屏障：之后的 filter 并进 WHERE 就跑到了 LIMIT 前面
                let from = self.close(open, sql, input)?;
                let mut block = Block::passthrough(me, from, input_ids);
                block.limit = Some(*n);
                block.barrier = true;
                *open = Some(block);
            }
            TransformKind::Join { on } => {
                // 右表的 SQL 早已就绪（它是源节点，排在前面）；左表就是当前链的最后一个块
                let right = sql[base.inputs[1] as usize]
                    .clone()
                    .ok_or("内部错误：join 右表的 SQL 尚未生成")?;
                let from = self.close(open, sql, input)?;
                let mut from_ids = input_ids.clone();
                from_ids.extend(self.output_ids(base.inputs[1]));
                // join 开新的一层，但不是屏障：后面的 filter / extend / select 可以并进来
                *open = Some(Block::passthrough(me, from, from_ids).with_join(
                    right,
                    self.emit_expr(on, &NO_SUBST),
                    input_ids,
                ));
            }
        }
        Ok(())
    }

    /// extend / select：给当前层换上新的选择列表。当前层已经投影过、或是屏障时，开新的一层。
    ///
    /// 透传列的写法交给块自己决定（`column_ref`）：join 块里带表别名，其余直接写 `"fN"`。
    fn project(
        &self,
        me: pebble_hir::NodeId,
        base: &pebble_hir::NodeBase,
        items: Vec<(pebble_hir::FieldId, Option<String>)>,
        sql: &mut [Option<String>],
        open: &mut Option<Block>,
    ) -> Result<(), String> {
        let input = base.inputs[0] as usize;
        let mut block = match open.take() {
            Some(b) if !b.barrier && !b.projected => b,
            other => {
                // 放回去让 close() 封口（other 是 Some(不可并的块) 或 None）
                *open = other;
                let from = self.close(open, sql, input)?;
                let mut fresh = Block::passthrough(me, from, self.output_ids(base.inputs[0]));
                fresh.projected = true;
                fresh
            }
        };
        block.select = items
            .iter()
            .map(|(id, expr)| match expr {
                None => block.column_ref(*id),
                Some(e) => format!("{e} AS \"f{id}\""),
            })
            .collect();
        block.projected = true;
        *open = Some(block);
        Ok(())
    }

    /// 聚合层：`SELECT <键> AS fN, <聚合> AS fN FROM (内层) [GROUP BY <键>] [HAVING …]`。
    ///
    /// 聚合层是屏障：它后面的 filter 不能并进 WHERE——那就跑到了分组前面。
    /// 例外是紧跟其后的 filter（即 having），它并进 HAVING（见 `lower_transform`）。
    /// `subst` 记下每个输出字段的表达式，发射 HAVING 时用它替换字段引用。
    fn aggregate_block(
        &self,
        me: pebble_hir::NodeId,
        from: String,
        from_ids: BTreeSet<pebble_hir::FieldId>,
        group_keys: &[pebble_hir::GroupKeyH],
        bindings: &[pebble_hir::AggBindingH],
    ) -> Block {
        let mut select = Vec::new();
        let mut group_by = Vec::new();
        let mut subst = BTreeMap::new();
        for k in group_keys {
            let key_sql = self.emit_expr(&k.value, &NO_SUBST);
            select.push(format!("{} AS \"f{}\"", key_sql, k.output));
            group_by.push(key_sql.clone());
            subst.insert(k.output, key_sql);
        }
        for b in bindings {
            let call_sql = self.emit_agg_call(&b.call, &NO_SUBST);
            select.push(format!("{} AS \"f{}\"", call_sql, b.output));
            subst.insert(b.output, call_sql);
        }
        Block {
            select,
            projected: true,
            group_by: Some(group_by),
            subst,
            barrier: true,
            ..Block::passthrough(me, from, from_ids)
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
        Ok(format!(
            "SELECT {} FROM {} AS \"t{}\"",
            col_list, from, source
        ))
    }

    fn emit_agg_call(&self, call: &pebble_hir::AggCallH, env: &EmitEnv) -> String {
        use pebble_hir::AggFunc::*;
        match call.func {
            Count => match &call.arg {
                None => "COUNT(*)".to_string(),
                Some(a) => format!("COUNT({})", self.emit_expr(a, env)),
            },
            Sum => {
                let x = self.emit_expr(call.arg.as_ref().expect("arity 已校验"), env);
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
                let x = self.emit_expr(call.arg.as_ref().expect("arity 已校验"), env);
                let m = if matches!(call.func, Min) {
                    format!("MIN({})", x)
                } else {
                    format!("MAX({})", x)
                };
                match call.arg.as_ref().unwrap().ty().base() {
                    Type::F64 => format!("CASE WHEN ({m}) = 0 THEN 0.0 ELSE ({m}) END", m = m),
                    _ => m,
                }
            }
            Avg => {
                let a = call.arg.as_ref().expect("arity 已校验");
                let x = self.emit_expr(a, env);
                let raw = match a.ty().base() {
                    Type::I64 => format!("(SUM(CAST(({}) AS DOUBLE))) / (COUNT({}))", x, x),
                    _ => format!("(SUM({})) / (COUNT({}))", x, x),
                };
                norm_f64(&raw)
            }
        }
    }

    fn final_select(&self, inner: &str, input_id: pebble_hir::NodeId) -> String {
        let schema = &self.hir.nodes[input_id as usize].base().output;
        let mut select_list = Vec::new();
        for f in &schema.fields {
            select_list.push(format!("\"f{}\" AS {}", f.id, sql_ident(&f.name)));
        }
        format!(
            "SELECT {} FROM ({}) AS \"final\"",
            select_list.join(", "),
            inner
        )
    }

    // ---------- 表达式 ----------

    fn emit_expr(&self, e: &THirExpr, env: &EmitEnv) -> String {
        match e {
            THirExpr::Literal { value, ty, .. } => match value {
                Literal::Int(v) => v.to_string(),
                Literal::Float(v) => format!("CAST({:?} AS DOUBLE)", v),
                Literal::Str(s) => sql_str(s),
                Literal::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
                Literal::Null => format!("CAST(NULL AS {})", sql_type(ty.base())),
            },
            THirExpr::Field { field, .. } => match env.subst.get(field) {
                // HAVING 里引用聚合的输出：换成它的表达式。标准 SQL 的 HAVING 看不到选择列表的别名
                Some(sql) => sql.clone(),
                None => format!("\"f{}\"", field),
            },
            THirExpr::Cast { expr, to, .. } => self.emit_cast(expr, to, env),
            THirExpr::Binary {
                op,
                left,
                right,
                ty,
                ..
            } => self.emit_binary(*op, left, right, ty, env),
            THirExpr::Unary { op, expr, ty, .. } => self.emit_unary(*op, expr, ty, env),
            THirExpr::If {
                condition,
                then,
                else_,
                ..
            } => format!(
                "CASE WHEN ({}) THEN ({}) ELSE ({}) END",
                self.emit_expr(condition, env),
                self.emit_expr(then, env),
                self.emit_expr(else_, env)
            ),
            THirExpr::Call { name, args, .. } => self.emit_call(*name, args, env),
        }
    }

    fn emit_binary(
        &self,
        op: BinOp,
        left: &THirExpr,
        right: &THirExpr,
        _ty: &Type,
        env: &EmitEnv,
    ) -> String {
        use BinOp::*;
        match op {
            And => format!(
                "(({}) AND ({}))",
                self.emit_expr(left, env),
                self.emit_expr(right, env)
            ),
            Or => format!(
                "(({}) OR ({}))",
                self.emit_expr(left, env),
                self.emit_expr(right, env)
            ),
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
                format!(
                    "(({}) {} ({}))",
                    self.emit_expr(left, env),
                    o,
                    self.emit_expr(right, env)
                )
            }
            Add | Sub | Mul | Div | Mod => {
                let lb = left.ty().base();
                let rb = right.ty().base();
                if matches!(lb, Type::Str) && matches!(rb, Type::Str) {
                    // 字符串拼接（严格传播 null）
                    return format!(
                        "(({}) || ({}))",
                        self.emit_expr(left, env),
                        self.emit_expr(right, env)
                    );
                }
                if matches!(lb, Type::I64) && matches!(rb, Type::I64) {
                    self.emit_int_arith(op, &self.emit_expr(left, env), &self.emit_expr(right, env))
                } else {
                    let l = cast_f64(&self.emit_expr(left, env), lb);
                    let r = cast_f64(&self.emit_expr(right, env), rb);
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
                let raw = format!("(({}) - (({}) * trunc((({}) / ({})))))", l, r, l, r);
                let normed = norm_f64(&raw);
                format!(
                    "CASE WHEN ({}) IS NULL THEN NULL WHEN ({}) = 0 THEN NULL ELSE {} END",
                    r, r, normed
                )
            }
            _ => unreachable!(),
        }
    }

    fn emit_unary(&self, op: UnaryOp, expr: &THirExpr, _ty: &Type, env: &EmitEnv) -> String {
        match op {
            UnaryOp::Not => {
                // NOT (x IS NULL) → x IS NOT NULL（判空否定形式的规范输出）
                if let THirExpr::Call {
                    name: BuiltinFn::IsNull,
                    args,
                    ..
                } = expr
                {
                    return format!("({}) IS NOT NULL", self.emit_expr(&args[0], env));
                }
                format!("(NOT ({}))", self.emit_expr(expr, env))
            }
            UnaryOp::Negate => {
                let x = self.emit_expr(expr, env);
                match expr.ty().base() {
                    Type::I64 => format!(
                        "CASE WHEN ({x}) IS NULL THEN NULL WHEN ({x}) = {I64_MIN} THEN NULL ELSE -({x}) END",
                        x = x
                    ),
                    _ => format!("((-({x})) + 0.0)", x = x),
                }
            }
        }
    }

    fn emit_call(&self, name: BuiltinFn, args: &[THirExpr], env: &EmitEnv) -> String {
        use BuiltinFn::*;
        match name {
            IsNull => format!("({}) IS NULL", self.emit_expr(&args[0], env)),
            Abs => {
                let x = self.emit_expr(&args[0], env);
                match args[0].ty().base() {
                    Type::I64 => format!(
                        "CASE WHEN ({x}) IS NULL THEN NULL WHEN ({x}) = {I64_MIN} THEN NULL ELSE ABS(({x})) END",
                        x = x
                    ),
                    _ => format!("ABS(({}))", x),
                }
            }
            Round => {
                let x = self.emit_expr(&args[0], env);
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
            Lower => format!("lower(({}))", self.emit_expr(&args[0], env)),
            Upper => format!("upper(({}))", self.emit_expr(&args[0], env)),
            // 不能直接用 DuckDB 的 `trim(x)`：实测它不去掉制表符（第 11 章）。把规范要求的
            // Unicode White_Space 字符集显式交给 `trim(x, chars)`
            Trim => format!(
                "trim(({}), {})",
                self.emit_expr(&args[0], env),
                whitespace_set_sql()
            ),
            Length => format!(
                "CAST(length(({})) AS BIGINT)",
                self.emit_expr(&args[0], env)
            ),
            Contains => format!(
                "contains(({}), ({}))",
                self.emit_expr(&args[0], env),
                self.emit_expr(&args[1], env)
            ),
        }
    }

    /// 受限 cast（07 §7.8）。每条规则都是「先把引擎的行为收敛到规范，再取值」：
    /// 引擎接受得更宽的写法（空白、下划线、`inf`）或别的取整习惯，都被守卫拦在外面。
    /// 取值的参考实现是语义层的 `f64_to_i64` / `parse_i64_strict` / `parse_f64_strict`，
    /// 测试拿它们和这里生成的 SQL 在真实 DuckDB 上对拍。
    fn emit_cast(&self, expr: &THirExpr, to: &Type, env: &EmitEnv) -> String {
        let x = self.emit_expr(expr, env);
        match (expr.ty().base(), to) {
            // 永不失败；null 经 CAST 仍是 null
            (Type::I64, Type::F64) => format!("CAST(({x}) AS DOUBLE)"),
            (Type::F64, Type::I64) => {
                // 先查值域，再取整。值域内的 x 取整后必然可表示：取整最多把 x 推高 0.5，
                // 而 2^63 附近能表示的 f64 都是整数、彼此至少相差 1024。
                // 取整公式里的 CAST(FLOOR(x) AS BIGINT) 只在小数部分恰为 0.5 时求值，那时 |x| < 2^53
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (isfinite(({x}))) THEN NULL \
                     WHEN ({x}) >= 9223372036854775808.0 OR ({x}) < -9223372036854775808.0 THEN NULL \
                     ELSE CAST({} AS BIGINT) END",
                    rne_integer(&x)
                )
            }
            (Type::Str, Type::I64) => {
                // 正则先把写法限死；越界的数字串 TRY_CAST 成 HUGEINT 得 NULL。
                // 最后一步也用 TRY_CAST：万一前两道都没拦住，结果是 NULL 而不是引擎报错
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (regexp_matches(({x}), '^[+-]?[0-9]+$')) THEN NULL \
                     WHEN NOT (TRY_CAST(({x}) AS HUGEINT) BETWEEN {I64_MIN} AND {I64_MAX}) THEN NULL \
                     ELSE TRY_CAST(({x}) AS BIGINT) END"
                )
            }
            (Type::Str, Type::F64) => {
                // 规范文法：[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?
                let grammar = r"^[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?$";
                let parsed = format!("TRY_CAST(({x}) AS DOUBLE)");
                format!(
                    "CASE WHEN ({x}) IS NULL THEN NULL \
                     WHEN NOT (regexp_matches(({x}), '{grammar}')) THEN NULL \
                     ELSE {} END",
                    // 非有限（如 "1e400"）→ NULL；-0.0 → +0.0
                    norm_f64(&parsed)
                )
            }
            // 语义层只放行四种转换；走到这里说明编译器自己出了错
            _ => "NULL /* 内部错误：不支持的 cast */".to_string(),
        }
    }
}

/// 发射表达式时的环境。
///
/// 绝大多数场合它是空的。唯一的例外是 `HAVING`：标准 SQL 的 HAVING 不能引用同一层选择列表的
/// 别名（比如 `COUNT(*) AS "f6"` 里的 `"f6"`），所以要把聚合输出的字段引用换成它的表达式。
/// `subst` 就是这张「输出字段 id → 表达式 SQL」的替换表。
struct EmitEnv {
    subst: BTreeMap<pebble_hir::FieldId, String>,
}

/// 没有替换的环境：WHERE、选择列表、ORDER BY 等场合都用它。
const NO_SUBST: EmitEnv = EmitEnv {
    subst: BTreeMap::new(),
};
/// 一层 SELECT——一个「块」。
///
/// v1 的 codegen 给每个算子都套一层子查询，n 个算子就是 n 层嵌套，DuckDB 解析到几百层
/// 就吃不消（第 10 章）。v2 把能合并的算子并进同一层 SELECT：嵌套深度只取决于
/// 「必须分层的地方」的个数，而不是算子的个数。
///
/// 一个块对应一条 SELECT 的各个子句：
/// `SELECT [DISTINCT] <select> FROM (<from>) [JOIN … ON …] [WHERE …] [GROUP BY …] [ORDER BY …] [LIMIT …]`
#[derive(Debug)]
struct Block {
    /// 开启这个块的节点编号，用作表别名 `s<n>`（join 块：`l<n>` / `r<n>`）
    id: pebble_hir::NodeId,
    /// FROM 里的内层 SQL
    from: String,
    /// FROM 直接提供的列：内层节点的输出字段 id（join 块：左、右两侧的并集）
    from_ids: BTreeSet<pebble_hir::FieldId>,
    /// join 的右侧 SQL 与连接条件
    join: Option<(String, String)>,
    /// join 块里由左表带来的列：生成列引用时要带表别名
    left_ids: Option<BTreeSet<pebble_hir::FieldId>>,
    /// 选择列表：列引用（透传）或 `<表达式> AS "fN"`
    select: Vec<String>,
    /// 已有 extend / select 并了进来：选择列表不再是纯透传，再来的投影必须开新的一层
    projected: bool,
    distinct: bool,
    /// WHERE 里的谓词，按流水线顺序。发射时合成一棵平衡的 AND 树（见 `and_all`）
    predicates: Vec<String>,
    group_by: Option<Vec<String>>,
    /// HAVING 里的谓词：聚合层之后紧跟的 filter（即 having）并在这里（第 11 章）
    having: Vec<String>,
    /// 聚合层专用：输出字段 id → 它的表达式 SQL。发射 HAVING 时用它替换字段引用
    subst: BTreeMap<pebble_hir::FieldId, String>,
    order_by: Vec<String>,
    limit: Option<i64>,
    /// 屏障：之后的算子不得再并进来（distinct / take / 聚合）。
    /// 它们后面的 filter 若并进 WHERE，就等于跑到了它们前面——结果会变。
    /// 唯一的例外是聚合层：紧跟其后的 filter 是「对分组结果过滤」，并进它的 HAVING
    barrier: bool,
}

impl Block {
    /// 一个纯透传的新块：输出 = 输入的全部列。
    fn passthrough(
        id: pebble_hir::NodeId,
        from: String,
        from_ids: BTreeSet<pebble_hir::FieldId>,
    ) -> Self {
        let mut block = Block {
            id,
            from,
            from_ids,
            join: None,
            left_ids: None,
            select: Vec::new(),
            projected: false,
            distinct: false,
            predicates: Vec::new(),
            group_by: None,
            having: Vec::new(),
            subst: BTreeMap::new(),
            order_by: Vec::new(),
            limit: None,
            barrier: false,
        };
        block.select = block
            .from_ids
            .iter()
            .map(|f| block.column_ref(*f))
            .collect();
        block
    }

    /// 一个列的引用。join 块里用表别名限定——不依赖「两边的列名恰好不重复」这种巧合；
    /// 其余的块直接写 `"fN"`。
    fn column_ref(&self, id: pebble_hir::FieldId) -> String {
        match &self.left_ids {
            Some(left) if left.contains(&id) => format!("\"l{}\".\"f{}\"", self.id, id),
            Some(_) => format!("\"r{}\".\"f{}\"", self.id, id),
            None => format!("\"f{}\"", id),
        }
    }

    /// 把这个块变成 join 块：两侧的列都由本层直接提供，透传列从此带表别名。
    fn with_join(
        mut self,
        right: String,
        on: String,
        left_ids: BTreeSet<pebble_hir::FieldId>,
    ) -> Self {
        self.join = Some((right, on));
        self.left_ids = Some(left_ids);
        self.select = self.from_ids.iter().map(|f| self.column_ref(*f)).collect();
        self
    }

    fn render(&self) -> String {
        let mut sql = String::from("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&self.select.join(", "));
        match &self.join {
            None => {
                let _ = write!(sql, " FROM ({}) AS \"s{}\"", self.from, self.id);
            }
            Some((right, on)) => {
                let _ = write!(
                    sql,
                    " FROM ({}) AS \"l{}\" JOIN ({}) AS \"r{}\" ON ({})",
                    self.from, self.id, right, self.id, on
                );
            }
        }
        if !self.predicates.is_empty() {
            let _ = write!(sql, " WHERE ({})", and_all(&self.predicates));
        }
        if let Some(keys) = &self.group_by {
            if !keys.is_empty() {
                let _ = write!(sql, " GROUP BY {}", keys.join(", "));
            }
        }
        if !self.having.is_empty() {
            let _ = write!(sql, " HAVING ({})", and_all(&self.having));
        }
        if !self.order_by.is_empty() {
            let _ = write!(sql, " ORDER BY {}", self.order_by.join(", "));
        }
        if let Some(n) = self.limit {
            let _ = write!(sql, " LIMIT {n}");
        }
        sql
    }
}

/// 把一串谓词合成一棵平衡的 AND 树。
///
/// 不能从头到尾左结合地串下去：`(((p1 AND p2) AND p3) AND …)` 的括号深度等于谓词个数，
/// 两万个 filter 就是两万层，DuckDB 的解析器在 1000 层就会拒绝（Max expression depth）。
/// 平衡树的深度只有 log₂(n)：两万个谓词只有 15 层。
fn and_all(preds: &[String]) -> String {
    match preds.len() {
        1 => preds[0].clone(),
        n => {
            let (left, right) = preds.split_at(n.div_ceil(2));
            format!("({} AND {})", and_all(left), and_all(right))
        }
    }
}

/// 表达式引用到的全部字段 id。
/// 这里的递归是安全的：表达式嵌套深度有上限（第 8 章，64 层）。
fn field_refs(e: &THirExpr, out: &mut BTreeSet<pebble_hir::FieldId>) {
    match e {
        THirExpr::Field { field, .. } => {
            out.insert(*field);
        }
        THirExpr::Binary { left, right, .. } => {
            field_refs(left, out);
            field_refs(right, out);
        }
        THirExpr::Unary { expr, .. } => field_refs(expr, out),
        THirExpr::If {
            condition,
            then,
            else_,
            ..
        } => {
            field_refs(condition, out);
            field_refs(then, out);
            field_refs(else_, out);
        }
        THirExpr::Call { args, .. } => {
            for a in args {
                field_refs(a, out);
            }
        }
        THirExpr::Cast { expr, .. } => field_refs(expr, out),
        THirExpr::Literal { .. } => {}
    }
}

/// 「按 round-to-nearest, ties-to-even 取整」的 DOUBLE 表达式（结果已是整数值）。
/// 小数部分写成 `(x) - FLOOR((x))`：它始终落在 [0, 1)，对负数也成立（见第 6 章）。
fn rne_integer(x: &str) -> String {
    format!(
        "CASE WHEN (({x}) - FLOOR(({x}))) < 0.5 THEN FLOOR(({x})) \
         WHEN (({x}) - FLOOR(({x}))) > 0.5 THEN (FLOOR(({x})) + 1.0) \
         WHEN MOD(CAST(FLOOR(({x})) AS BIGINT), 2) = 0 THEN FLOOR(({x})) \
         ELSE (FLOOR(({x})) + 1.0) END"
    )
}
/// Unicode White_Space 的全部 25 个码点（04 §4.8：trim 去除两端 Unicode White_Space）。
/// 写成 `chr(n) || chr(n) …`：纯 ASCII、不含控制字符，canonical SQL 保持单行。
fn whitespace_set_sql() -> String {
    const WHITE_SPACE: [u32; 25] = [
        0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x0020, 0x0085, 0x00A0, 0x1680, 0x2000, 0x2001,
        0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028, 0x2029,
        0x202F, 0x205F, 0x3000,
    ];
    WHITE_SPACE
        .iter()
        .map(|c| format!("chr({c})"))
        .collect::<Vec<_>>()
        .join(" || ")
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
        AggBindingH, AggCallH, AggFunc, FieldOrigin, Hir, LockField, LockSource, LockType,
        SchemaLock,
    };

    fn lock_for(kind: &SourceKind) -> Option<SchemaLock> {
        match kind {
            SourceKind::Table { locator, .. } => Some(SchemaLock::build(
                LockSource::Table {
                    locator: locator.clone(),
                },
                vec![
                    LockField {
                        name: "status".into(),
                        ty: LockType::Str,
                        optional: false,
                    },
                    LockField {
                        name: "amount".into(),
                        ty: LockType::I64,
                        optional: true,
                    },
                    LockField {
                        name: "customer_id".into(),
                        ty: LockType::Str,
                        optional: false,
                    },
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
        let catalog = pebble_semantics::Catalog::from_locks(vec![
            lock_for(&SourceKind::Table {
                locator: "orders.csv".into(),
                schema_lock_hash: pebble_hir::Hash([0u8; 32]),
            })
            .expect("lock"),
        ]);
        pebble_semantics::compile(&program, &catalog, &mut diags).expect("lower")
    }

    /// 把一段 Pebble 源码 lowering 成 HIR（表 `orders.csv` 的 lock 同 `lock_for`）。
    fn lower(src: &str) -> Hir {
        let mut diags = pebble_diagnostic::DiagBag::new();
        let program = pebble_parser::parse(src, &mut diags).expect("parse");
        let lock = lock_for(&SourceKind::Table {
            locator: "orders.csv".into(),
            schema_lock_hash: pebble_hir::Hash([0u8; 32]),
        })
        .expect("lock");
        let catalog = pebble_semantics::Catalog::from_locks(vec![lock]);
        pebble_semantics::compile(&program, &catalog, &mut diags).expect("lower")
    }

    fn sql_of(src: &str) -> String {
        let hir = lower(src);
        let BackendArtifact::DuckDbSql { canonical_sql } =
            compile_sql(&hir, &lock_for).expect("codegen");
        canonical_sql
    }

    #[test]
    fn extend_directly_on_source_compiles() {
        // 回归：extend 是第一个算子时，曾报「extend 输出字段 f0 无来源」——合法程序被拒绝。
        // 病根是取「输入 schema」时多套了一层；前面隔着一个算子时又碰巧是对的，所以样例一直是绿的。
        let sql = sql_of("Table(\"orders.csv\") as o\n  .extend({ x = 1 })\n  .collect()\n");
        assert!(
            sql.contains("SELECT \"f0\", \"f1\", \"f2\", 1 AS \"f3\" FROM (SELECT \"t0\".\"status\" AS \"f0\""),
            "{sql}"
        );
    }

    /// `src` 里 join 节点的编号（SQL 里表别名 `l<n>` / `r<n>` 用的就是它）。
    /// 从 HIR 里读，不在测试里手数：右表的扫描也是节点，编号会把它算进去。
    fn join_node_id(src: &str) -> usize {
        lower(src)
            .nodes
            .iter()
            .position(|n| {
                matches!(
                    n,
                    HirNode::Transform {
                        kind: TransformKind::Join { .. },
                        ..
                    }
                )
            })
            .expect("src 里应有一个 join")
    }

    #[test]
    fn join_lists_left_and_right_columns_once() {
        // 回归 1：join 直接接在源后面时，曾生成 `SELECT , "f0", …`——
        // 编译「成功」，坏掉的 SQL 进了 manifest，到 DuckDB 上才报语法错误
        let first_src = "Table(\"orders.csv\") as a\n  .join(Table(\"orders.csv\") as b, on = a.status == b.status)\n  .collect()\n";
        let first = sql_of(first_src);
        let j = join_node_id(first_src);
        assert!(
            first.contains(&format!(
                "SELECT \"l{j}\".\"f0\", \"l{j}\".\"f1\", \"l{j}\".\"f2\", \"r{j}\".\"f3\", \"r{j}\".\"f4\", \"r{j}\".\"f5\" FROM ("
            )),
            "{first}"
        );

        // 回归 2：join 在另一个算子之后时，左表的列曾在列表里出现两遍
        let filtered_src = "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .join(Table(\"orders.csv\") as b, on = a.status == b.status)\n  .collect()\n";
        let after_filter = sql_of(filtered_src);
        let j = join_node_id(filtered_src);
        assert!(
            after_filter.contains(&format!(
                "SELECT \"l{j}\".\"f0\", \"l{j}\".\"f1\", \"l{j}\".\"f2\", \"r{j}\".\"f3\", \"r{j}\".\"f4\", \"r{j}\".\"f5\" FROM ("
            )),
            "{after_filter}"
        );
        // 左侧确实是过滤之后的结果，而不是绕过了 filter 直接接在源上
        let left_side = after_filter
            .split(&format!("AS \"l{j}\" JOIN ("))
            .next()
            .unwrap_or("");
        assert!(left_side.contains("WHERE"), "{after_filter}");

        // 任何一层 SELECT 的列表都不该是空的
        for sql in [&first, &after_filter] {
            assert!(!sql.contains("SELECT ,"), "{sql}");
            assert!(!sql.contains(", ,"), "{sql}");
        }
    }

    #[test]
    fn emits_canonical_sql_with_p0_guards() {
        let hir = build_m0_hir();
        let artifact = compile_sql(&hir, &lock_for).expect("codegen");
        let BackendArtifact::DuckDbSql { canonical_sql } = artifact;
        // 前言：确定性引擎配置
        assert!(canonical_sql.starts_with(&format!("{ARTIFACT_HEADER}\nSET threads TO 1;")));
        // 判空 → IS NULL / IS NOT NULL（经语义层 is_null lowering）
        assert!(
            canonical_sql.contains("IS NOT NULL"),
            "判空必须生成 IS NOT NULL"
        );
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

    // ---------- 第 10 章：融合 ----------

    /// 嵌套层数：每个 `FROM (` 是一层。
    fn layers(sql: &str) -> usize {
        sql.matches("FROM (").count()
    }

    fn occurrences(sql: &str, needle: &str) -> usize {
        sql.matches(needle).count()
    }

    /// `src` 里第一个满足条件的节点的编号（SQL 里的表别名 `s<n>` 用的就是它）。
    /// 从 HIR 里读，不在测试里手数。
    fn node_id_where(src: &str, f: impl Fn(&HirNode) -> bool) -> usize {
        lower(src)
            .nodes
            .iter()
            .position(f)
            .expect("src 里应有这样的节点")
    }

    /// 只有一个算子的流水线：SQL 正文必须与 v1 逐字相同。
    ///
    /// 融合只在「相邻且可并」时才改变 SQL。有了这条性质，v1 → v2 的差别就可以被完整地枚举：
    /// 不相邻可并的地方，一个字节都没动。
    #[test]
    fn single_operator_pipelines_keep_the_v1_text() {
        let scan = "SELECT \"t0\".\"status\" AS \"f0\", \"t0\".\"amount\" AS \"f1\", \"t0\".\"customer_id\" AS \"f2\" FROM read_csv('orders.csv', header = true, columns = {'status': 'VARCHAR', 'amount': 'BIGINT', 'customer_id': 'VARCHAR'}) AS \"t0\"";
        // (算子, 最外层的选择列表, 中间那一层)
        let all = "\"f0\" AS \"status\", \"f1\" AS \"amount\", \"f2\" AS \"customer_id\"";
        let cases = [
            (
                ".filter(a.amount > 1)",
                all,
                format!(
                    "SELECT \"f0\", \"f1\", \"f2\" FROM ({scan}) AS \"s1\" WHERE (((\"f1\") > (1)))"
                ),
            ),
            (
                ".extend({ x = 1 })",
                "\"f0\" AS \"status\", \"f1\" AS \"amount\", \"f2\" AS \"customer_id\", \"f3\" AS \"x\"",
                format!("SELECT \"f0\", \"f1\", \"f2\", 1 AS \"f3\" FROM ({scan}) AS \"s1\""),
            ),
            (
                ".select(a.status, a.customer_id)",
                "\"f0\" AS \"status\", \"f2\" AS \"customer_id\"",
                format!("SELECT \"f0\", \"f2\" FROM ({scan}) AS \"s1\""),
            ),
            (
                ".distinct()",
                all,
                format!("SELECT DISTINCT \"f0\", \"f1\", \"f2\" FROM ({scan}) AS \"s1\""),
            ),
            (
                ".sort(a.amount desc)",
                all,
                format!(
                    "SELECT \"f0\", \"f1\", \"f2\" FROM ({scan}) AS \"s1\" ORDER BY \"f1\" DESC NULLS LAST"
                ),
            ),
            (
                ".take(5)",
                all,
                format!("SELECT \"f0\", \"f1\", \"f2\" FROM ({scan}) AS \"s1\" LIMIT 5"),
            ),
        ];
        for (op, outer, layer) in cases {
            let sql = sql_of(&format!(
                "Table(\"orders.csv\") as a\n  {op}\n  .collect()\n"
            ));
            let expected = format!(
                "{ARTIFACT_HEADER}\nSET threads TO 1;\nSELECT {outer} FROM ({layer}) AS \"final\";\n"
            );
            assert_eq!(sql, expected, "{op}");
        }
    }

    /// 连续的 filter 并进同一层的 WHERE。
    #[test]
    fn consecutive_filters_share_one_layer() {
        let sql = sql_of(
            "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .filter(a.amount < 9)\n  .filter(a.status == \"paid\")\n  .collect()\n",
        );
        assert_eq!(layers(&sql), 2, "{sql}");
        assert_eq!(occurrences(&sql, " WHERE "), 1, "{sql}");
        assert_eq!(occurrences(&sql, " AND "), 2, "{sql}");
        // 三个谓词各出现一次
        for pred in [
            "((\"f1\") > (1))",
            "((\"f1\") < (9))",
            "((\"f0\") = ('paid'))",
        ] {
            assert_eq!(occurrences(&sql, pred), 1, "{pred} in {sql}");
        }
    }

    /// AND 树必须是平衡的：括号深度只随谓词个数取对数增长。
    #[test]
    fn and_tree_is_balanced() {
        let v = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(and_all(&v(&["p1"])), "p1");
        assert_eq!(and_all(&v(&["p1", "p2"])), "(p1 AND p2)");
        assert_eq!(and_all(&v(&["p1", "p2", "p3"])), "((p1 AND p2) AND p3)");
        assert_eq!(
            and_all(&v(&["p1", "p2", "p3", "p4"])),
            "((p1 AND p2) AND (p3 AND p4))"
        );

        let many: Vec<String> = (0..20_000).map(|i| format!("p{i}")).collect();
        let tree = and_all(&many);
        let (mut depth, mut max) = (0usize, 0usize);
        for c in tree.chars() {
            if c == '(' {
                depth += 1;
                max = max.max(depth);
            } else if c == ')' {
                depth -= 1;
            }
        }
        assert_eq!(depth, 0, "括号应当配对");
        // 左结合的写法会有 19 999 层；平衡树只有 ⌈log₂ 20000⌉ = 15 层
        assert!((14..=16).contains(&max), "AND 树深度 {max}");
    }

    /// filter 并进当前层的条件：它引用的列都直接来自这一层的 FROM。
    #[test]
    fn a_filter_joins_the_layer_only_if_its_columns_come_from_from() {
        // 谓词引用的是输入本来就有的列：并进 extend 所在的那一层
        let old_col = sql_of(
            "Table(\"orders.csv\") as a\n  .extend({ x = a.amount * 2 })\n  .filter(a.amount > 1)\n  .collect()\n",
        );
        assert_eq!(layers(&old_col), 2, "{old_col}");
        assert_eq!(occurrences(&old_col, " AS \"f3\""), 1, "{old_col}");
        assert_eq!(occurrences(&old_col, " WHERE "), 1, "{old_col}");

        // 谓词引用的是这一层新算出来的列：SQL 的 WHERE 看不到同一个 SELECT 里的别名，必须开新的一层
        let src = "Table(\"orders.csv\") as a\n  .extend({ x = a.amount * 2 })\n  .filter(x > 2)\n  .collect()\n";
        let new_col = sql_of(src);
        assert_eq!(layers(&new_col), 3, "{new_col}");
        let f = node_id_where(src, |n| {
            matches!(
                n,
                HirNode::Transform {
                    kind: TransformKind::Filter { .. },
                    ..
                }
            )
        });
        assert!(
            new_col.contains(&format!(") AS \"s{f}\" WHERE (")),
            "{new_col}"
        );
    }

    /// select 与 sort 并进 filter 开的那一层；take 自己一层。
    #[test]
    fn select_and_sort_merge_into_the_filter_layer_but_take_does_not() {
        let fused = sql_of(
            "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .select(a.status, a.amount)\n  .sort(a.amount)\n  .collect()\n",
        );
        assert_eq!(layers(&fused), 2, "{fused}");
        assert_eq!(occurrences(&fused, " ORDER BY "), 1, "{fused}");
        assert!(
            fused.contains("SELECT \"f0\", \"f1\" FROM (SELECT \"t0\"."),
            "{fused}"
        );

        // ORDER BY 与 LIMIT 写在同一个 SELECT 里，DuckDB 可能走另一条执行路径（top-N），
        // 与「先完整排序、再截取」并不保证在键值相等时选出同样的行。所以 take 不并
        let src = "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .sort(a.amount)\n  .take(3)\n  .collect()\n";
        let with_take = sql_of(src);
        assert_eq!(layers(&with_take), 3, "{with_take}");
        let t = node_id_where(src, |n| {
            matches!(
                n,
                HirNode::Transform {
                    kind: TransformKind::Take { .. },
                    ..
                }
            )
        });
        assert!(
            with_take.contains(&format!(") AS \"s{t}\" LIMIT 3")),
            "{with_take}"
        );
    }

    /// take、distinct、聚合是屏障：它们后面的 filter 若并进 WHERE，就跑到了它们前面。
    #[test]
    fn take_and_distinct_are_barriers() {
        let cases = [
            // take 之后的 filter：并进 WHERE 就是在 LIMIT 之前过滤
            ".filter(a.amount > 1)\n  .take(2)\n  .filter(a.status == \"x\")",
            // distinct 之后的 filter：并进 WHERE 就是在去重之前过滤
            ".filter(a.amount > 1)\n  .distinct()\n  .filter(a.status == \"x\")",
        ];
        for ops in cases {
            let sql = sql_of(&format!(
                "Table(\"orders.csv\") as a\n  {ops}\n  .collect()\n"
            ));
            // 三个块（filter 开的层、屏障自己的层、屏障之后的 filter 开的层）+ 最外层 final
            assert_eq!(layers(&sql), 4, "{ops}\n{sql}");
            assert_eq!(occurrences(&sql, " WHERE "), 2, "{ops}\n{sql}");
        }
        // take 自己的一层里只有 LIMIT：ORDER BY 留在它里面那一层（第 10 章规则三）。
        // 节点编号：源 0、filter 1（开 s1）、sort 2（并进 s1）、take 3（开 s3）
        let sorted_then_taken = sql_of(
            "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .sort(a.amount)\n  .take(3)\n  .collect()\n",
        );
        assert_eq!(layers(&sorted_then_taken), 3, "{sorted_then_taken}");
        assert_eq!(occurrences(&sorted_then_taken, " ORDER BY "), 1);
        // SQL 由外向内嵌套：s3 的 FROM 里是排好序的 s1，s3 自己以 LIMIT 收尾，后面没有 ORDER BY
        assert!(
            sorted_then_taken.contains(" NULLS LAST) AS \"s3\" LIMIT 3) AS \"final\";"),
            "{sorted_then_taken}"
        );
    }

    /// 聚合是屏障的唯一例外：紧跟其后的 filter 是「对分组结果过滤」，并进它的 HAVING（第 11 章）。
    #[test]
    fn a_filter_right_after_aggregation_becomes_having() {
        let sql = sql_of(
            "Table(\"orders.csv\") as a\n  .filter(a.amount > 1)\n  .group_by(a.status)\n  .agg(n = count(), total = sum(a.amount))\n  .filter(n > 0 && total > 100)\n  .select(status, n)\n  .sort(n desc)\n  .collect()\n",
        );
        // filter 开的层、聚合层（HAVING 并了进去）、select 开的层（sort 并了进去）、最外层 final
        assert_eq!(layers(&sql), 4, "{sql}");
        // HAVING 紧跟在 GROUP BY 之后；子句顺序是 SQL 规定的 WHERE → GROUP BY → HAVING → ORDER BY
        assert!(sql.contains(" GROUP BY \"f0\" HAVING ("), "{sql}");
        let pos = |needle: &str| {
            sql.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {sql}"))
        };
        assert!(
            pos(" WHERE ") < pos(" GROUP BY ")
                && pos(" GROUP BY ") < pos(" HAVING ")
                && pos(" HAVING ") < pos(" ORDER BY "),
            "{sql}"
        );
        // 引用聚合输出处换成它的表达式：标准 SQL 的 HAVING 看不到选择列表的别名。
        // COUNT(*) 出现两次——选择列表一次，HAVING 里一次
        assert_eq!(occurrences(&sql, "COUNT(*)"), 2, "{sql}");
        // 分组之前的那个 filter 仍然在 WHERE 里
        assert_eq!(occurrences(&sql, " WHERE "), 1, "{sql}");

        // HAVING 只吸收「紧跟」聚合的 filter；中间隔了别的算子，filter 就回到 WHERE
        let apart = sql_of(
            "Table(\"orders.csv\") as a\n  .agg(n = count())\n  .select(n)\n  .filter(n > 0)\n  .collect()\n",
        );
        assert_eq!(occurrences(&apart, " HAVING "), 0, "{apart}");
        assert_eq!(occurrences(&apart, " WHERE "), 1, "{apart}");

        // 全局聚合（没有 GROUP BY）同样适用
        let global = sql_of(
            "Table(\"orders.csv\") as a\n  .agg(total = sum(a.amount))\n  .filter(total > 100)\n  .collect()\n",
        );
        assert!(global.contains(" HAVING ("), "{global}");
        assert!(!global.contains("GROUP BY"), "{global}");
    }

    /// 聚合之后的 select、sort 不能并进聚合层：各自该开层的开层。
    #[test]
    fn select_after_aggregation_opens_a_layer_and_sort_joins_it() {
        let agg = sql_of(
            "Table(\"orders.csv\") as a\n  .group_by(a.status)\n  .agg(n = count())\n  .select(status, n)\n  .sort(n desc)\n  .collect()\n",
        );
        // 聚合层、select 开的层（sort 并了进去）、最外层 final
        assert_eq!(layers(&agg), 3, "{agg}");
        assert!(agg.contains("GROUP BY"), "{agg}");
        assert_eq!(occurrences(&agg, " ORDER BY "), 1, "{agg}");
        assert_eq!(occurrences(&agg, " HAVING "), 0, "{agg}");
    }

    /// 受限 cast 的 SQL（07 §7.8）：每条规则都先用守卫把引擎的行为收敛到规范，再取值。
    #[test]
    fn cast_sql_narrows_the_engine_to_the_spec() {
        let cast_of = |expr: &str| {
            sql_of(&format!(
                "Table(\"orders.csv\") as a\n  .extend({{ x = {expr} }})\n  .collect()\n"
            ))
        };

        // i64 → f64：永不失败，直接 CAST；null 经 CAST 仍是 null
        let sql = cast_of("cast(a.amount as f64)");
        assert!(sql.contains("CAST((\"f1\") AS DOUBLE) AS \"f3\""), "{sql}");

        // f64 → i64：先排除非有限与越界，再按取偶取整，最后 CAST
        let sql = cast_of("cast(2.5 as i64)");
        let x = "CAST(2.5 AS DOUBLE)";
        assert!(
            sql.contains(&format!("CASE WHEN ({x}) IS NULL THEN NULL WHEN NOT (isfinite(({x}))) THEN NULL WHEN ({x}) >= 9223372036854775808.0 OR ({x}) < -9223372036854775808.0 THEN NULL ELSE CAST(CASE WHEN")),
            "{sql}"
        );
        assert!(sql.contains("MOD(CAST(FLOOR(("), "{sql}");
        assert!(sql.contains("AS BIGINT) END AS \"f3\""), "{sql}");

        // string → i64：正则限死写法；HUGEINT 中转查值域；全程 TRY_CAST，不给引擎报错的机会
        let sql = cast_of("cast(a.status as i64)");
        assert!(
            sql.contains("regexp_matches((\"f0\"), '^[+-]?[0-9]+$')"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "TRY_CAST((\"f0\") AS HUGEINT) BETWEEN -9223372036854775808 AND 9223372036854775807"
            ),
            "{sql}"
        );
        assert!(!sql.contains(" ELSE CAST("), "{sql}");
        assert!(
            sql.contains("ELSE TRY_CAST((\"f0\") AS BIGINT) END AS \"f3\""),
            "{sql}"
        );

        // string → f64：规范文法的正则；TRY_CAST 之后做有限性检查与零规范化
        let sql = cast_of("cast(a.status as f64)");
        assert!(
            sql.contains(
                "regexp_matches((\"f0\"), '^[+-]?([0-9]+(\\.[0-9]*)?|\\.[0-9]+)([eE][+-]?[0-9]+)?$')"
            ),
            "{sql}"
        );
        assert!(sql.contains("TRY_CAST((\"f0\") AS DOUBLE)"), "{sql}");
        assert!(sql.contains("isfinite(") && sql.contains("+ 0.0"), "{sql}");
    }

    /// trim 的字符集是规范里的 Unicode White_Space（25 个码点），不是 DuckDB 默认的那一套。
    #[test]
    fn trim_passes_the_whole_unicode_whitespace_set() {
        let sql =
            sql_of("Table(\"orders.csv\") as a\n  .extend({ x = trim(a.status) })\n  .collect()\n");
        assert!(
            sql.contains("trim((\"f0\"), chr(9) || chr(10) || chr(11) || chr(12) || chr(13) || chr(32) || chr(133) || chr(160) || chr(5760) || "),
            "{sql}"
        );
        assert!(sql.contains("chr(8287) || chr(12288)) AS \"f3\""), "{sql}");
        // 零宽空格 U+200B（8203）不是 White_Space，不在集合里
        assert!(!sql.contains("chr(8203)"), "{sql}");
        assert_eq!(occurrences(&sql, "chr("), 25, "{sql}");
    }

    /// join 开新的一层，但它不是屏障：后面的 filter、extend 可以并进来。
    #[test]
    fn a_join_layer_absorbs_what_follows_it() {
        let src = "Table(\"orders.csv\") as a\n  .join(Table(\"orders.csv\") as b, on = a.status == b.status)\n  .filter(b.amount > 1)\n  .extend({ z = 1 })\n  .collect()\n";
        let sql = sql_of(src);
        assert_eq!(layers(&sql), 2, "{sql}");
        assert_eq!(occurrences(&sql, " JOIN ("), 1, "{sql}");
        assert_eq!(occurrences(&sql, " WHERE "), 1, "{sql}");
        let j = node_id_where(src, |n| {
            matches!(
                n,
                HirNode::Transform {
                    kind: TransformKind::Join { .. },
                    ..
                }
            )
        });
        // 原有的透传列仍然带表别名；新列追加在选择列表的末尾
        assert!(
            sql.contains(&format!(
                "SELECT \"l{j}\".\"f0\", \"l{j}\".\"f1\", \"l{j}\".\"f2\", \"r{j}\".\"f3\", \"r{j}\".\"f4\", \"r{j}\".\"f5\", 1 AS \"f6\" FROM ("
            )),
            "{sql}"
        );
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
        let sql2 = cg.emit_int_arith(pebble_hir::BinOp::Add, "\"f1\"", "\"f2\"");
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
            &NO_SUBST,
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
