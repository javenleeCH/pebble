//! pebble-semantics：解析域/Schema/类型检查 + AST → HIR lowering。
//!
//! 实现 docs/draft/spec/04/05/07 的全部 P0 语义：
//! - 严格 null 传播；`x == null`/`x != null` lowering 为判空内置形式（恒 bool）；
//! - 强 Kleene 逻辑；谓词只认 true；
//! - 流敏非空收缩 NN(p)（可靠但不完备）；
//! - checked-null：可能失败运算静态类型必须可空；编译期可确定的定义域失败报 SEM-016；
//! - 聚合签名按「全局/分组 × 参数可空 × 溢出」推导；
//! - 源别名与输出字段名分属不同命名空间；join 重名裸引用报 SEM-011；
//! - 多错误报告与毒化（第 9 章）：算子里相互独立的部分逐个检查，定义失败的名字被毒化，
//!   之后对它的引用沉默地失败——一个根因只报一次，独立的错误一个不漏；
//! - 语义封闭：本层不做任何「由实现定义」的放行。

use pebble_diagnostic::{Code, DiagBag, Span};
use pebble_hir::{
    AggBindingH, AggCallH, AggFunc, BinOp, BuiltinFn, FieldId, FieldOrigin, FieldSchema, Hir,
    HirNode, NodeBase, NodeId, Projection, SEMANTICS_VERSION, Schema, SchemaLock, SortKeyH,
    SourceId, SourceKind, THirExpr, TerminalKind, TransformKind, Type, UnaryOp, canonical,
};
use pebble_parser::ast;
use std::collections::{BTreeMap, BTreeSet};

/// Schema lock 目录：identity = `table:<locator>` / `memory:<name>`。
pub struct Catalog {
    pub locks: BTreeMap<String, SchemaLock>,
}

impl Catalog {
    pub fn from_locks(locks: Vec<SchemaLock>) -> Self {
        Catalog {
            locks: locks
                .into_iter()
                .map(|l| (l.source.identity(), l))
                .collect(),
        }
    }

    /// 从目录加载全部 `*.lock.json`。
    pub fn load_dir(dir: &std::path::Path) -> Result<Catalog, String> {
        let mut locks = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| format!("读取 lock 目录失败: {}", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
                let lock = SchemaLock::from_json(&text)
                    .map_err(|e| format!("{}: {}", path.display(), e))?;
                locks.push(lock);
            }
        }
        Ok(Catalog::from_locks(locks))
    }

    pub fn get(&self, identity: &str) -> Option<&SchemaLock> {
        self.locks.get(identity)
    }
}

/// 编译入口：AST + Catalog → HIR。存在 Error 诊断时返回 None。
pub fn compile(program: &ast::Program, catalog: &Catalog, diags: &mut DiagBag) -> Option<Hir> {
    let mut lower = Lowerer {
        catalog,
        diags,
        nodes: Vec::new(),
        next_field: 0,
        next_source: 0,
        aliases: BTreeMap::new(),
        history: Vec::new(),
        const_failed: false,
        poisoned: BTreeSet::new(),
    };
    let result = lower.run(&program.pipeline);
    if diags.has_error() {
        return None;
    }
    match result {
        Some(hir) => Some(hir),
        // 没有诊断却没有结果：这是编译器自己的 bug。不能让用户面对一个沉默的失败
        None => {
            diags.push(pebble_diagnostic::Diagnostic::error(
                Code::SemOpPosition,
                "内部错误：语义检查中止，但没有产生任何诊断",
                Span::new(0, 0),
            ));
            None
        }
    }
}

struct Lowerer<'a> {
    catalog: &'a Catalog,
    diags: &'a mut DiagBag,
    nodes: Vec<HirNode>,
    next_field: FieldId,
    next_source: SourceId,
    aliases: BTreeMap<String, SourceId>,
    /// 历史可见 Schema：用于区分 SEM-001（从未存在）与 SEM-012（超出作用域）。
    history: Vec<Schema>,
    /// 同一常量定义域失败只报一次。
    const_failed: bool,
    /// 毒化表：定义失败的字段。之后任何引用它们的表达式都**沉默地**失败——
    /// 那是同一个根因的后果，再报一次就是级联错误（第 9 章）。
    poisoned: BTreeSet<FieldId>,
}

