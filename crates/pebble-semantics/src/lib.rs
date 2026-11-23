//! pebble-semantics：解析域/Schema/类型检查 + AST → HIR lowering。
//!
//! 实现 docs/draft/spec/04/05/07 的全部 P0 语义：
//! - 严格 null 传播；`x == null`/`x != null` lowering 为判空内置形式（恒 bool）；
//! - 强 Kleene 逻辑；谓词只认 true；
//! - 流敏非空收缩 NN(p)（可靠但不完备）；
//! - checked-null：可能失败运算静态类型必须可空；编译期可确定的定义域失败报 SEM-016；
//! - 聚合签名按「全局/分组 × 参数可空 × 溢出」推导；
//! - 源别名与输出字段名分属不同命名空间；join 重名裸引用报 SEM-011；
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
    };
    let result = lower.run(&program.pipeline);
    if diags.has_error() {
        return None;
    }
    result
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
}

impl Lowerer<'_> {
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
                        return None;
                    };
                    let (node_id, schema) = self.lower_aggregate(
                        current_id,
                        &current_schema,
                        Some(keys),
                        agg_bindings,
                        *agg_span,
                    )?;
                    current_id = node_id;
                    current_schema = schema;
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
                    )?;
                    current_id = node_id;
                    current_schema = schema;
                    i += 1;
                }
                ast::Op::Filter { pred, span } => {
                    self.history.push(current_schema.clone());
                    let resolved = self.resolve_expr(pred, &current_schema, false)?;
                    self.check_predicate(&resolved)?;
                    self.const_domain_check(&resolved);
                    let narrowed = self.narrow(&current_schema, &resolved);
                    let node_id = self.push_node(vec![current_id], narrowed, *span, |base| {
                        HirNode::Transform {
                            base,
                            kind: TransformKind::Filter { pred: resolved },
                        }
                    });
                    current_schema = self.nodes[node_id as usize].base().output.clone();
                    current_id = node_id;
                    i += 1;
                }
                ast::Op::Extend { record, span } => {
                    self.history.push(current_schema.clone());
                    // 绑定名唯一（SEM-014）
                    let mut seen = BTreeSet::new();
                    for b in &record.bindings {
                        if !seen.insert(b.name.clone()) {
                            self.diags.error(
                                Code::SemDuplicateName,
                                format!("同一 extend 记录中名称 `{}` 重复", b.name),
                                b.span,
                            );
                            return None;
                        }
                    }
                    // RHS 对输入行求值；兄弟绑定互不可见（环境即输入 Schema）
                    let mut hir_bindings = Vec::new();
                    for b in &record.bindings {
                        let value = self.resolve_expr(&b.value, &current_schema, false)?;
                        self.const_domain_check(&value);
                        let output = self.fresh_field();
                        hir_bindings.push(pebble_hir::Binding {
                            output,
                            name: b.name.clone(),
                            value,
                        });
                    }
                    // 输出 Schema：覆盖留在原位并换身份；新增按绑定顺序追加
                    let mut fields: Vec<FieldSchema> = Vec::new();
                    let mut replaced: BTreeSet<String> = BTreeSet::new();
                    for f in &current_schema.fields {
                        if let Some(b) = hir_bindings.iter().find(|b| b.name == f.name) {
                            if current_schema.by_name(&f.name).len() > 1 {
                                self.diags.error(
                                    Code::SemTypeMismatch,
                                    format!(
                                        "extend 覆盖 `{}` 时输入存在多个同名字段，必须先消歧",
                                        f.name
                                    ),
                                    *span,
                                );
                                return None;
                            }
                            replaced.insert(f.name.clone());
                            fields.push(FieldSchema {
                                id: b.output,
                                name: f.name.clone(),
                                ty: b.value.ty().clone(),
                                origin: FieldOrigin::Derived {
                                    node: self.nodes.len() as NodeId,
                                    binding: f.name.clone(),
                                },
                            });
                        } else {
                            fields.push(f.clone());
                        }
                    }
                    for b in &hir_bindings {
                        if !replaced.contains(&b.name) {
                            let ty = b.value.ty().clone();
                            fields.push(FieldSchema {
                                id: b.output,
                                name: b.name.clone(),
                                ty,
                                origin: FieldOrigin::Derived {
                                    node: self.nodes.len() as NodeId,
                                    binding: b.name.clone(),
                                },
                            });
                        }
                    }
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
                    i += 1;
                }
                ast::Op::Select { items, span } => {
                    self.history.push(current_schema.clone());
                    let mut projections = Vec::new();
                    let mut out_fields = Vec::new();
                    let mut seen_names: BTreeMap<String, Span> = BTreeMap::new();
                    for item in items {
                        match &item.alias {
                            None => {
                                // 无 as：字段引用（parser 已保证），保留身份
                                let resolved =
                                    self.resolve_expr(&item.expr, &current_schema, false)?;
                                self.const_domain_check(&resolved);
                                let field = match &resolved {
                                    THirExpr::Field { field, .. } => *field,
                                    _ => unreachable!("PAR-006 已保证字段引用"),
                                };
                                let src = current_schema
                                    .fields
                                    .iter()
                                    .find(|f| f.id == field)
                                    .expect("字段已解析");
                                let name = src.name.clone();
                                if let Some(prev) = seen_names.insert(name.clone(), item.span) {
                                    self.diags.error(
                                        Code::SchDuplicateOutput,
                                        format!("select 输出名称 `{}` 重复", name),
                                        item.span.merge(prev),
                                    );
                                    return None;
                                }
                                projections.push(Projection {
                                    expr: resolved,
                                    output: field,
                                    name: name.clone(),
                                });
                                out_fields.push(src.clone());
                            }
                            Some(alias) => {
                                let resolved =
                                    self.resolve_expr(&item.expr, &current_schema, false)?;
                                self.const_domain_check(&resolved);
                                if let Some(prev) = seen_names.insert(alias.clone(), item.span) {
                                    self.diags.error(
                                        Code::SchDuplicateOutput,
                                        format!("select 输出名称 `{}` 重复", alias),
                                        item.span.merge(prev),
                                    );
                                    return None;
                                }
                                let output = self.fresh_field();
                                let ty = resolved.ty().clone();
                                projections.push(Projection {
                                    expr: resolved,
                                    output,
                                    name: alias.clone(),
                                });
                                out_fields.push(FieldSchema {
                                    id: output,
                                    name: alias.clone(),
                                    ty,
                                    origin: FieldOrigin::Derived {
                                        node: self.nodes.len() as NodeId,
                                        binding: alias.clone(),
                                    },
                                });
                            }
                        }
                    }
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
                    for k in keys {
                        let resolved = self.resolve_expr(&k.expr, &current_schema, false)?;
                        self.const_domain_check(&resolved);
                        let base = resolved.ty().base();
                        match base {
                            Type::I64 | Type::F64 | Type::Str => {}
                            _ => {
                                self.diags.error(
                                    Code::SemOpUnsupported,
                                    "sort 键类型必须可排序（i64/f64/string）",
                                    k.span,
                                );
                                return None;
                            }
                        }
                        hir_keys.push(SortKeyH {
                            expr: resolved,
                            descending: k.descending,
                        });
                    }
                    let node_id =
                        self.push_node(vec![current_id], current_schema.clone(), *span, |base| {
                            HirNode::Transform {
                                base,
                                kind: TransformKind::Sort { keys: hir_keys },
                            }
                        });
                    current_id = node_id;
                    i += 1;
                }
                ast::Op::Take { n, span } => {
                    if *n < 0 {
                        self.diags
                            .error(Code::SemTypeMismatch, "take 的参数必须是非负整数", *span);
                        return None;
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
                    let (right_id, right_schema) = self.lower_source(right)?;
                    let combined = Schema {
                        fields: current_schema
                            .fields
                            .iter()
                            .chain(right_schema.fields.iter())
                            .cloned()
                            .collect(),
                    };
                    let resolved = self.resolve_expr(on, &combined, true)?;
                    self.check_predicate(&resolved)?;
                    self.const_domain_check(&resolved);
                    let node_id =
                        self.push_node(vec![current_id, right_id], combined, *span, |base| {
                            HirNode::Transform {
                                base,
                                kind: TransformKind::Join { on: resolved },
                            }
                        });
                    current_schema = self.nodes[node_id as usize].base().output.clone();
                    current_id = node_id;
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
            if self.aliases.insert(alias.to_string(), source_id).is_some() {
                self.diags.error(
                    Code::SemAliasDuplicate,
                    format!("源别名 `{}` 重复", alias),
                    lock_span,
                );
                return None;
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

    #[allow(clippy::too_many_arguments)]
    fn lower_aggregate(
        &mut self,
        input_id: NodeId,
        input_schema: &Schema,
        group: Option<&Vec<ast::GroupKey>>,
        bindings: &Vec<ast::AggBinding>,
        span: Span,
    ) -> Option<(NodeId, Schema)> {
        self.history.push(input_schema.clone());
        let grouped = group.is_some();
        let node_id_placeholder = self.nodes.len() as NodeId;

        // 分组键：MVP 只接受字段引用，输出名去限定
        let mut group_keys = Vec::new();
        let mut key_names: BTreeMap<String, Span> = BTreeMap::new();
        if let Some(keys) = group {
            for k in keys {
                if k.alias.is_some() {
                    self.diags.error(
                        Code::SemTypeMismatch,
                        "MVP 不支持分组键 `as` 命名（表达式分组键与命名后置）",
                        k.span,
                    );
                    return None;
                }
                let field_expr = match &k.expr {
                    ast::Expr::Field { alias, name, span } => (alias, name, *span),
                    _ => {
                        self.diags.error(
                            Code::SemTypeMismatch,
                            "MVP 的 group_by 键只接受字段引用",
                            k.span,
                        );
                        return None;
                    }
                };
                let resolved =
                    self.resolve_field(field_expr.0, field_expr.1, field_expr.2, input_schema)?;
                let name = field_expr.1.clone();
                if let Some(prev) = key_names.insert(name.clone(), k.span) {
                    self.diags.error(
                        Code::SchGroupKeyRename,
                        format!("分组键去限定后重名 `{}`（SCH-004）", name),
                        k.span.merge(prev),
                    );
                    return None;
                }
                let ty = resolved.ty().clone();
                let output = self.fresh_field();
                group_keys.push(pebble_hir::GroupKeyH {
                    value: resolved,
                    output,
                    name: name.clone(),
                });
                // 分组键输出（组级值）放在 node schema；先记录，稍后统一构建
                let _ = ty;
            }
        }

        // 聚合绑定
        let mut hir_bindings = Vec::new();
        let mut all_names: BTreeMap<String, Span> = key_names.clone();
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
                return None;
            }
            let (ast_func, arg, call_span) = (&b.call.func, &b.call.arg, b.call.span);
            let func = to_hir_agg(ast_func);
            // count 可带/不带参数；其余聚合必须恰好一个参数
            let arity_ok = if matches!(func, AggFunc::Count) {
                true
            } else {
                arg.is_some()
            };
            if !arity_ok {
                self.diags.error(
                    Code::SemTypeMismatch,
                    format!("`{}` 的参数数量不合法", func_name(&func)),
                    call_span,
                );
                return None;
            }
            let resolved_arg = match arg {
                Some(a) => Some(self.resolve_with_base(a, input_schema, None, false)?),
                None => None,
            };
            if let Some(ra) = &resolved_arg {
                self.const_domain_check(ra);
            }
            let ty = self.agg_result_type(&func, resolved_arg.as_ref(), grouped, call_span)?;
            let output = self.fresh_field();
            hir_bindings.push(AggBindingH {
                output,
                name: b.name.clone(),
                call: AggCallH {
                    func,
                    arg: resolved_arg,
                },
            });
            let _ = ty;
        }

        // 输出 Schema：分组键 + 聚合绑定
        let mut fields = Vec::new();
        for k in &group_keys {
            let ty = k.value.ty().clone();
            fields.push(FieldSchema {
                id: k.output,
                name: k.name.clone(),
                ty,
                origin: FieldOrigin::Derived {
                    node: node_id_placeholder,
                    binding: k.name.clone(),
                },
            });
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
                    node: node_id_placeholder,
                    binding: b.name.clone(),
                },
            });
        }

        let node_id = self.push_node(vec![input_id], Schema { fields }, span, |base| {
            HirNode::Aggregate {
                base,
                group_keys,
                bindings: hir_bindings,
            }
        });
        debug_assert_eq!(node_id, node_id_placeholder);
        let schema = self.nodes[node_id as usize].base().output.clone();
        Some((node_id, schema))
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
            let l = self.resolve_with_base(left, env, Some(Bool), in_join_on)?;
            let r = self.resolve_with_base(right, env, Some(Bool), in_join_on)?;
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
                let matches: Vec<&FieldSchema> = env
                    .fields
                    .iter()
                    .filter(|f| match &f.origin {
                        FieldOrigin::Source {
                            source,
                            source_field,
                        } => *source == source_id && source_field == name,
                        FieldOrigin::Derived { .. } => false,
                    })
                    .collect();
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
                let matches = env.by_name(name);
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

    fn visible_fields(&self, env: &Schema) -> String {
        env.fields
            .iter()
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
                "编译期可确定的定义域失败（除零/模零/整数溢出/非有限 f64 结果）",
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

enum ConstVal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
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
        THirExpr::Call { span, .. } => Err(*span),
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
        if let Some(program) = pebble_parser::parse(src, &mut diags) {
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
}