impl Lowerer<'_> {
    /// 沿流水线逐算子做解析域、Schema 与类型检查（08-diagnostics §8.6 第 3 条）。
    ///
    /// 遇到第一个错误就停，是最简单的正确做法——但那样拼错两个字段名就要编译两次。
    /// 这里和第 8 章的解析器是同一套思想的另一半：
    ///
    /// - **独立的部分逐个解析**：extend 的每个绑定、select 的每一项、agg 的每个键与绑定、
    ///   sort 的每个键。一个失败不影响同一算子里其余部分的检查。
    /// - **失败的名字被毒化**：它的输出位置留一个占位并记入 `poisoned`；
    ///   之后任何引用它的表达式**沉默地**失败（见 `resolve_field`）——那是同一个根因的后果。
    /// - **算子不发射，流水线继续**：带着占位 Schema 往下走，后面的算子里独立的错误照常报出。
    /// - **唯一的致命错误是「源未知」**：表的形状都不知道，后面无从检查，直接返回 `None`。
    ///
    /// 返回 `None` 只表示「有错误，没有 HIR」。带占位的中间节点会被丢弃（`compile` 统一把关）。
    fn run(&mut self, pipeline: &ast::Pipeline) -> Option<Hir> {
        // 1. 源节点
        let (mut current_id, mut current_schema) = self.lower_source(&pipeline.source)?;

        // 2. 算子链
        let ops = &pipeline.ops;
        let mut i = 0usize;
        while i < ops.len() {
            match &ops[i] {
                ast::Op::GroupBy { keys, span } => {
                    // group_by 必须紧跟 agg
                    let Some(ast::Op::Agg {
                        bindings: agg_bindings,
                        span: agg_span,
                    }) = ops.get(i + 1)
                    else {
                        self.diags.error(
                            Code::SemOpPosition,
                            "group_by 之后必须且只能紧跟一个 agg",
                            *span,
                        );
                        // 跳过这个孤立的 group_by，继续检查后面的算子
                        i += 1;
                        continue;
                    };
                    let (node_id, schema) = self.lower_aggregate(
                        current_id,
                        &current_schema,
                        Some(keys),
                        agg_bindings,
                        *agg_span,
                    );
                    current_schema = schema;
                    if let Some(n) = node_id {
                        current_id = n;
                    }
                    i += 2;
                }
                ast::Op::Agg {
                    bindings,
                    span: agg_span,
                } => {
                    // 全局聚合（不接 group_by）
                    let (node_id, schema) = self.lower_aggregate(
                        current_id,
                        &current_schema,
                        None,
                        bindings,
                        *agg_span,
                    );
                    current_schema = schema;
                    if let Some(n) = node_id {
                        current_id = n;
                    }
                    i += 1;
                }
                ast::Op::Filter { pred, span } => {
                    if let Some((node_id, schema)) =
                        self.lower_filter(current_id, &current_schema, pred, *span)
                    {
                        current_schema = schema;
                        current_id = node_id;
                    }
                    i += 1;
                }
                ast::Op::Having { pred, span } => {
                    // having 是语法糖（06 §6.4）：必须紧跟在 agg 之后，然后按与 filter 完全
                    // 相同的方式 lowering。位置不对时报告并跳过这个算子，后面的检查照常进行。
                    if !matches!(ops.get(i.wrapping_sub(1)), Some(ast::Op::Agg { .. })) {
                        self.diags
                            .error(Code::SemOpPosition, "having 必须紧跟在 agg 之后", *span);
                        i += 1;
                        continue;
                    }
                    if let Some((node_id, schema)) =
                        self.lower_filter(current_id, &current_schema, pred, *span)
                    {
                        current_schema = schema;
                        current_id = node_id;
                    }
                    i += 1;
                }
                ast::Op::Extend { record, span } => {
                    self.history.push(current_schema.clone());
                    // 绑定名唯一（SEM-014）：后出现的重名报错并被丢弃
                    let is_first = |idx: usize| {
                        record.bindings[..idx]
                            .iter()
                            .all(|p| p.name != record.bindings[idx].name)
                    };
                    for (idx, b) in record.bindings.iter().enumerate() {
                        if !is_first(idx) {
                            self.diags.error(
                                Code::SemDuplicateName,
                                format!("同一 extend 记录中名称 `{}` 重复", b.name),
                                b.span,
                            );
                        }
                    }
                    // RHS 对输入行求值；兄弟绑定互不可见（环境即输入 Schema）。逐个绑定解析
                    let mut hir_bindings = Vec::new();
                    let mut failed = BTreeSet::new();
                    for (idx, b) in record.bindings.iter().enumerate() {
                        if !is_first(idx) {
                            continue;
                        }
                        match self.resolve_expr(&b.value, &current_schema, false) {
                            Some(value) => {
                                self.const_domain_check(&value);
                                let output = self.fresh_field();
                                hir_bindings.push(pebble_hir::Binding {
                                    output,
                                    name: b.name.clone(),
                                    value,
                                });
                            }
                            None => {
                                failed.insert(b.name.clone());
                            }
                        }
                    }
                    // 输出 Schema：覆盖留在原位并换身份；新增按绑定顺序追加
                    let node = self.nodes.len() as NodeId;
                    let mut fields: Vec<FieldSchema> = Vec::new();
                    let mut replaced = BTreeSet::new();
                    for f in &current_schema.fields {
                        let binding = hir_bindings.iter().find(|b| b.name == f.name);
                        let ambiguous = current_schema.by_name(&f.name).len() > 1;
                        match binding {
                            Some(b) if !ambiguous => {
                                // 成功的覆盖：留在原位，换成新的身份
                                replaced.insert(f.name.clone());
                                fields.push(FieldSchema {
                                    id: b.output,
                                    name: f.name.clone(),
                                    ty: b.value.ty().clone(),
                                    origin: FieldOrigin::Derived {
                                        node,
                                        binding: f.name.clone(),
                                    },
                                });
                            }
                            Some(_) => {
                                // 覆盖时输入有多个同名字段：无法确定覆盖的是哪一个
                                self.diags.error(
                                    Code::SemTypeMismatch,
                                    format!(
                                        "extend 覆盖 `{}` 时输入存在多个同名字段，必须先消歧",
                                        f.name
                                    ),
                                    *span,
                                );
                                replaced.insert(f.name.clone());
                                fields.push(self.poison_field(&f.name, node));
                            }
                            None if failed.contains(&f.name) => {
                                // 这个名字指向的是一个失败的新定义：占位并毒化
                                replaced.insert(f.name.clone());
                                fields.push(self.poison_field(&f.name, node));
                            }
                            None => fields.push(f.clone()),
                        }
                    }
                    for b in &hir_bindings {
                        if !replaced.contains(&b.name) {
                            fields.push(FieldSchema {
                                id: b.output,
                                name: b.name.clone(),
                                ty: b.value.ty().clone(),
                                origin: FieldOrigin::Derived {
                                    node,
                                    binding: b.name.clone(),
                                },
                            });
                        }
                    }
                    // 失败的新绑定也按记录里的出现顺序追加（与成功的绑定一致）
                    for b in &record.bindings {
                        if failed.contains(&b.name) && !replaced.contains(&b.name) {
                            replaced.insert(b.name.clone());
                            fields.push(self.poison_field(&b.name, node));
                        }
                    }
                    if failed.is_empty() {
                        let node_id =
                            self.push_node(vec![current_id], Schema { fields }, *span, |base| {
                                HirNode::Transform {
                                    base,
                                    kind: TransformKind::Extend {
                                        bindings: hir_bindings,
                                    },
                                }
                            });
                        current_schema = self.nodes[node_id as usize].base().output.clone();
                        current_id = node_id;
                    } else {
                        // 带着占位继续；算子不发射
                        current_schema = Schema { fields };
                    }
                    i += 1;
                }
                ast::Op::Select { items, span } => {
                    self.history.push(current_schema.clone());
                    let node = self.nodes.len() as NodeId;
                    let mut projections = Vec::new();
                    let mut out_fields = Vec::new();
                    let mut seen_names: BTreeMap<String, Span> = BTreeMap::new();
                    let mut damaged = false;
                    for item in items {
                        // 输出名：有 as 用 as；否则一定是字段引用（PAR-006 已保证），用字段名
                        let name = match &item.alias {
                            Some(alias) => alias.clone(),
                            None => match &item.expr {
                                ast::Expr::Field { name, .. } => name.clone(),
                                _ => unreachable!("PAR-006 已保证无 as 的项是字段引用"),
                            },
                        };
                        if let Some(prev) = seen_names.insert(name.clone(), item.span) {
                            self.diags.error(
                                Code::SchDuplicateOutput,
                                format!("select 输出名称 `{}` 重复", name),
                                item.span.merge(prev),
                            );
                            damaged = true; // 重名的这一项丢弃
                            continue;
                        }
                        match self.resolve_expr(&item.expr, &current_schema, false) {
                            Some(resolved) => {
                                self.const_domain_check(&resolved);
                                if item.alias.is_none() {
                                    // 无 as：字段引用，保留身份
                                    let field = match &resolved {
                                        THirExpr::Field { field, .. } => *field,
                                        _ => unreachable!("PAR-006 已保证字段引用"),
                                    };
                                    let src = current_schema
                                        .fields
                                        .iter()
                                        .find(|f| f.id == field)
                                        .expect("字段已解析");
                                    projections.push(Projection {
                                        expr: resolved,
                                        output: field,
                                        name: name.clone(),
                                    });
                                    out_fields.push(src.clone());
                                } else {
                                    let output = self.fresh_field();
                                    let ty = resolved.ty().clone();
                                    projections.push(Projection {
                                        expr: resolved,
                                        output,
                                        name: name.clone(),
                                    });
                                    out_fields.push(FieldSchema {
                                        id: output,
                                        name: name.clone(),
                                        ty,
                                        origin: FieldOrigin::Derived {
                                            node,
                                            binding: name.clone(),
                                        },
                                    });
                                }
                            }
                            None => {
                                damaged = true;
                                out_fields.push(self.poison_field(&name, node));
                            }
                        }
                    }
                    if damaged {
                        current_schema = Schema { fields: out_fields };
                    } else {
                        let node_id = self.push_node(
                            vec![current_id],
                            Schema { fields: out_fields },
                            *span,
                            |base| HirNode::Transform {
                                base,
                                kind: TransformKind::Select { projections },
                            },
                        );
                        current_schema = self.nodes[node_id as usize].base().output.clone();
                        current_id = node_id;
                    }
                    i += 1;
                }
                ast::Op::Distinct { span } => {
                    self.history.push(current_schema.clone());
                    let node_id =
                        self.push_node(vec![current_id], current_schema.clone(), *span, |base| {
                            HirNode::Transform {
                                base,
                                kind: TransformKind::Distinct,
                            }
                        });
                    current_id = node_id;
                    i += 1;
                }
                ast::Op::Sort { keys, span } => {
                    self.history.push(current_schema.clone());
                    let mut hir_keys = Vec::new();
                    let mut damaged = false;
                    for k in keys {
                        match self.resolve_expr(&k.expr, &current_schema, false) {
                            Some(resolved) => {
                                self.const_domain_check(&resolved);
                                match resolved.ty().base() {
                                    Type::I64 | Type::F64 | Type::Str => hir_keys.push(SortKeyH {
                                        expr: resolved,
                                        descending: k.descending,
                                    }),
                                    _ => {
                                        self.diags.error(
                                            Code::SemOpUnsupported,
                                            "sort 键类型必须可排序（i64/f64/string）",
                                            k.span,
                                        );
                                        damaged = true;
                                    }
                                }
                            }
                            None => damaged = true,
                        }
                    }
                    if !damaged {
                        let node_id = self.push_node(
                            vec![current_id],
                            current_schema.clone(),
                            *span,
                            |base| HirNode::Transform {
                                base,
                                kind: TransformKind::Sort { keys: hir_keys },
                            },
                        );
                        current_id = node_id;
                    }
                    i += 1;
                }
                ast::Op::Take { n, span } => {
                    if *n < 0 {
                        self.diags
                            .error(Code::SemTypeMismatch, "take 的参数必须是非负整数", *span);
                        i += 1;
                        continue;
                    }
                    self.history.push(current_schema.clone());
                    let node_id =
                        self.push_node(vec![current_id], current_schema.clone(), *span, |base| {
                            HirNode::Transform {
                                base,
                                kind: TransformKind::Take { n: *n },
                            }
                        });
                    current_id = node_id;
                    i += 1;
                }
                ast::Op::Join { right, on, span } => {
                    self.history.push(current_schema.clone());
                    // 右表的 Schema 未知是致命的（同主源）
                    let (right_id, right_schema) = self.lower_source(right)?;
                    let combined = Schema {
                        fields: current_schema
                            .fields
                            .iter()
                            .chain(right_schema.fields.iter())
                            .cloned()
                            .collect(),
                    };
                    let node_id = match self.resolve_expr(on, &combined, true) {
                        Some(resolved) => {
                            if self.check_predicate(&resolved).is_some() {
                                self.const_domain_check(&resolved);
                                Some(self.push_node(
                                    vec![current_id, right_id],
                                    combined.clone(),
                                    *span,
                                    |base| HirNode::Transform {
                                        base,
                                        kind: TransformKind::Join { on: resolved },
                                    },
                                ))
                            } else {
                                None
                            }
                        }
                        None => None,
                    };
                    current_schema = match node_id {
                        Some(n) => {
                            current_id = n;
                            self.nodes[n as usize].base().output.clone()
                        }
                        // 连接条件没能解析：算子不发射，但两侧的字段都对下游可见
                        None => combined,
                    };
                    i += 1;
                }
            }
        }

        // 3. 终端
        let terminal = match &pipeline.terminal {
            Some(t) => t,
            None => {
                self.diags.error(
                    Code::SemOpPosition,
                    "流水线必须以终端算子（write/collect/print）结尾",
                    pipeline.span,
                );
                return None;
            }
        };
        self.history.push(current_schema.clone());
        let kind = match terminal {
            ast::TerminalOp::Write { path, .. } => TerminalKind::Write { path: path.clone() },
            ast::TerminalOp::Collect { .. } => TerminalKind::Collect,
            ast::TerminalOp::Print { .. } => TerminalKind::Print,
        };
        let root = self.push_node(vec![current_id], current_schema, terminal.span(), |base| {
            HirNode::Terminal { base, kind }
        });

        let hir = Hir {
            nodes: std::mem::take(&mut self.nodes),
            root,
            semantics_version: SEMANTICS_VERSION.to_string(),
        };
        if let Err(e) = hir.validate() {
            // 内部不变量失败属于编译器 bug；以诊断形式暴露而不是静默
            self.diags.push(pebble_diagnostic::Diagnostic::error(
                Code::SemOpPosition,
                format!("HIR 不变量校验失败: {}", e),
                Span::new(0, 0),
            ));
            return None;
        }
        Some(hir)
    }

    /// filter 的 lowering：解析谓词、检查类型、发射节点；输出 Schema 是非空收缩之后的。
    /// 谓词解析不出来或不是 bool 时返回 `None`（错误已报告，或是毒化）：算子跳过，Schema 不变。
    /// `having` 走的是同一个函数——它在语义上就是紧跟 agg 的 filter。
    fn lower_filter(
        &mut self,
        current_id: NodeId,
        current_schema: &Schema,
        pred: &ast::Expr,
        span: Span,
    ) -> Option<(NodeId, Schema)> {
        self.history.push(current_schema.clone());
        let resolved = self.resolve_expr(pred, current_schema, false)?;
        self.check_predicate(&resolved)?;
        self.const_domain_check(&resolved);
        let narrowed = self.narrow(current_schema, &resolved);
        let node_id = self.push_node(vec![current_id], narrowed, span, |base| {
            HirNode::Transform {
                base,
                kind: TransformKind::Filter { pred: resolved },
            }
        });
        let schema = self.nodes[node_id as usize].base().output.clone();
        Some((node_id, schema))
    }

    // ---------- 源 ----------

    fn lower_source(&mut self, source: &ast::SourceOp) -> Option<(NodeId, Schema)> {
        let (identity, lock_span) = match source {
            ast::SourceOp::Table { path, span, .. } => (format!("table:{}", path), *span),
            ast::SourceOp::MemoryTable { name, span, .. } => (format!("memory:{}", name), *span),
        };
        let lock = match self.catalog.get(&identity) {
            Some(l) => l,
            None => {
                self.diags.error(
                    Code::SchNoSchema,
                    format!("数据源 `{}` 缺少 Schema lock（SCH-001）", identity),
                    lock_span,
                );
                return None;
            }
        };
        let lock_hash = match lock.validate() {
            Ok(h) => h,
            Err(e) => {
                self.diags.error(Code::SchLockMismatch, e, lock_span);
                return None;
            }
        };
        let source_id = self.next_source;
        self.next_source += 1;
        if let Some(alias) = source.alias() {
            let previous = self.aliases.insert(alias.to_string(), source_id);
            if let Some(old) = previous {
                // 别名重复（SEM-004）：报告，并保留先前的绑定；后面的检查照常进行
                self.aliases.insert(alias.to_string(), old);
                self.diags.error(
                    Code::SemAliasDuplicate,
                    format!("源别名 `{}` 重复", alias),
                    lock_span,
                );
            }
        }
        let fields: Vec<FieldSchema> = lock
            .fields
            .iter()
            .map(|f| FieldSchema {
                id: self.fresh_field(),
                name: f.name.clone(),
                ty: f.ty.to_type(f.optional),
                origin: FieldOrigin::Source {
                    source: source_id,
                    source_field: f.name.clone(),
                },
            })
            .collect();
        let schema = Schema { fields };
        let kind = match source {
            ast::SourceOp::Table { path, .. } => SourceKind::Table {
                locator: path.clone(),
                schema_lock_hash: lock_hash,
            },
            ast::SourceOp::MemoryTable { name, .. } => SourceKind::MemoryTable {
                name: name.clone(),
                schema_lock_hash: lock_hash,
            },
        };
        let node_id = self.push_node(vec![], schema, lock_span, |base| HirNode::Source {
            base,
            source: source_id,
            kind,
        });
        let schema = self.nodes[node_id as usize].base().output.clone();
        Some((node_id, schema))
    }

    // ---------- 聚合 ----------

    /// 聚合节点（分组 / 全局）。返回 `(节点, 输出 Schema)`：有任一键或绑定失败时节点为 `None`，
    /// 但输出 Schema 照常给出——失败的名字是毒化占位，流水线带着它继续。
    #[allow(clippy::too_many_arguments)]
    fn lower_aggregate(
        &mut self,
        input_id: NodeId,
        input_schema: &Schema,
        group: Option<&Vec<ast::GroupKey>>,
        bindings: &Vec<ast::AggBinding>,
        span: Span,
    ) -> (Option<NodeId>, Schema) {
        self.history.push(input_schema.clone());
        let grouped = group.is_some();
        let node = self.nodes.len() as NodeId;
        let mut damaged = false;

        // 分组键：MVP 只接受字段引用，输出名去限定。逐个键检查
        let mut group_keys = Vec::new();
        let mut key_names: BTreeMap<String, Span> = BTreeMap::new();
        let mut failed_keys: Vec<String> = Vec::new();
        if let Some(keys) = group {
            for k in keys {
                if k.alias.is_some() {
                    self.diags.error(
                        Code::SemTypeMismatch,
                        "MVP 不支持分组键 `as` 命名（表达式分组键与命名后置）",
                        k.span,
                    );
                    damaged = true;
                    continue;
                }
                let field_expr = match &k.expr {
                    ast::Expr::Field { alias, name, span } => (alias, name, *span),
                    _ => {
                        self.diags.error(
                            Code::SemTypeMismatch,
                            "MVP 的 group_by 键只接受字段引用",
                            k.span,
                        );
                        damaged = true;
                        continue;
                    }
                };
                let name = field_expr.1.clone();
                if let Some(prev) = key_names.insert(name.clone(), k.span) {
                    self.diags.error(
                        Code::SchGroupKeyRename,
                        format!("分组键去限定后重名 `{}`（SCH-004）", name),
                        k.span.merge(prev),
                    );
                    damaged = true;
                    continue;
                }
                match self.resolve_field(field_expr.0, field_expr.1, field_expr.2, input_schema) {
                    Some(resolved) => {
                        let output = self.fresh_field();
                        group_keys.push(pebble_hir::GroupKeyH {
                            value: resolved,
                            output,
                            name: name.clone(),
                        });
                    }
                    None => {
                        // 已报告，或是毒化。组级的这个名字留一个占位
                        damaged = true;
                        failed_keys.push(name);
                    }
                }
            }
        }

        // 聚合绑定。逐个绑定检查
        let mut hir_bindings = Vec::new();
        let mut all_names: BTreeMap<String, Span> = key_names.clone();
        let mut failed: Vec<String> = Vec::new();
        for b in bindings {
            if let Some(prev) = all_names.insert(b.name.clone(), b.span) {
                if key_names.contains_key(&b.name) {
                    self.diags.error(
                        Code::SchDuplicateOutput,
                        format!("聚合绑定 `{}` 与分组键重名（SCH-003）", b.name),
                        b.span.merge(prev),
                    );
                } else {
                    self.diags.error(
                        Code::SemDuplicateName,
                        format!("同一 agg 绑定列表中名称 `{}` 重复（SEM-014）", b.name),
                        b.span.merge(prev),
                    );
                }
                damaged = true;
                continue;
            }
            match self.lower_agg_binding(b, input_schema, grouped) {
                Ok(binding) => hir_bindings.push(binding),
                Err(()) => {
                    damaged = true;
                    failed.push(b.name.clone());
                }
            }
        }

        // 输出 Schema：分组键 + 聚合绑定（失败的是毒化占位）
        let mut fields = Vec::new();
        for k in &group_keys {
            fields.push(FieldSchema {
                id: k.output,
                name: k.name.clone(),
                ty: k.value.ty().clone(),
                origin: FieldOrigin::Derived {
                    node,
                    binding: k.name.clone(),
                },
            });
        }
        for name in &failed_keys {
            fields.push(self.poison_field(name, node));
        }
        for b in &hir_bindings {
            let ty = self
                .agg_result_type(&b.call.func, b.call.arg.as_ref(), grouped, span)
                .expect("已校验");
            fields.push(FieldSchema {
                id: b.output,
                name: b.name.clone(),
                ty,
                origin: FieldOrigin::Derived {
                    node,
                    binding: b.name.clone(),
                },
            });
        }
        for name in &failed {
            fields.push(self.poison_field(name, node));
        }
        let schema = Schema { fields };

        if damaged {
            return (None, schema);
        }
        let node_id = self.push_node(vec![input_id], schema.clone(), span, |base| {
            HirNode::Aggregate {
                base,
                group_keys,
                bindings: hir_bindings,
            }
        });
        debug_assert_eq!(node_id, node);
        (Some(node_id), schema)
    }

    /// 一个聚合绑定：参数数量、参数解析、返回类型。失败时错误已报告（或是毒化）。
    fn lower_agg_binding(
        &mut self,
        b: &ast::AggBinding,
        input_schema: &Schema,
        grouped: bool,
    ) -> Result<AggBindingH, ()> {
        let (arg, call_span) = (&b.call.arg, b.call.span);
        let func = to_hir_agg(&b.call.func);
        // count 可带/不带参数；其余聚合必须恰好一个参数
        let arity_ok = matches!(func, AggFunc::Count) || arg.is_some();
        if !arity_ok {
            self.diags.error(
                Code::SemTypeMismatch,
                format!("`{}` 的参数数量不合法", func_name(&func)),
                call_span,
            );
            return Err(());
        }
        let resolved_arg = match arg {
            Some(a) => Some(
                self.resolve_with_base(a, input_schema, None, false)
                    .ok_or(())?,
            ),
            None => None,
        };
        if let Some(ra) = &resolved_arg {
            self.const_domain_check(ra);
        }
        // 返回类型不合法时错误已由 agg_result_type 报告
        self.agg_result_type(&func, resolved_arg.as_ref(), grouped, call_span)
            .ok_or(())?;
        let output = self.fresh_field();
        Ok(AggBindingH {
            output,
            name: b.name.clone(),
            call: AggCallH {
                func,
                arg: resolved_arg,
            },
        })
    }

    /// 聚合返回类型（05 §5.3 签名表）。
    fn agg_result_type(
        &mut self,
        func: &AggFunc,
        arg: Option<&THirExpr>,
        grouped: bool,
        span: Span,
    ) -> Option<Type> {
        use AggFunc::*;
        match func {
            Count => Some(Type::I64),
            Sum => {
                let a = arg.expect("arity 已校验");
                match a.ty().base() {
                    Type::I64 => Some(Type::Opt(Box::new(Type::I64))),
                    Type::F64 => Some(Type::Opt(Box::new(Type::F64))),
                    _ => {
                        self.diags.error(
                            Code::SemOpUnsupported,
                            "sum 的参数必须是数值（i64/f64）",
                            span,
                        );
                        None
                    }
                }
            }
            Min | Max => {
                let a = arg.expect("arity 已校验");
                match a.ty().base() {
                    Type::I64 | Type::F64 | Type::Str => {
                        let base = a.ty().base().clone();
                        if grouped && !a.ty().is_opt() {
                            Some(base)
                        } else {
                            Some(Type::Opt(Box::new(base)))
                        }
                    }
                    _ => {
                        self.diags.error(
                            Code::SemOpUnsupported,
                            "min/max 的参数必须是 i64/f64/string",
                            span,
                        );
                        None
                    }
                }
            }
            Avg => {
                let a = arg.expect("arity 已校验");
                match a.ty().base() {
                    Type::I64 | Type::F64 => {
                        if grouped && !a.ty().is_opt() {
                            Some(Type::F64)
                        } else {
                            Some(Type::Opt(Box::new(Type::F64)))
                        }
                    }
                    _ => {
                        self.diags.error(
                            Code::SemOpUnsupported,
                            "avg 的参数必须是数值（i64/f64）",
                            span,
                        );
                        None
                    }
                }
            }
        }
    }

    // ---------- 表达式 ----------

    fn resolve_expr(
        &mut self,
        expr: &ast::Expr,
        env: &Schema,
        in_join_on: bool,
    ) -> Option<THirExpr> {
        match expr {
            ast::Expr::Literal(lit, span) => self.literal(lit, *span),
            ast::Expr::Field { alias, name, span } => self.resolve_field(alias, name, *span, env),
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => self.resolve_binary(op, left, right, *span, env, in_join_on),
            ast::Expr::Unary { op, expr, span } => {
                use ast::UnaryOp::*;
                match op {
                    Negate => {
                        let inner = self.resolve_expr(expr, env, in_join_on)?;
                        let ty = match inner.ty().base() {
                            Type::I64 => Type::Opt(Box::new(Type::I64)),
                            Type::F64 => {
                                if inner.ty().is_opt() {
                                    Type::Opt(Box::new(Type::F64))
                                } else {
                                    Type::F64
                                }
                            }
                            _ => {
                                self.diags.error(
                                    Code::SemOpUnsupported,
                                    "一元 `-` 仅接受数值",
                                    *span,
                                );
                                return None;
                            }
                        };
                        Some(THirExpr::Unary {
                            op: UnaryOp::Negate,
                            expr: Box::new(inner),
                            ty,
                            span: *span,
                        })
                    }
                    Not => {
                        let inner = if is_null_literal(expr) {
                            // !null → ?bool（Kleene：!null = null）
                            THirExpr::Literal {
                                value: pebble_hir::Literal::Null,
                                ty: Type::Opt(Box::new(Type::Bool)),
                                span: expr.span(),
                            }
                        } else {
                            self.resolve_expr(expr, env, in_join_on)?
                        };
                        if !matches!(inner.ty().base(), Type::Bool) {
                            self.diags.error(
                                Code::SemOpUnsupported,
                                "逻辑非 `!` 仅接受 bool",
                                *span,
                            );
                            return None;
                        }
                        let ty = if inner.ty().is_opt() {
                            Type::Opt(Box::new(Type::Bool))
                        } else {
                            Type::Bool
                        };
                        Some(THirExpr::Unary {
                            op: UnaryOp::Not,
                            expr: Box::new(inner),
                            ty,
                            span: *span,
                        })
                    }
                }
            }
            ast::Expr::If {
                condition,
                then,
                else_,
                span,
            } => {
                let cond = self.resolve_with_base(condition, env, Some(Type::Bool), in_join_on)?;
                if !matches!(cond.ty().base(), Type::Bool) {
                    self.diags
                        .error(Code::SemPredicateNotBool, "if 条件必须是 bool/?bool", *span);
                    return None;
                }
                // null 分支经另一分支推导基础类型；两侧均为 null 报 SEM-008（05 §5.2）
                let (then_e, else_e) = if is_null_literal(then) && is_null_literal(else_) {
                    self.diags.error(
                        Code::SemNullMisuse,
                        "if 两个分支均为 `null`，无法确定基础类型",
                        *span,
                    );
                    return None;
                } else if is_null_literal(then) {
                    let else_e = self.resolve_expr(else_, env, in_join_on)?;
                    let then_e = THirExpr::Literal {
                        value: pebble_hir::Literal::Null,
                        ty: Type::Opt(Box::new(else_e.ty().base().clone())),
                        span: then.span(),
                    };
                    (then_e, else_e)
                } else if is_null_literal(else_) {
                    let then_e = self.resolve_expr(then, env, in_join_on)?;
                    let else_e = THirExpr::Literal {
                        value: pebble_hir::Literal::Null,
                        ty: Type::Opt(Box::new(then_e.ty().base().clone())),
                        span: else_.span(),
                    };
                    (then_e, else_e)
                } else {
                    (
                        self.resolve_expr(then, env, in_join_on)?,
                        self.resolve_expr(else_, env, in_join_on)?,
                    )
                };
                // 分支统一（05 §5.2）：A==B → A；T vs ?T → ?T；否则 SEM-007
                let ty = unify_branch_types(then_e.ty(), else_e.ty()).or_else(|| {
                    self.diags.error(
                        Code::SemIfBranchMismatch,
                        format!(
                            "if 分支类型无法统一：{} vs {}",
                            then_e.ty().render(),
                            else_e.ty().render()
                        ),
                        *span,
                    );
                    None
                })?;
                Some(THirExpr::If {
                    condition: Box::new(cond),
                    then: Box::new(then_e),
                    else_: Box::new(else_e),
                    ty,
                    span: *span,
                })
            }
            ast::Expr::Call { name, args, span } => {
                self.resolve_call(name, args, *span, env, in_join_on)
            }
            ast::Expr::Cast { expr, target, span } => {
                // 操作数不给 null 提供基础类型：cast(null as i64) 的操作数类型无法从目标推出来
                let inner = self.resolve_expr(expr, env, in_join_on)?;
                let to = match target {
                    ast::CastTarget::I64 => Type::I64,
                    ast::CastTarget::F64 => Type::F64,
                    ast::CastTarget::Str => Type::Str,
                };
                let from = inner.ty().base().clone();
                // 只有四种转换（04 §4.12）。可能失败的转换，结果一律可空（checked-null）
                let may_fail = match (&from, &to) {
                    (Type::I64, Type::F64) => false,
                    (Type::F64, Type::I64) | (Type::Str, Type::I64) | (Type::Str, Type::F64) => {
                        true
                    }
                    _ => {
                        self.diags.error_hint(
                            Code::SemOpUnsupported,
                            format!("不支持从 {} 转换到 {}", from.render(), to.render()),
                            *span,
                            "只支持四种转换：i64 → f64、f64 → i64、string → i64、string → f64",
                        );
                        return None;
                    }
                };
                let ty = if may_fail || inner.ty().is_opt() {
                    Type::Opt(Box::new(to.clone()))
                } else {
                    to.clone()
                };
                Some(THirExpr::Cast {
                    expr: Box::new(inner),
                    to,
                    ty,
                    span: *span,
                })
            }
        }
    }

    /// 带 null 基类型上下文的解析：null 字面量以 base 的可选形式定型。
    fn resolve_with_base(
        &mut self,
        expr: &ast::Expr,
        env: &Schema,
        base: Option<Type>,
        in_join_on: bool,
    ) -> Option<THirExpr> {
        if is_null_literal(expr) {
            let Some(base) = base else {
                self.diags.error(
                    Code::SemNullMisuse,
                    "独立 `null` 无法确定基础类型（SEM-008）",
                    expr.span(),
                );
                return None;
            };
            return Some(THirExpr::Literal {
                value: pebble_hir::Literal::Null,
                ty: Type::Opt(Box::new(base)),
                span: expr.span(),
            });
        }
        self.resolve_expr(expr, env, in_join_on)
    }

    fn literal(&mut self, lit: &ast::Literal, span: Span) -> Option<THirExpr> {
        let (value, ty) = match lit {
            ast::Literal::Int(v) => (pebble_hir::Literal::Int(*v), Type::I64),
            ast::Literal::Float(v) => {
                if !v.is_finite() {
                    self.diags.error(
                        Code::SemLiteralOutOfRange,
                        "浮点字面量超出有限 f64 值域",
                        span,
                    );
                    return None;
                }
                (pebble_hir::Literal::Float(*v), Type::F64)
            }
            ast::Literal::Str(s) => (pebble_hir::Literal::Str(s.clone()), Type::Str),
            ast::Literal::Bool(b) => (pebble_hir::Literal::Bool(*b), Type::Bool),
            ast::Literal::Null => {
                // 无上下文的 null：SEM-008
                self.diags.error(
                    Code::SemNullMisuse,
                    "独立 `null` 无法确定基础类型（SEM-008）",
                    span,
                );
                return None;
            }
        };
        Some(THirExpr::Literal { value, ty, span })
    }

    fn resolve_binary(
        &mut self,
        op: &ast::BinOp,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
        env: &Schema,
        in_join_on: bool,
    ) -> Option<THirExpr> {
        use Type::*;
        use ast::BinOp::*;

        // 判空特殊形式：x == null / x != null（含 null == x）
        if matches!(op, Eq | Neq) {
            let (l_null, r_null) = (is_null_literal(left), is_null_literal(right));
            if l_null && r_null {
                return Some(THirExpr::Literal {
                    value: pebble_hir::Literal::Bool(*op == Eq),
                    ty: Bool,
                    span,
                });
            }
            if r_null || l_null {
                let (x, x_span) = if r_null {
                    (left, left.span())
                } else {
                    (right, right.span())
                };
                let inner = self.resolve_expr(x, env, in_join_on)?;
                let is_null = THirExpr::Call {
                    name: BuiltinFn::IsNull,
                    args: vec![inner],
                    ty: Bool,
                    span: x_span,
                };
                if *op == Eq {
                    return Some(is_null);
                }
                return Some(THirExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(is_null),
                    ty: Bool,
                    span,
                });
            }
        }

        // And/Or：Kleene，null 侧基类型 Bool
        if matches!(op, And | Or) {
            // 两侧的类型互不依赖：各自解析，两侧的错误都能报出来。
            //（连着写两个 `?` 的话，第一个错误就会让第二个还没检查的表达式被跳过）
            let l = self.resolve_with_base(left, env, Some(Bool), in_join_on);
            let r = self.resolve_with_base(right, env, Some(Bool), in_join_on);
            let (Some(l), Some(r)) = (l, r) else {
                return None;
            };
            for side in [&l, &r] {
                if !matches!(side.ty().base(), Bool) {
                    self.diags.error(
                        Code::SemOpUnsupported,
                        "逻辑运算的操作数必须是 bool/?bool",
                        span,
                    );
                    return None;
                }
            }
            let ty = if l.ty().is_opt() || r.ty().is_opt() {
                Opt(Box::new(Bool))
            } else {
                Bool
            };
            return Some(THirExpr::Binary {
                op: if *op == And { BinOp::And } else { BinOp::Or },
                left: Box::new(l),
                right: Box::new(r),
                ty,
                span,
            });
        }

        // 先解析非 null 侧以确定 null 侧基类型
        let (l, r) = if is_null_literal(left) {
            let r = self.resolve_expr(right, env, in_join_on)?;
            let l = self.resolve_with_base(left, env, Some(r.ty().base().clone()), in_join_on)?;
            (l, r)
        } else if is_null_literal(right) {
            let l = self.resolve_expr(left, env, in_join_on)?;
            let r = self.resolve_with_base(right, env, Some(l.ty().base().clone()), in_join_on)?;
            (l, r)
        } else {
            (
                self.resolve_expr(left, env, in_join_on)?,
                self.resolve_expr(right, env, in_join_on)?,
            )
        };

        let hir_op = match op {
            Add => BinOp::Add,
            Sub => BinOp::Sub,
            Mul => BinOp::Mul,
            Div => BinOp::Div,
            Mod => BinOp::Mod,
            Eq => BinOp::Eq,
            Neq => BinOp::Neq,
            Gt => BinOp::Gt,
            Lt => BinOp::Lt,
            Gte => BinOp::Gte,
            Lte => BinOp::Lte,
            And | Or => unreachable!(),
        };

        let lb = l.ty().base();
        let rb = r.ty().base();
        let either_opt = l.ty().is_opt() || r.ty().is_opt();

        let ty = match (&hir_op, lb, rb) {
            // 算术
            (BinOp::Add, Str, Str) => {
                if either_opt {
                    Opt(Box::new(Str))
                } else {
                    Str
                }
            }
            (BinOp::Add, I64, I64) => Opt(Box::new(I64)),
            (BinOp::Add, _, _) if is_numeric(lb) && is_numeric(rb) => Opt(Box::new(F64)),
            (BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod, I64, I64) => Opt(Box::new(I64)),
            (BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod, _, _)
                if is_numeric(lb) && is_numeric(rb) =>
            {
                Opt(Box::new(F64))
            }
            // 比较：数值×数值 / 字符串×字符串 / 布尔仅相等
            (BinOp::Eq | BinOp::Neq, _, _)
                if (is_numeric(lb) && is_numeric(rb))
                    || (matches!(lb, Str) && matches!(rb, Str))
                    || (matches!(lb, Bool) && matches!(rb, Bool)) =>
            {
                Opt(Box::new(Bool))
            }
            (BinOp::Gt | BinOp::Lt | BinOp::Gte | BinOp::Lte, _, _)
                if (is_numeric(lb) && is_numeric(rb))
                    || (matches!(lb, Str) && matches!(rb, Str)) =>
            {
                Opt(Box::new(Bool))
            }
            _ => {
                let code = if in_join_on {
                    Code::SchJoinKeyMismatch
                } else {
                    Code::SemOpUnsupported
                };
                self.diags.error(
                    code,
                    format!(
                        "运算符 `{}` 不支持 {} 与 {}",
                        op_name(op),
                        l.ty().render(),
                        r.ty().render()
                    ),
                    span,
                );
                return None;
            }
        };

        Some(THirExpr::Binary {
            op: hir_op,
            left: Box::new(l),
            right: Box::new(r),
            ty,
            span,
        })
    }

    fn resolve_call(
        &mut self,
        name: &str,
        args: &[ast::Expr],
        span: Span,
        env: &Schema,
        in_join_on: bool,
    ) -> Option<THirExpr> {
        use BuiltinFn::*;
        let (func, param_bases, result_rule): (BuiltinFn, Vec<Type>, u8) = match name {
            "abs" => (Abs, vec![], 0), // 参数类型分派
            "round" => (Round, vec![Type::F64], 1),
            "lower" => (Lower, vec![Type::Str], 1),
            "upper" => (Upper, vec![Type::Str], 1),
            "trim" => (Trim, vec![Type::Str], 1),
            "length" => (Length, vec![Type::Str], 2),
            "contains" => (Contains, vec![Type::Str, Type::Str], 3),
            "is_null" => (IsNull, vec![], 4),
            _ => {
                self.diags.error_hint(
                    Code::SemOpUnsupported,
                    format!("未知的内置函数 `{}`", name),
                    span,
                    "MVP 白名单: abs round lower upper trim length contains is_null",
                );
                return None;
            }
        };
        // is_null：任意标量参数，恒返回 bool
        if func == IsNull {
            if args.len() != 1 {
                self.diags
                    .error(Code::SemTypeMismatch, "is_null 恰好接收一个参数", span);
                return None;
            }
            if is_null_literal(&args[0]) {
                self.diags
                    .error(Code::SemNullMisuse, "is_null(null) 无法确定基础类型", span);
                return None;
            }
            let inner = self.resolve_expr(&args[0], env, in_join_on)?;
            let ty = Type::Bool;
            return Some(THirExpr::Call {
                name: IsNull,
                args: vec![inner],
                ty,
                span,
            });
        }
        // abs：按参数基础类型分派
        if func == Abs {
            if args.len() != 1 {
                self.diags
                    .error(Code::SemTypeMismatch, "abs 恰好接收一个参数", span);
                return None;
            }
            if is_null_literal(&args[0]) {
                self.diags
                    .error(Code::SemNullMisuse, "abs(null) 无法确定数值基础类型", span);
                return None;
            }
            let inner = self.resolve_expr(&args[0], env, in_join_on)?;
            let ty = match inner.ty().base() {
                Type::I64 => Type::Opt(Box::new(Type::I64)),
                Type::F64 => {
                    if inner.ty().is_opt() {
                        Type::Opt(Box::new(Type::F64))
                    } else {
                        Type::F64
                    }
                }
                _ => {
                    self.diags
                        .error(Code::SemOpUnsupported, "abs 的参数必须是 i64/f64", span);
                    return None;
                }
            };
            return Some(THirExpr::Call {
                name: Abs,
                args: vec![inner],
                ty,
                span,
            });
        }
        // 固定参数签名的严格函数
        if args.len() != param_bases.len() {
            self.diags.error(
                Code::SemTypeMismatch,
                format!("`{}` 需要 {} 个参数", name, param_bases.len()),
                span,
            );
            return None;
        }
        let mut resolved = Vec::new();
        let mut any_opt = false;
        for (arg, base) in args.iter().zip(&param_bases) {
            let r = if is_null_literal(arg) {
                self.resolve_with_base(arg, env, Some(base.clone()), in_join_on)?
            } else {
                self.resolve_expr(arg, env, in_join_on)?
            };
            if !types_unify(r.ty().base(), base) {
                self.diags.error(
                    Code::SemOpUnsupported,
                    format!(
                        "`{}` 的参数类型应为 {}，实际 {}",
                        name,
                        base.render(),
                        r.ty().render()
                    ),
                    arg.span(),
                );
                return None;
            }
            any_opt |= r.ty().is_opt();
            resolved.push(r);
        }
        let ty = match (func, any_opt) {
            (Round, false) => Type::F64,
            (Round, true) => Type::Opt(Box::new(Type::F64)),
            (Lower | Upper | Trim, false) => Type::Str,
            (Lower | Upper | Trim, true) => Type::Opt(Box::new(Type::Str)),
            (Length, false) => Type::I64,
            (Length, true) => Type::Opt(Box::new(Type::I64)),
            (Contains, false) => Type::Bool,
            (Contains, true) => Type::Opt(Box::new(Type::Bool)),
            _ => unreachable!(),
        };
        let _ = result_rule;
        Some(THirExpr::Call {
            name: func,
            args: resolved,
            ty,
            span,
        })
    }

    // ---------- 字段解析 ----------

    fn resolve_field(
        &mut self,
        alias: &Option<String>,
        name: &str,
        span: Span,
        env: &Schema,
    ) -> Option<THirExpr> {
        let found = match alias {
            Some(a) => {
                let Some(&source_id) = self.aliases.get(a) else {
                    self.diags.error_hint(
                        Code::SemUnknownField,
                        format!("来源别名 `{}` 不存在", a),
                        span,
                        format!("可用别名: {}", self.available_aliases()),
                    );
                    return None;
                };
                // 毒化的字段对解析不可见：它是某个失败定义的占位
                let matches: Vec<&FieldSchema> = env
                    .fields
                    .iter()
                    .filter(|f| !self.poisoned.contains(&f.id))
                    .filter(|f| match &f.origin {
                        FieldOrigin::Source {
                            source,
                            source_field,
                        } => *source == source_id && source_field == name,
                        FieldOrigin::Derived { .. } => false,
                    })
                    .collect();
                // 被失败的 extend 覆盖掉的源字段（如 `.extend({ amount = … })` 失败后的 `o.amount`）：
                // 占位是 Derived 来源，上面按「来源 + 字段名」找不到它。它同样是被毒化的名字
                if matches.is_empty()
                    && env
                        .fields
                        .iter()
                        .any(|f| f.name == *name && self.poisoned.contains(&f.id))
                {
                    return None;
                }
                match matches.len() {
                    0 => None,
                    1 => Some(matches[0].clone()),
                    _ => {
                        // 同一源内字段名唯一（lock 保证）；防御式报错
                        self.diags.error(
                            Code::SemAmbiguousField,
                            format!("字段 `{}.{}` 解析到多个身份", a, name),
                            span,
                        );
                        return None;
                    }
                }
            }
            None => {
                // 毒化的字段对解析不可见：它是某个失败定义的占位。
                // 若这个名字的候选**全部**是毒化的，就沉默地停下——
                // 不能落到下面「从未存在 / 超出作用域」的分支去：占位本身就在历史里，会被误判成 SEM-012
                let raw = env.by_name(name);
                if !raw.is_empty() && raw.iter().all(|f| self.poisoned.contains(&f.id)) {
                    return None;
                }
                let matches: Vec<&FieldSchema> = raw
                    .into_iter()
                    .filter(|f| !self.poisoned.contains(&f.id))
                    .collect();
                match matches.len() {
                    0 => None,
                    1 => Some(matches[0].clone()),
                    _ => {
                        let candidates: Vec<String> = matches
                            .iter()
                            .map(|f| match &f.origin {
                                FieldOrigin::Source { source, .. } => format!(
                                    "{}.{}",
                                    self.alias_of(*source).unwrap_or_else(|| "?".into()),
                                    f.name
                                ),
                                FieldOrigin::Derived { .. } => f.name.clone(),
                            })
                            .collect();
                        self.diags.error_hint(
                            Code::SemAmbiguousField,
                            format!("裸字段 `{}` 存在歧义", name),
                            span,
                            format!("可指 {}；请用别名限定", candidates.join(" 或 ")),
                        );
                        return None;
                    }
                }
            }
        };
        match found {
            Some(f) => Some(THirExpr::Field {
                field: f.id,
                ty: f.ty.clone(),
                span,
            }),
            None => {
                // 区分 SEM-001（从未存在）与 SEM-012（超出作用域）
                let ever = self.history.iter().any(|s| {
                    let by_alias = alias
                        .as_ref()
                        .map(|a| {
                            self.aliases
                                .get(a)
                                .map(|&sid| {
                                    s.fields.iter().any(|f| {
                                        matches!(&f.origin,
                                        FieldOrigin::Source { source, source_field }
                                            if *source == sid && source_field == name)
                                    })
                                })
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let by_bare = alias.is_none() && s.by_name(name).len() == 1;
                    by_alias || by_bare
                });
                if ever {
                    self.diags.error_hint(
                        Code::SemOutOfScope,
                        format!(
                            "字段 `{}` 超出当前作用域（SEM-012）",
                            display_ref(alias, name)
                        ),
                        span,
                        format!("当前可见字段: {}", self.visible_fields(env)),
                    );
                } else {
                    self.diags.error_hint(
                        Code::SemUnknownField,
                        format!("字段 `{}` 不存在于当前解析域", display_ref(alias, name)),
                        span,
                        format!("当前可见字段: {}", self.visible_fields(env)),
                    );
                }
                None
            }
        }
    }

    // ---------- 辅助 ----------

    fn check_predicate(&mut self, e: &THirExpr) -> Option<()> {
        if !matches!(e.ty().base(), Type::Bool) {
            self.diags.error(
                Code::SemPredicateNotBool,
                format!("谓词必须是 bool/?bool，实际为 {}", e.ty().render()),
                e.span(),
            );
            return None;
        }
        Some(())
    }

    /// 流敏非空收缩：谓词为 true 时必然非空的字段收窄为 T（仅作用于 filter 输出）。
    fn narrow(&self, schema: &Schema, pred: &THirExpr) -> Schema {
        let nn = nn_set(pred);
        if nn.is_empty() {
            return schema.clone();
        }
        let mut fields = schema.fields.clone();
        for f in &mut fields {
            if nn.contains(&f.id) {
                if let Type::Opt(inner) = f.ty.clone() {
                    f.ty = *inner;
                }
            }
        }
        Schema { fields }
    }

    fn available_aliases(&self) -> String {
        if self.aliases.is_empty() {
            "（无）".into()
        } else {
            self.aliases.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    }

    fn alias_of(&self, source: SourceId) -> Option<String> {
        self.aliases
            .iter()
            .find(|(_, sid)| **sid == source)
            .map(|(k, _)| k.clone())
    }

    /// 诊断提示里的「当前可见字段」。毒化的占位不算可见——它的定义失败了，
    /// 把它当成可用字段推荐给用户是误导。
    fn visible_fields(&self, env: &Schema) -> String {
        env.fields
            .iter()
            .filter(|f| !self.poisoned.contains(&f.id))
            .map(|f| f.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn fresh_field(&mut self) -> FieldId {
        let id = self.next_field;
        self.next_field += 1;
        id
    }

    /// 一个定义失败的名字：在输出 Schema 里占住它的位置，并记入毒化表。
    ///
    /// 为什么要占位，而不是干脆不放？因为下游很可能引用它（`.extend({ x = … }).filter(x > 0)`）。
    /// 占位让这个名字「存在但坏了」：引用它的表达式在 `resolve_field` 里沉默地停下，
    /// 而不是报一条「字段 x 不存在」——那只是前一个错误的后果。
    /// 类型只是填空，永远不会被读取：碰到毒化的表达式在类型检查之前就停了。
    fn poison_field(&mut self, name: &str, node: NodeId) -> FieldSchema {
        let id = self.fresh_field();
        self.poisoned.insert(id);
        FieldSchema {
            id,
            name: name.to_string(),
            ty: Type::Opt(Box::new(Type::I64)),
            origin: FieldOrigin::Derived {
                node,
                binding: name.to_string(),
            },
        }
    }

    fn push_node(
        &mut self,
        inputs: Vec<NodeId>,
        output: Schema,
        span: Span,
        build: impl FnOnce(NodeBase) -> HirNode,
    ) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let base = NodeBase {
            id,
            inputs,
            output,
            span,
        };
        self.nodes.push(build(base));
        id
    }

    /// 编译期常量定义域检查（SEM-016）。
    /// 仅当整个表达式为全常量且求值必然定义域失败时报错；
    /// 非常量子表达式（如 `x + 1/0`）按运行时 checked-null 返回 null，不诊断。
    fn const_domain_check(&mut self, e: &THirExpr) {
        if self.const_failed || !e.is_const() {
            return;
        }
        if let Err(span) = const_val(e) {
            self.const_failed = true;
            self.diags.error_hint(
                Code::SemConstDomain,
                "编译期可确定的定义域失败（除零/模零/整数溢出/非有限 f64 结果/必然失败的 cast）",
                span,
                "该表达式是常量且必然失败；请修正常量或改用运行时数据",
            );
        }
    }
}

// ---------- NN(p)：非空保证分析（05 §5.3） ----------

fn nn_set(e: &THirExpr) -> BTreeSet<FieldId> {
    let mut out = BTreeSet::new();
    nn_walk(e, &mut out);
    out
}

fn nn_walk(e: &THirExpr, out: &mut BTreeSet<FieldId>) {
    match e {
        // !is_null(f) / f != null（lowering 后同形）
        THirExpr::Unary {
            op: UnaryOp::Not,
            expr,
            ..
        } => {
            if let THirExpr::Call {
                name: BuiltinFn::IsNull,
                args,
                ..
            } = expr.as_ref()
            {
                if let Some(THirExpr::Field { field, .. }) = args.first() {
                    out.insert(*field);
                }
            }
        }
        THirExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            let mut l = BTreeSet::new();
            let mut r = BTreeSet::new();
            nn_walk(left, &mut l);
            nn_walk(right, &mut r);
            out.extend(l.intersection(&r).cloned().collect::<BTreeSet<_>>());
            out.extend(l);
            out.extend(r);
        }
        THirExpr::Binary {
            op: BinOp::Or,
            left,
            right,
            ..
        } => {
            let mut l = BTreeSet::new();
            let mut r = BTreeSet::new();
            nn_walk(left, &mut l);
            nn_walk(right, &mut r);
            out.extend(l.intersection(&r).cloned().collect::<BTreeSet<_>>());
        }
        THirExpr::If {
            condition,
            then,
            else_,
            ..
        } => {
            // then 为 true 的路径：cond 非 null 保证 ∪ then 保证
            let mut cond_nn = BTreeSet::new();
            nn_walk(condition, &mut cond_nn);
            let mut then_nn = BTreeSet::new();
            nn_walk(then, &mut then_nn);
            let then_path: BTreeSet<FieldId> = cond_nn.union(&then_nn).cloned().collect();
            // else 为 true 的路径：cond 为 false/null 时，仅当 cond 是 is_null(f) 才有 {f} 保证
            let mut else_nn = BTreeSet::new();
            nn_walk(else_, &mut else_nn);
            let mut else_path = else_nn;
            if let THirExpr::Call {
                name: BuiltinFn::IsNull,
                args,
                ..
            } = condition.as_ref()
            {
                if let Some(THirExpr::Field { field, .. }) = args.first() {
                    else_path.insert(*field);
                }
            }
            out.extend(
                then_path
                    .intersection(&else_path)
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            );
        }
        _ => {}
    }
}

// ---------- 常量定义域求值 ----------

#[derive(Clone, Debug)]
enum ConstVal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

/// 混合数值运算的提升规则：一侧是 f64、另一侧是 i64 时，把 i64 按 round-to-nearest, ties-to-even
/// 转成 f64（与 `cast::i64_to_f64` 同一条规则）。
fn promote_mixed(l: &ConstVal, r: &ConstVal) -> (ConstVal, ConstVal) {
    match (l, r) {
        (ConstVal::Float(_), ConstVal::Int(b)) => {
            (l.clone(), ConstVal::Float(cast::i64_to_f64(*b)))
        }
        (ConstVal::Int(a), ConstVal::Float(_)) => {
            (ConstVal::Float(cast::i64_to_f64(*a)), r.clone())
        }
        _ => (l.clone(), r.clone()),
    }
}

/// 求值全常量表达式；Err(span) 表示必然的定义域失败（→ SEM-016）。
fn const_val(e: &THirExpr) -> Result<ConstVal, Span> {
    match e {
        THirExpr::Literal { value, span, .. } => match value {
            pebble_hir::Literal::Int(v) => Ok(ConstVal::Int(*v)),
            pebble_hir::Literal::Float(v) => {
                if !v.is_finite() {
                    Err(*span)
                } else {
                    Ok(ConstVal::Float(*v))
                }
            }
            pebble_hir::Literal::Str(s) => Ok(ConstVal::Str(s.clone())),
            pebble_hir::Literal::Bool(b) => Ok(ConstVal::Bool(*b)),
            pebble_hir::Literal::Null => Ok(ConstVal::Null),
        },
        THirExpr::Field { span, .. } => Err(*span),
        THirExpr::Binary {
            op,
            left,
            right,
            span,
            ..
        } => {
            let l = const_val(left)?;
            let r = const_val(right)?;
            // 混合数值运算先提升到 f64（05 §5.2）。`1.5 / 0` 里的 `0` 是 i64 字面量：
            // 不提升的话它既不匹配「f64 除以 0.0」也不匹配「i64 除以 0」，这个编译期就能
            // 确定的除零就被放过去了
            let (l, r) = promote_mixed(&l, &r);
            Ok(match (op, l, r) {
                (BinOp::Add, ConstVal::Int(a), ConstVal::Int(b)) => match a.checked_add(b) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (BinOp::Sub, ConstVal::Int(a), ConstVal::Int(b)) => match a.checked_sub(b) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (BinOp::Mul, ConstVal::Int(a), ConstVal::Int(b)) => match a.checked_mul(b) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (BinOp::Div, ConstVal::Int(a), ConstVal::Int(b)) => match a.checked_div(b) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (BinOp::Mod, ConstVal::Int(a), ConstVal::Int(b)) => match a.checked_rem(b) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (BinOp::Div | BinOp::Mod, ConstVal::Float(_), ConstVal::Float(0.0)) => {
                    return Err(*span);
                }
                (BinOp::Add | BinOp::Sub | BinOp::Mul, ConstVal::Float(a), ConstVal::Float(b)) => {
                    let v = match op {
                        BinOp::Add => a + b,
                        BinOp::Sub => a - b,
                        _ => a * b,
                    };
                    if !v.is_finite() {
                        return Err(*span);
                    }
                    ConstVal::Float(v)
                }
                (BinOp::Div, ConstVal::Float(a), ConstVal::Float(b)) => {
                    let v = a / b;
                    if !v.is_finite() {
                        return Err(*span);
                    }
                    ConstVal::Float(v)
                }
                (BinOp::Eq, ConstVal::Null, _) | (BinOp::Eq, _, ConstVal::Null) => {
                    ConstVal::Bool(false)
                }
                (BinOp::Eq, ConstVal::Int(a), ConstVal::Int(b)) => ConstVal::Bool(a == b),
                (BinOp::Eq, ConstVal::Float(a), ConstVal::Float(b)) => ConstVal::Bool(a == b),
                (BinOp::Eq, ConstVal::Str(a), ConstVal::Str(b)) => ConstVal::Bool(a == b),
                (BinOp::Eq, ConstVal::Bool(a), ConstVal::Bool(b)) => ConstVal::Bool(a == b),
                (BinOp::And, ConstVal::Bool(false), _) => ConstVal::Bool(false),
                (BinOp::And, _, ConstVal::Bool(false)) => ConstVal::Bool(false),
                (BinOp::And, ConstVal::Bool(true), ConstVal::Bool(true)) => ConstVal::Bool(true),
                (BinOp::Or, ConstVal::Bool(true), _) => ConstVal::Bool(true),
                (BinOp::Or, _, ConstVal::Bool(true)) => ConstVal::Bool(true),
                (BinOp::Or, ConstVal::Bool(false), ConstVal::Bool(false)) => ConstVal::Bool(false),
                _ => ConstVal::Null,
            })
        }
        THirExpr::Unary { op, expr, span, .. } => {
            let v = const_val(expr)?;
            Ok(match (op, v) {
                (UnaryOp::Negate, ConstVal::Int(i64::MIN)) => return Err(*span),
                (UnaryOp::Negate, ConstVal::Int(a)) => match a.checked_neg() {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (UnaryOp::Negate, ConstVal::Float(a)) => ConstVal::Float(-a),
                (UnaryOp::Not, ConstVal::Bool(b)) => ConstVal::Bool(!b),
                _ => ConstVal::Null,
            })
        }
        THirExpr::If {
            condition,
            then,
            else_,
            ..
        } => match const_val(condition) {
            Ok(ConstVal::Bool(true)) => const_val(then),
            Ok(ConstVal::Bool(false)) | Ok(ConstVal::Null) => const_val(else_),
            _ => Ok(ConstVal::Null),
        },
        // 内置函数按规范（04 §4.8）在常量上求值。这里曾经是 `Err(*span)`——把「我不求值」
        // 当成了「必然失败」，于是 `round(1.5)`、`lower("ABC")` 这类合法的常量表达式全被误报成 SEM-016
        THirExpr::Call {
            name, args, span, ..
        } => {
            use pebble_hir::BuiltinFn::*;
            let vals = args.iter().map(const_val).collect::<Result<Vec<_>, _>>()?;
            // 严格传播：任一参数为 null，结果为 null（is_null 除外）
            if *name != IsNull && vals.iter().any(|v| matches!(v, ConstVal::Null)) {
                return Ok(ConstVal::Null);
            }
            Ok(match (name, vals.as_slice()) {
                (IsNull, [v]) => ConstVal::Bool(matches!(v, ConstVal::Null)),
                (Abs, [ConstVal::Int(a)]) => match a.checked_abs() {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span), // abs(i64::MIN) 为 null
                },
                (Abs, [ConstVal::Float(a)]) => ConstVal::Float(a.abs()),
                (Round, [ConstVal::Float(a)]) => {
                    ConstVal::Float(cast::norm_zero(a.round_ties_even()))
                }
                (Lower, [ConstVal::Str(s)]) => ConstVal::Str(s.to_lowercase()),
                (Upper, [ConstVal::Str(s)]) => ConstVal::Str(s.to_uppercase()),
                (Trim, [ConstVal::Str(s)]) => ConstVal::Str(s.trim().to_string()),
                (Length, [ConstVal::Str(s)]) => ConstVal::Int(s.chars().count() as i64),
                (Contains, [ConstVal::Str(a), ConstVal::Str(b)]) => {
                    ConstVal::Bool(a.contains(b.as_str()))
                }
                _ => ConstVal::Null,
            })
        }
        // 受限 cast（07 §7.8）。Err 表示「这个常量 cast 必然失败」→ SEM-016
        THirExpr::Cast { expr, to, span, .. } => {
            let v = const_val(expr)?;
            Ok(match (v, to) {
                (ConstVal::Null, _) => ConstVal::Null,
                (ConstVal::Int(a), Type::F64) => ConstVal::Float(a as f64),
                (ConstVal::Float(x), Type::I64) => match cast::f64_to_i64(x) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (ConstVal::Str(s), Type::I64) => match cast::parse_i64_strict(s.as_str()) {
                    Some(v) => ConstVal::Int(v),
                    None => return Err(*span),
                },
                (ConstVal::Str(s), Type::F64) => match cast::parse_f64_strict(s.as_str()) {
                    Some(v) => ConstVal::Float(v),
                    None => return Err(*span),
                },
                _ => ConstVal::Null,
            })
        }
    }
}

/// cast 取值规则的**参考实现**（07 §7.8）。
///
/// 常量诊断（SEM-016）用它；测试拿它和真实 DuckDB 上生成的 SQL 对拍。规则写在规范里，
/// 这里和后端各自实现一遍——规则变了，两边一起改，对拍会告诉你漏了哪一边。
pub mod cast {
    /// `i64 → f64`：取最接近的 f64（round-to-nearest, ties-to-even），永不失败。
    pub fn i64_to_f64(a: i64) -> f64 {
        norm_zero(a as f64)
    }

    /// `f64 → i64`：先按 round-to-nearest, ties-to-even 取整，再看是否落在 i64 值域内。
    pub fn f64_to_i64(x: f64) -> Option<i64> {
        if !x.is_finite() {
            return None;
        }
        let r = x.round_ties_even();
        // 2^63 在 f64 里恰好可表示；落在 [-2^63, 2^63) 里即在 i64 值域内
        if (-9223372036854775808.0..9223372036854775808.0).contains(&r) {
            Some(r as i64)
        } else {
            None
        }
    }

    /// `string → i64`：只接受 `[+-]?[0-9]+`（ASCII）。
    pub fn parse_i64_strict(s: &str) -> Option<i64> {
        let body = s.strip_prefix(['+', '-']).unwrap_or(s);
        if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<i64>().ok()
    }

    /// `string → f64`：只接受 ASCII 十进制或科学记数法，结果必须有限。
    /// 文法：`[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?`
    pub fn parse_f64_strict(s: &str) -> Option<f64> {
        let bytes = s.as_bytes();
        let mut i = 0;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let digits = |i: &mut usize| {
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            *i > start
        };
        let int_digits = digits(&mut i);
        let mut frac_digits = false;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            frac_digits = digits(&mut i);
        }
        // 小数点前后至少一边有数字
        if !int_digits && !frac_digits {
            return None;
        }
        if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
            i += 1;
            if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                i += 1;
            }
            if !digits(&mut i) {
                return None;
            }
        }
        if i != bytes.len() {
            return None;
        }
        let v = s.parse::<f64>().ok()?;
        if v.is_finite() {
            Some(norm_zero(v))
        } else {
            None
        }
    }

    /// 把零规范化为 +0.0（05 §5.1：±0 表示同一个 Pebble 数值）。
    pub fn norm_zero(v: f64) -> f64 {
        if v == 0.0 { 0.0 } else { v }
    }
}

// ---------- 小工具 ----------

fn is_null_literal(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Literal(ast::Literal::Null, _))
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::I64 | Type::F64)
}

fn types_unify(actual: &Type, expected: &Type) -> bool {
    actual == expected
}

fn unify_branch_types(a: &Type, b: &Type) -> Option<Type> {
    if a == b {
        return Some(a.clone());
    }
    // 一侧 null 字面量（Opt(T)）另一侧 T → ?T
    match (a, b) {
        (Type::Opt(inner), other) | (other, Type::Opt(inner)) => {
            if other == inner.as_ref() {
                Some(Type::Opt(inner.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn op_name(op: &ast::BinOp) -> &'static str {
    use ast::BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Neq => "!=",
        Gt => ">",
        Lt => "<",
        Gte => ">=",
        Lte => "<=",
        And => "&&",
        Or => "||",
    }
}

fn func_name(f: &AggFunc) -> &'static str {
    match f {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
        AggFunc::Avg => "avg",
    }
}

fn to_hir_agg(f: &ast::AggFunc) -> AggFunc {
    match f {
        ast::AggFunc::Count => AggFunc::Count,
        ast::AggFunc::Sum => AggFunc::Sum,
        ast::AggFunc::Min => AggFunc::Min,
        ast::AggFunc::Max => AggFunc::Max,
        ast::AggFunc::Avg => AggFunc::Avg,
    }
}

fn display_ref(alias: &Option<String>, name: &str) -> String {
    match alias {
        Some(a) => format!("{}.{}", a, name),
        None => name.to_string(),
    }
}

pub use canonical::{canonical_bytes, hir_hash};

#[cfg(test)]
mod tests {
    use super::*;
    use pebble_hir::{LockField, LockSource, LockType};

    fn test_catalog() -> Catalog {
        let orders = SchemaLock::build(
            LockSource::MemoryTable {
                name: "orders".into(),
                data_file: Some("orders.csv".into()),
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
        );
        let scores = SchemaLock::build(
            LockSource::Table {
                locator: "scores.parquet".into(),
            },
            vec![
                LockField {
                    name: "id".into(),
                    ty: LockType::I64,
                    optional: false,
                },
                LockField {
                    name: "score".into(),
                    ty: LockType::F64,
                    optional: true,
                },
            ],
        );
        Catalog::from_locks(vec![orders, scores])
    }

    fn compile_ok(src: &str) -> Hir {
        let mut diags = DiagBag::new();
        let program = pebble_parser::parse(src, &mut diags).expect("parse");
        let catalog = test_catalog();
        compile(&program, &catalog, &mut diags)
            .unwrap_or_else(|| panic!("diags: {:#?}", diags.diags))
    }

    fn compile_err(src: &str, code: &str) -> DiagBag {
        let mut diags = DiagBag::new();
        let catalog = test_catalog();
        if let Ok(program) = pebble_parser::parse(src, &mut diags) {
            assert!(compile(&program, &catalog, &mut diags).is_none());
        }
        assert!(
            diags.diags.iter().any(|d| d.code.as_str() == code),
            "expected {}, got {:?} in {}",
            code,
            diags
                .diags
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>(),
            src
        );
        diags
    }

    const M0: &str = r#"
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

    #[test]
    fn lowers_m0_and_hashes_deterministically() {
        let h1 = compile_ok(M0);
        let h2 = compile_ok(M0);
        assert_eq!(hir_hash(&h1), hir_hash(&h2));
        assert!(h1.validate().is_ok());
        // 节点数：源 + filter + aggregate + filter + select + sort + take + collect = 8
        assert_eq!(h1.nodes.len(), 8);
    }

    #[test]
    fn narrowing_shrinks_nullable_after_null_guard() {
        let hir = compile_ok(
            r#"
Table("scores.parquet") as s
  .filter(s.score != null)
  .collect()
"#,
        );
        let filter = &hir.nodes[1];
        let score = filter
            .base()
            .output
            .fields
            .iter()
            .find(|f| f.name == "score")
            .expect("score 字段");
        assert!(
            matches!(score.ty, Type::F64),
            "filter 后 score 应收缩为非空 f64"
        );
    }

    #[test]
    fn agg_signatures_follow_context() {
        let hir = compile_ok(
            r#"
MemoryTable("orders") as o
  .group_by(o.status)
  .agg(
    c = count(),
    s = sum(o.amount),
    mn = min(o.amount),
  )
  .collect()
"#,
        );
        let agg = &hir.nodes[1];
        let out = &agg.base().output;
        let get = |n: &str| out.fields.iter().find(|f| f.name == n).unwrap().ty.clone();
        assert!(matches!(get("c"), Type::I64));
        assert!(matches!(get("s"), Type::Opt(_)), "sum 恒可空（空组/溢出）");
        // min：分组聚合但参数可空 → ?i64
        assert!(matches!(get("mn"), Type::Opt(_)));
    }

    #[test]
    fn global_agg_is_single_row_node() {
        let hir = compile_ok(
            r#"
MemoryTable("orders") as o
  .agg(total = sum(o.amount),)
  .collect()
"#,
        );
        match &hir.nodes[1] {
            HirNode::Aggregate { group_keys, .. } => assert!(group_keys.is_empty()),
            other => panic!("期望聚合节点，实际 {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn semantic_error_table() {
        // SEM-001 未知字段
        compile_err(
            "MemoryTable(\"orders\") as o\n  .filter(o.statusx == \"paid\")\n  .collect()\n",
            "SEM-001",
        );
        // SEM-010 谓词非 bool
        compile_err(
            "MemoryTable(\"orders\") as o\n  .filter(o.status)\n  .collect()\n",
            "SEM-010",
        );
        // SEM-016 常量除零（全常量表达式必然失败）
        compile_err(
            "MemoryTable(\"orders\") as o\n  .filter(1 / 0 > 2)\n  .collect()\n",
            "SEM-016",
        );
        // SEM-005 group_by 后未接 agg
        compile_err(
            "MemoryTable(\"orders\") as o\n  .group_by(o.status)\n  .collect()\n",
            "SEM-005",
        );
        // SEM-005 缺终端
        compile_err(
            "MemoryTable(\"orders\") as o\n  .filter(o.status == \"paid\")\n",
            "SEM-005",
        );
        // SEM-014 重复绑定
        compile_err(
            "MemoryTable(\"orders\") as o\n  .extend({ x = 1, x = 2, })\n  .collect()\n",
            "SEM-014",
        );
        // SEM-012 agg 后引用输入行字段
        compile_err(
            "MemoryTable(\"orders\") as o\n  .group_by(o.status)\n  .agg(c = count(),)\n  .filter(o.amount > 0)\n  .collect()\n",
            "SEM-012",
        );
        // SEM-003 sum 非数值
        compile_err(
            "MemoryTable(\"orders\") as o\n  .agg(s = sum(o.status),)\n  .collect()\n",
            "SEM-003",
        );
        // SEM-008 独立 null
        compile_err(
            "MemoryTable(\"orders\") as o\n  .extend({ x = null })\n  .collect()\n",
            "SEM-008",
        );
        // SEM-004 别名重复
        compile_err(
            "MemoryTable(\"orders\") as o\n  .join(MemoryTable(\"orders\") as o, on = o.status == o.status)\n  .collect()\n",
            "SEM-004",
        );
    }

    #[test]
    fn ambiguity_and_join_keys() {
        // SEM-011 join 后同名字段裸引用歧义
        let cat = Catalog::from_locks(vec![
            SchemaLock::build(
                LockSource::MemoryTable {
                    name: "a".into(),
                    data_file: None,
                },
                vec![LockField {
                    name: "id".into(),
                    ty: LockType::I64,
                    optional: false,
                }],
            ),
            SchemaLock::build(
                LockSource::MemoryTable {
                    name: "b".into(),
                    data_file: None,
                },
                vec![LockField {
                    name: "id".into(),
                    ty: LockType::I64,
                    optional: false,
                }],
            ),
        ]);
        let src = "MemoryTable(\"a\") as x\n  .join(MemoryTable(\"b\") as y, on = x.id == y.id)\n  .filter(id > 0)\n  .collect()\n";
        let mut diags = DiagBag::new();
        let program = pebble_parser::parse(src, &mut diags).expect("parse");
        assert!(compile(&program, &cat, &mut diags).is_none());
        assert!(diags.diags.iter().any(|d| d.code.as_str() == "SEM-011"));

        // SCH-002 join 键类型不一致（i64 vs string）
        let cat2 = Catalog::from_locks(vec![
            SchemaLock::build(
                LockSource::MemoryTable {
                    name: "a".into(),
                    data_file: None,
                },
                vec![LockField {
                    name: "id".into(),
                    ty: LockType::I64,
                    optional: false,
                }],
            ),
            SchemaLock::build(
                LockSource::MemoryTable {
                    name: "b".into(),
                    data_file: None,
                },
                vec![LockField {
                    name: "id".into(),
                    ty: LockType::Str,
                    optional: false,
                }],
            ),
        ]);
        let src2 = "MemoryTable(\"a\") as x\n  .join(MemoryTable(\"b\") as y, on = x.id == y.id)\n  .collect()\n";
        let mut d2 = DiagBag::new();
        let p2 = pebble_parser::parse(src2, &mut d2).expect("parse");
        assert!(compile(&p2, &cat2, &mut d2).is_none());
        assert!(d2.diags.iter().any(|d| d.code.as_str() == "SCH-002"));
    }

    #[test]
    fn sch001_missing_lock() {
        compile_err(
            "MemoryTable(\"nonexistent\") as o\n  .collect()\n",
            "SCH-001",
        );
    }

    #[test]
    fn sch004_group_key_rename_collision() {
        compile_err(
            "MemoryTable(\"orders\") as o\n  .group_by(o.status, o.status)\n  .agg(c = count(),)\n  .collect()\n",
            "SCH-004",
        );
    }

    #[test]
    fn sem007_if_branch_mismatch() {
        compile_err(
            "MemoryTable(\"orders\") as o\n  .extend({ x = if o.amount > 0 { 1 } else { \"a\" } })\n  .collect()\n",
            "SEM-007",
        );
    }

    #[test]
    fn format_comment_only_changes_do_not_change_hash() {
        let a = compile_ok(M0);
        let commented = M0.replace(".filter(", "// 审批要点：过滤已支付订单\n  .filter(");
        let b = compile_ok(&commented);
        assert_eq!(
            hir_hash(&a),
            hir_hash(&b),
            "注释/空白不得影响 canonical hash"
        );
    }

    #[test]
    fn semantic_change_changes_hash() {
        let a = compile_ok(M0);
        let changed = M0.replace(">= 10000", ">= 9999");
        let b = compile_ok(&changed);
        assert_ne!(hir_hash(&a), hir_hash(&b));
    }

    #[test]
    fn non_ascii_comments_change_spans_but_not_the_hash() {
        // Span 改用 UTF-8 字节偏移后（第 8 章），中文注释会改变其后所有 Span 的数值。
        // Span 只服务诊断、不进入 canonical 序列化——所以哈希必须纹丝不动。
        let a = compile_ok(M0);
        let commented = format!("// 审批要点：每个汉字占 3 字节\n// 第二行中文注释\n{M0}");
        let b = compile_ok(&commented);
        assert_eq!(hir_hash(&a), hir_hash(&b));
    }

    #[test]
    fn semantic_error_is_located_after_chinese_comments() {
        let src = "// 审批要点\nMemoryTable(\"orders\") as o\n  .filter(o.statusx == \"paid\")\n  .collect()\n";
        let diags = compile_err(src, "SEM-001");
        let text = pebble_diagnostic::render(src, "demo.pebble", &diags.diags[0]);
        // 第 3 行第 11 列：整个字段引用 `o.statusx`（9 个字符）的起点
        assert!(text.contains("demo.pebble:3:11"), "{text}");
        assert!(
            text.contains(&format!("{} SEM-001", "^".repeat(9))),
            "{text}"
        );
    }

    // ---------- 第 9 章：语义阶段的多错误报告与毒化 ----------
    //
    // 与解析器那边同理：每个用例断言的是**完整的**错误列表——一个根因只报一次，独立的错误一个不漏。

    /// 全部诊断的错误码，按源码位置排序。
    fn codes(src: &str) -> Vec<String> {
        let mut diags = DiagBag::new();
        let catalog = test_catalog();
        let program = pebble_parser::parse(src, &mut diags).expect("测试源码应当语法正确");
        assert!(
            compile(&program, &catalog, &mut diags).is_none(),
            "有错误时不得产出 HIR"
        );
        diags.normalize();
        diags
            .diags
            .iter()
            .map(|d| d.code.as_str().to_string())
            .collect()
    }

    #[test]
    fn independent_errors_in_different_ops_are_all_reported() {
        // 以前：只报第一个，第二个要等改完第一个、重新编译才看得见
        let src = "MemoryTable(\"orders\") as o\n  .filter(o.typo_a > 1)\n  .filter(o.typo_b > 2)\n  .collect()\n";
        assert_eq!(codes(src), ["SEM-001", "SEM-001"]);
        // 不同种类的错误混在一起也一样
        let src = "MemoryTable(\"orders\") as o\n  .filter(o.amount)\n  .filter(o.typo > 2)\n  .collect()\n";
        assert_eq!(codes(src), ["SEM-010", "SEM-001"]);
        // 同一个谓词的两侧：`&&` 两边各自解析
        let src =
            "MemoryTable(\"orders\") as o\n  .filter(o.typo_a > 1 && o.typo_b < 2)\n  .collect()\n";
        assert_eq!(codes(src), ["SEM-001", "SEM-001"]);
    }

    #[test]
    fn bindings_in_one_extend_are_checked_independently() {
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .extend({\n",
            "    a = o.typo_a,\n",
            "    b = o.typo_b,\n",
            "    c = o.amount + 1,\n",
            "  })\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001", "SEM-001"]);
    }

    #[test]
    fn a_failed_name_is_poisoned_not_reported_twice() {
        // x 的定义失败；后面两处对 x 的使用都是它的后果，不该再报
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .extend({ x = o.typo })\n",
            "  .filter(x > 0)\n",
            "  .extend({ y = x + 1 })\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001"]);
    }

    #[test]
    fn poison_does_not_hide_unrelated_errors_downstream() {
        // x 毒化了，但 o.nope 是另一个独立的错误（在别的元素里）
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .extend({ x = o.typo })\n",
            "  .filter(x > 0)\n",
            "  .extend({ y = 2, z = o.nope })\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001", "SEM-001"]);
    }

    #[test]
    fn override_of_a_real_field_is_poisoned_when_it_fails() {
        // 覆盖失败：名字 amount 现在指向失败的新定义。
        // 别名限定的 `o.amount` 与裸 `amount` 都必须沉默——它们是对同一个失败定义的引用
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .extend({ amount = o.typo })\n",
            "  .filter(o.amount > 0)\n",
            "  .select(amount)\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001"]);
    }

    #[test]
    fn select_items_are_checked_independently() {
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .select(o.typo_a, status, o.typo_b as t)\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001", "SEM-001"]);
    }

    #[test]
    fn a_broken_select_still_cuts_the_scope() {
        // select 有一项坏了，但它裁剪作用域的语义不变：后面用到被裁掉的字段仍是 SEM-012
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .select(o.typo_a, status)\n",
            "  .filter(o.amount > 0)\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001", "SEM-012"]);
    }

    #[test]
    fn agg_keys_and_bindings_are_checked_independently() {
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .group_by(o.typo_key)\n",
            "  .agg(\n",
            "    a = sum(o.typo_a),\n",
            "    b = max(status),\n",
            "    c = avg(status),\n",
            "    d = count(),\n",
            "  )\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-001", "SEM-001", "SEM-003"]);
        // 聚合之后再引用输入行字段：与上面的错误无关，照常报出
        let src = src.replace("  .collect()", "  .filter(o.amount > 0)\n  .collect()");
        assert_eq!(codes(&src), ["SEM-001", "SEM-001", "SEM-003", "SEM-012"]);
    }

    #[test]
    fn group_by_without_agg_does_not_hide_later_errors() {
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .group_by(o.customer_id)\n",
            "  .sort(o.typo_a, o.typo_b)\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-005", "SEM-001", "SEM-001"]);
    }

    #[test]
    fn duplicate_names_are_reported_and_the_rest_is_still_checked() {
        let src = concat!(
            "MemoryTable(\"orders\") as o\n",
            "  .extend({\n",
            "    x = 1,\n",
            "    x = o.typo,\n",
            "    y = o.typo2,\n",
            "  })\n",
            "  .collect()\n",
        );
        assert_eq!(codes(src), ["SEM-014", "SEM-001"]);
    }

    #[test]
    fn missing_source_lock_is_fatal_for_the_semantic_phase() {
        // 表的形状未知：后面的检查无从谈起，只报这一条
        let src = "MemoryTable(\"nope\") as o\n  .filter(o.typo > 1)\n  .collect()\n";
        assert_eq!(codes(src), ["SCH-001"]);
    }

    #[test]
    fn valid_programs_are_unaffected_by_the_recovery_machinery() {
        let hir = compile_ok(M0);
        assert!(hir.validate().is_ok());
    }

    // ---------- 第 11 章：having 与 cast ----------

    /// 流水线最后一级的输出字段：(名字, 类型)。`base` 是接在源后面的那一段算子。
    fn output_types(src: &str) -> Vec<(String, String)> {
        let hir = compile_ok(src);
        let last = hir.nodes[hir.root as usize].base().inputs[0];
        hir.nodes[last as usize]
            .base()
            .output
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.render()))
            .collect()
    }

    fn with_orders(expr: &str) -> String {
        format!("MemoryTable(\"orders\") as o\n  .extend({{ x = {expr} }})\n  .collect()\n")
    }

    #[test]
    fn cast_reference_implementation_follows_the_spec() {
        use cast::*;
        // i64 → f64：取最接近的 f64，永不失败
        assert!(i64_to_f64(0).is_sign_positive(), "0 规范化为 +0.0");
        assert_eq!(
            i64_to_f64(9007199254740993),
            9007199254740992.0,
            "2^53 + 1 取偶到 2^53"
        );
        assert_eq!(i64_to_f64(i64::MIN), -9223372036854775808.0);
        assert_eq!(
            i64_to_f64(i64::MAX),
            9223372036854775808.0,
            "i64::MAX 取最接近的 2^63"
        );

        // f64 → i64：先按取偶取整，再查值域
        for (x, want) in [
            (2.5, 2),
            (-2.5, -2),
            (0.5, 0),
            (-0.5, 0),
            (1.5, 2),
            (-1.5, -2),
            (0.49999999999999994, 0),
            (-0.4, 0),
            (3.7, 4),
            (-3.7, -4),
            (9223372036854774784.0, 9223372036854774784),
            (-9223372036854775808.0, -9223372036854775808),
        ] {
            assert_eq!(f64_to_i64(x), Some(want), "f64_to_i64({x})");
        }
        for x in [
            9223372036854775808.0,
            -9223372036854777856.0,
            1e300,
            f64::NAN,
            f64::INFINITY,
        ] {
            assert_eq!(f64_to_i64(x), None, "f64_to_i64({x}) 应当越界");
        }

        // string → i64：只接受 [+-]?[0-9]+
        for (s, want) in [
            ("1", Some(1)),
            ("+5", Some(5)),
            ("-0", Some(0)),
            ("007", Some(7)),
            ("9223372036854775807", Some(i64::MAX)),
            ("-9223372036854775808", Some(i64::MIN)),
        ] {
            assert_eq!(parse_i64_strict(s), want, "parse_i64_strict({s:?})");
        }
        for s in [
            " 1",
            "1 ",
            "1_000",
            "0x10",
            "1.",
            ".5",
            "1e3",
            "1.0",
            "9223372036854775808",
            "-9223372036854775809",
            "",
            "+",
            "-",
            "１２",
            "abc",
        ] {
            assert_eq!(
                parse_i64_strict(s),
                None,
                "parse_i64_strict({s:?}) 应当失败"
            );
        }

        // string → f64：十进制或科学记数法，结果必须有限
        for (s, want) in [
            ("1", 1.0),
            ("+5", 5.0),
            ("1.", 1.0),
            (".5", 0.5),
            ("1e3", 1000.0),
            ("1E+05", 100000.0),
            ("-.5e-2", -0.005),
            ("1e-400", 0.0),
            ("0.1", 0.1),
        ] {
            assert_eq!(parse_f64_strict(s), Some(want), "parse_f64_strict({s:?})");
        }
        assert!(
            parse_f64_strict("-0").unwrap().is_sign_positive(),
            "-0 规范化为 +0.0"
        );
        assert_eq!(parse_f64_strict("1.7976931348623157e308"), Some(f64::MAX));
        for s in [
            "1e400",
            "1.7976931348623159e308",
            "inf",
            "nan",
            "Infinity",
            "-inf",
            "1_000",
            " 1",
            "1 ",
            "",
            ".",
            "e5",
            "1e",
            "1e+",
            "1.2.3",
            "0x10",
            "１",
        ] {
            assert_eq!(
                parse_f64_strict(s),
                None,
                "parse_f64_strict({s:?}) 应当失败"
            );
        }
    }

    #[test]
    fn cast_typing_follows_checked_null() {
        let src = "MemoryTable(\"orders\") as o\n  .extend({\n    a = cast(1 as f64),\n    b = cast(o.amount as f64),\n    c = cast(1.5 as i64),\n    d = cast(\"1\" as i64),\n    e = cast(o.status as f64),\n  })\n  .select(a, b, c, d, e)\n  .collect()\n";
        assert_eq!(
            output_types(src),
            vec![
                ("a".into(), "f64".to_string()),  // i64 → f64 永不失败：保持非空
                ("b".into(), "?f64".to_string()), // 操作数可空 → 结果可空
                ("c".into(), "?i64".to_string()), // 可能失败的转换：操作数非空，结果也可空
                ("d".into(), "?i64".to_string()),
                ("e".into(), "?f64".to_string()),
            ]
        );
    }

    #[test]
    fn cast_rejects_everything_outside_the_four_conversions() {
        for (expr, what) in [
            ("cast(true as i64)", "bool 参与转换"),
            ("cast(1 as string)", "数值 → string"),
            ("cast(1.5 as string)", "f64 → string"),
            ("cast(o.status as string)", "相同类型"),
            ("cast(o.amount as i64)", "相同类型（可空）"),
        ] {
            let src = with_orders(expr);
            assert_eq!(codes(&src), ["SEM-003"], "{what}: {expr}");
            let bag = compile_err(&src, "SEM-003");
            assert!(
                bag.diags
                    .iter()
                    .any(|d| d.hint.as_deref().is_some_and(|h| h.contains("四种转换"))),
                "{what}：应当提示只支持哪四种转换"
            );
        }
    }

    #[test]
    fn a_bare_null_has_no_operand_type_for_cast() {
        assert_eq!(codes(&with_orders("cast(null as i64)")), ["SEM-008"]);
    }

    #[test]
    fn constant_casts_are_diagnosed_only_when_they_certainly_fail() {
        // 必然失败的常量 cast：SEM-016
        for expr in [
            "cast(\"abc\" as i64)",
            "cast(\" 1\" as i64)",
            "cast(\"1.0\" as i64)",
            "cast(\"9223372036854775808\" as i64)",
            "cast(\"1e400\" as f64)",
            "cast(\"inf\" as f64)",
            "cast(1000000000000000000000.0 as i64)",
            "cast(9223372036854775808.0 as i64)",
        ] {
            assert_eq!(codes(&with_orders(expr)), ["SEM-016"], "{expr}");
        }
        // 成功的常量 cast：不报错
        for expr in [
            "cast(2.5 as i64)",
            "cast(-2.5 as i64)",
            "cast(\"12\" as i64)",
            "cast(\"1e3\" as f64)",
            "cast(\"1e-400\" as f64)",
            "cast(9007199254740993 as f64)",
            "cast(0.49999999999999994 as i64)",
            "cast(\"-0\" as f64)",
        ] {
            compile_ok(&with_orders(expr));
        }
    }

    #[test]
    fn constant_builtin_calls_are_no_longer_misreported_as_domain_failures() {
        // 以前：常量里的内置函数调用一律被当成「必然失败」，下面这些合法程序全都编译不过
        for expr in [
            "round(1.5)",
            "round(2.5)",
            "abs(-3)",
            "abs(-3.5)",
            "lower(\"ABC\")",
            "upper(\"abc\")",
            "trim(\"  x  \")",
            "length(\"héllo\")",
            "contains(\"abc\", \"b\")",
            "is_null(1)",
            "round(2.5) + abs(-3) + length(lower(\"ABC\"))",
        ] {
            compile_ok(&with_orders(expr));
        }
        // 真正的常量定义域失败仍然要报
        for expr in [
            "abs(0 - 9223372036854775807 - 1)", // abs(i64::MIN)
            "round(1.5) / 0",
            // 混合类型：0 是 i64 字面量，先提升到 f64 再判断
            "1.5 / 0",
            "1 % 0.0",
            "1 % (length(\"ab\") - 2)",
        ] {
            assert_eq!(codes(&with_orders(expr)), ["SEM-016"], "{expr}");
        }
    }

    #[test]
    fn having_is_sugar_for_filter_after_agg() {
        let head = "MemoryTable(\"orders\") as o\n  .group_by(o.status)\n  .agg(n = count(), total = sum(o.amount))\n";
        let tail =
            "  .{op}(total > 100 && n >= 2)\n  .select(status, n)\n  .sort(n desc)\n  .collect()\n";
        let h = compile_ok(&format!("{head}{}", tail.replace("{op}", "having")));
        let f = compile_ok(&format!("{head}{}", tail.replace("{op}", "filter")));
        assert_eq!(
            hir_hash(&h),
            hir_hash(&f),
            "having 与紧跟 agg 的 filter 是同一个 HIR——语义等价的机器证据"
        );
    }

    #[test]
    fn having_must_directly_follow_agg() {
        let head = "MemoryTable(\"orders\") as o\n";
        assert_eq!(
            codes(&format!("{head}  .having(o.amount > 1)\n  .collect()\n")),
            ["SEM-005"]
        );
        // 中间隔了别的算子也不行
        assert_eq!(
            codes(&format!(
                "{head}  .agg(n = count())\n  .select(n)\n  .having(n > 1)\n  .collect()\n"
            )),
            ["SEM-005"]
        );
        // 全局聚合之后可以
        compile_ok(&format!(
            "{head}  .agg(n = count())\n  .having(n > 1)\n  .collect()\n"
        ));
        // 位置不对时报错并跳过，后面的错误照常报（第 9 章）
        assert_eq!(
            codes(&format!(
                "{head}  .filter(o.amount > 1)\n  .having(o.amount > 1)\n  .filter(o.nope > 1)\n  .collect()\n"
            )),
            ["SEM-005", "SEM-001"]
        );
    }

    #[test]
    fn having_checks_its_predicate_exactly_like_filter() {
        let head = "MemoryTable(\"orders\") as o\n";
        // 不是 bool：SEM-010
        assert_eq!(
            codes(&format!(
                "{head}  .agg(n = count())\n  .having(n)\n  .collect()\n"
            )),
            ["SEM-010"]
        );
        // 只看得见聚合的输出：输入行字段超出作用域（SEM-012）
        assert_eq!(
            codes(&format!(
                "{head}  .group_by(o.status)\n  .agg(n = count())\n  .having(o.amount > 1)\n  .collect()\n"
            )),
            ["SEM-012"]
        );
        // 谓词整体是常量且必然失败：照常诊断（非常量里的 1/0 留给运行时 checked-null）
        assert_eq!(
            codes(&format!(
                "{head}  .agg(n = count())\n  .having(1 / 0 > 0)\n  .collect()\n"
            )),
            ["SEM-016"]
        );
        // 非空收缩与 filter 相同：having(amount != null) 之后，分组键 amount 从 ?i64 收缩为 i64。
        // （聚合之后要用裸名字引用分组键：带别名的 o.amount 指向输入行字段，已超出作用域）
        let src = format!(
            "{head}  .group_by(o.amount)\n  .agg(n = count())\n  .having(amount != null)\n  .select(amount, n)\n  .collect()\n"
        );
        assert_eq!(
            output_types(&src),
            vec![
                ("amount".into(), "i64".to_string()),
                ("n".into(), "i64".to_string())
            ]
        );
    }
}
