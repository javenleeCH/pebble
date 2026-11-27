//! 第 8 章：解析器错误恢复的行为契约。
//!
//! 每个测试断言的都是**完整的错误列表**（码 + 起始行 + 起始列），而不只是「包含某个码」——
//! 多报一个级联错误和少报一个真实错误同样是 bug。

use pebble_diagnostic::{DiagBag, Span};
use pebble_parser::{PartialAst, SourceOp, TerminalOp, parse};

/// 一条诊断的位置：（错误码, 起始行, 起始列）。测试断言的是 `Vec<Located>`——
/// 恰好这些错误，一条不多、一条不少。
type Located = (String, usize, usize);

/// 一次完整的解析：诊断按源码位置排好序（与 `pebble build` 的输出顺序一致）。
fn run(src: &str) -> (Vec<Located>, Result<pebble_parser::Program, PartialAst>) {
    let mut diags = DiagBag::new();
    let result = parse(src, &mut diags);
    diags.normalize();
    let errors = diags
        .diags
        .iter()
        .map(|d| {
            let span = d.span.expect("解析期诊断必有源码位置");
            let (line, col) = line_col(src, span);
            (d.code.as_str().to_string(), line, col)
        })
        .collect();
    (errors, result)
}

fn errs(src: &str) -> Vec<Located> {
    run(src).0
}

/// 恢复出来的部分 AST（断言用）。
fn partial(src: &str) -> PartialAst {
    match run(src).1 {
        Ok(_) => panic!("期望解析失败，却成功了:\n{src}"),
        Err(p) => p,
    }
}

/// Span 起点 →（1-based 行，列）。列按**字符**数：一个汉字算一列。
fn line_col(src: &str, span: Span) -> (usize, usize) {
    let before = &src[..span.start];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

// ---------- 总原则：一个根因只报一次，独立的错误一个不漏 ----------

#[test]
fn two_broken_ops_report_both() {
    let (errors, result) =
        run("Table(\"a.csv\") as o\n  .filter(o.a > )\n  .select(o.b +)\n  .collect()\n");
    assert!(result.is_err());
    assert_eq!(
        errors,
        [("PAR-001".into(), 2, 17), ("PAR-001".into(), 3, 16)]
    );
    // 两个坏算子都被丢弃；终端完整解析，保留在部分 AST 里
    let p = partial("Table(\"a.csv\") as o\n  .filter(o.a > )\n  .select(o.b +)\n  .collect()\n");
    assert!(p.ops().is_empty());
    assert!(matches!(p.terminal(), Some(TerminalOp::Collect { .. })));
}

#[test]
fn lex_and_parse_errors_in_one_pass() {
    // 词法错误不再挡住语法错误：以前只要有 LEX 错误，解析器根本不运行
    let errors =
        errs("Table(\"a.csv\") as o\n  .filter(o.a ^ 2 > 1)\n  .select(o.b +)\n  .collect()\n");
    assert_eq!(
        errors,
        [("LEX-001".into(), 2, 15), ("PAR-001".into(), 3, 16)]
    );
}

// ---------- 词法层的洞：解析器绝不重复报错 ----------

#[test]
fn lex_hole_is_never_re_reported() {
    // 若词法层悄悄丢掉 `^`，解析器会接着报「期望 `)`，遇到 整数 `2`」——那就是级联错误
    let errors = errs("Table(\"a.csv\") as o\n  .filter(o.a ^ 2 > 1)\n  .collect()\n");
    assert_eq!(errors, [("LEX-001".into(), 2, 15)]);
}

#[test]
fn lex_hole_at_operator_name_position() {
    let errors = errs("Table(\"a.csv\") as o\n  .@filter(o.a > 1)\n  .select(o.b)\n");
    assert_eq!(errors, [("LEX-001".into(), 2, 4)]);
}

#[test]
fn unclosed_string_leaves_one_hole() {
    // 未闭合字符串之后的同行内容都被吞进字符串里；下一行的算子照常解析
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .filter(o.name == \"paid\n",
        "  .select(o.b)\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("LEX-002".into(), 2, 21)]);
}

#[test]
fn bad_number_forms_keep_the_token_stream_intact() {
    // 错误从数字的起点开始标注（`  .take(` 占 8 个字符）
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .take(1.)\n  .collect()\n"),
        [("LEX-004".into(), 2, 9)]
    );
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .take(.5)\n  .collect()\n"),
        [("LEX-004".into(), 2, 9)]
    );
    // 连 i128 都放不下的整数没有值可救：留一个洞，同样只报一次
    let huge = "9".repeat(60);
    assert_eq!(
        errs(&format!(
            "Table(\"a.csv\") as o\n  .take({huge})\n  .collect()\n"
        )),
        [("LEX-004".into(), 2, 9)]
    );
}

// ---------- 围栏 1：列表里的 `,` —— 坏一个元素，保住其余元素 ----------

#[test]
fn list_items_are_independent() {
    // 两个坏绑定各自报错；好绑定 d 仍然被解析（所以不会有第三个错误）
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .agg(\n",
        "    a = sum(o.x),\n",
        "    b = 1 + 1,\n",
        "    c = foo(1),\n",
        "    d = count(),\n",
        "  )\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-005".into(), 4, 9), ("PAR-005".into(), 5, 9)]);
}

#[test]
fn list_item_error_skips_only_that_item() {
    // 第一项 `as` 后面缺名字；同步停在 `,`，第二项 `o.b as y` 正常解析
    let errors = errs("Table(\"a.csv\") as o\n  .select(o.a + 1 as, o.b as y)\n  .collect()\n");
    assert_eq!(errors, [("PAR-001".into(), 2, 21)]);
}

#[test]
fn leading_comma_in_list() {
    let errors = errs("Table(\"a.csv\") as o\n  .select(, o.b)\n  .collect()\n");
    assert_eq!(errors, [("PAR-001".into(), 2, 11)]);
}

// ---------- 围栏 2：右括号 —— 坏的调用/括号表达式不外溢 ----------

#[test]
fn nested_call_error_is_contained() {
    // 坏的是 f(...) 这一项；同步到 select 自己的 `,`，`o.b` 照常解析
    let errors = errs("Table(\"a.csv\") as o\n  .select(f(o.a +) as x, o.b)\n  .collect()\n");
    assert_eq!(errors, [("PAR-001".into(), 2, 18)]);
}

#[test]
fn mismatched_closer_does_not_cascade() {
    // f( 没闭合就遇到了外层 if 的 `}`。括号栈认得「这个 `}` 不是我的」，
    // 所以后面的 else 分支和 y 绑定不会跟着遭殃
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({\n",
        "    x = if o.a > 0 { f(o.a + } else { 1 },\n",
        "    y = 2,\n",
        "  })\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-001".into(), 3, 30)]);
}

#[test]
fn if_without_else_keeps_sibling_binding() {
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({\n",
        "    x = if o.a > 0 { 1 },\n",
        "    y = 2,\n",
        "  })\n",
        "  .collect()\n",
    ));
    // 错误报在 then 分支的 `}` 之后、本该出现 `else` 的 token 上（这里是同行的 `,`）
    assert_eq!(errors, [("PAR-004".into(), 3, 25)]);
}

#[test]
fn unclosed_operator_at_end_of_file() {
    // 源码以换行结尾：EOF 在下一行行首
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .filter(o.a > 1\n"),
        [("PAR-001".into(), 3, 1)]
    );
    // 源码不以换行结尾：EOF 紧跟在最后一个字符之后（第 2 行共 17 个字符）
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .filter(o.a > 1"),
        [("PAR-001".into(), 2, 18)]
    );
}

// ---------- 围栏 3：下一个算子的起点 —— 回到流水线层 ----------

#[test]
fn missing_paren_before_next_operator() {
    // 新手最常见的错误：行尾忘写 `)`。错误报在下一个算子的 `.` 上，
    // 并且**就地恢复**：后面的 .select / .collect 被完整保留
    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1\n  .select(o.b)\n  .collect()\n";
    assert_eq!(errs(src), [("PAR-001".into(), 3, 3)]);
    let p = partial(src);
    assert_eq!(p.ops().len(), 1, "select 应被恢复出来");
    assert!(matches!(p.ops()[0], pebble_parser::Op::Select { .. }));
    assert!(matches!(p.terminal(), Some(TerminalOp::Collect { .. })));
}

#[test]
fn missing_paren_inside_list_swallows_the_late_closer() {
    // 在右括号前插入算子的典型笔误：被「隐式闭合」的括号，迟到的 `)` 是残骸
    let src = "Table(\"a.csv\") as o\n  .select(o.a, o.b\n  .sort(o.a))\n";
    assert_eq!(errs(src), [("PAR-001".into(), 3, 3)]);
    let p = partial(src);
    assert_eq!(p.ops().len(), 1);
    assert!(matches!(p.ops()[0], pebble_parser::Op::Sort { .. }));
}

#[test]
fn missing_two_closers_before_next_operator() {
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({\n",
        "    x = 1,\n",
        "    y = o.a\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-001".into(), 5, 3)]);
}

#[test]
fn unknown_operator_is_skipped() {
    let src = concat!(
        "Table(\"a.csv\") as o\n",
        "  .frobnicate(o.a)\n",
        "  .select(o.a)\n",
        "  .collect()\n",
    );
    assert_eq!(errs(src), [("PAR-001".into(), 2, 4)]);
    let p = partial(src);
    assert_eq!(p.ops().len(), 1, "select 应被恢复出来");
    assert!(matches!(p.terminal(), Some(TerminalOp::Collect { .. })));
}

#[test]
fn op_after_terminal_reported_once() {
    // 终端后面接了两个算子：PAR-002 只报一次
    let errors = errs("Table(\"a.csv\") as o\n  .collect()\n  .filter(o.a > 0)\n  .sort(o.a)\n");
    assert_eq!(errors, [("PAR-002".into(), 3, 3)]);
}

#[test]
fn operator_without_open_paren() {
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .filter o.a > 1)\n  .collect()\n"),
        [("PAR-001".into(), 2, 11)]
    );
}

// ---------- 源：坏了就跳到下一个算子 ----------

#[test]
fn broken_source_does_not_cascade() {
    // 以前：报完「必须以 Table 开头」之后，还会再报「期望文件结尾，遇到 `(`」
    let src = "Tabl(\"a.csv\") as o\n  .collect()\n";
    assert_eq!(errs(src), [("PAR-001".into(), 1, 1)]);
    let p = partial(src);
    assert!(matches!(p.source(), SourceOp::Table { path, .. } if path.is_empty()));
    assert!(matches!(p.terminal(), Some(TerminalOp::Collect { .. })));
}

#[test]
fn source_missing_paren_or_alias() {
    assert_eq!(
        errs("Table \"a.csv\" as o\n  .collect()\n"),
        [("PAR-001".into(), 1, 7)]
    );
    assert_eq!(
        errs("Table(\"a.csv\") as\n  .collect()\n"),
        [("PAR-001".into(), 2, 3)]
    );
}

// ---------- 具体的错误要抢在通用的之前（不然消息会误导）----------

#[test]
fn condition_in_braces_is_par004_not_par003() {
    // 把条件写进 `{ }`：若不专门识别，它会掉进「记录字面量只能给 extend 用」
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({ x = if { o.a > 0 } { 1 } else { 2 } })\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-004".into(), 2, 20)]);
}

#[test]
fn extra_argument_is_not_blamed_on_a_missing_paren() {
    // 错误报在多余的 `,` 上，消息不说「缺少右括号」
    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1, o.b > 2)\n  .collect()\n";
    assert_eq!(errs(src), [("PAR-001".into(), 2, 18)]);
    let mut diags = DiagBag::new();
    let _ = parse(src, &mut diags);
    let message = &diags.diags[0].message;
    assert!(message.contains("不需要 `,`"), "{message}");
    assert!(!message.contains("缺少"), "{message}");
}

#[test]
fn method_call_habit_is_a_single_error() {
    // `o.amount.abs(` 会被当成「括号里冒出了下一个算子」。恢复之后不能再追加
    // 一条「未知算子 abs」——那只是恢复方式的后果，不是用户的另一个错误
    let errors = errs(concat!(
        "Table(\"a.csv\") as o\n",
        "  .filter(o.amount.abs() > 1)\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-001".into(), 2, 19)]);
}

// ---------- 流水线层：真正多余的东西仍然要报 ----------

#[test]
fn stray_closer_at_pipeline_level_is_reported() {
    assert_eq!(
        errs("Table(\"a.csv\") as o\n  .collect())\n"),
        [("PAR-001".into(), 2, 13)]
    );
}

#[test]
fn empty_file_reports_exactly_one_error() {
    assert_eq!(errs(""), [("PAR-001".into(), 1, 1)]);
    assert_eq!(errs("\n\n  // 只有注释\n"), [("PAR-001".into(), 4, 1)]);
}

// ---------- Span 的单位：中文注释之后行列不漂移 ----------

#[test]
fn positions_after_chinese_comments_are_exact() {
    let errors = errs(concat!(
        "// 第 8 章演示：每个汉字在 UTF-8 里占 3 字节\n",
        "Table(\"a.csv\") as o\n",
        "  .filter(o.a > )\n",
        "  .collect()\n",
    ));
    assert_eq!(errors, [("PAR-001".into(), 3, 17)]);
}

// ---------- 不会死循环、不会报个没完 ----------

#[test]
fn pathological_inputs_terminate() {
    let cases = [
        "",
        "(((((",
        ")))))",
        ",,,,,,,,",
        ".",
        "...",
        "{",
        "}}}",
        "|||||",
        "\"unterminated",
        "Table(\"a.csv\") as o .filter((((((",
        "Table(\"a.csv\") as o\n  .agg(a = count(, b = sum(, c = min())\n",
        "Table(\"a.csv\") as o\n  .select(, , , ,)\n",
        "Table(\"a.csv\") as o\n  .extend({ x = , y = , z = })\n",
    ];
    for src in cases {
        let (errors, result) = run(src);
        assert!(result.is_err(), "{src:?} 不应解析成功");
        assert!(!errors.is_empty(), "{src:?} 至少报一个错");
        let tokens_upper_bound = src.chars().count() + 2;
        assert!(
            errors.len() <= tokens_upper_bound,
            "{src:?}: 错误数 {} 超过上界 {}",
            errors.len(),
            tokens_upper_bound
        );
    }
}

// ---------- 没有错误的程序：行为与恢复之前完全一致 ----------

#[test]
fn all_examples_parse_clean() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("examples 目录") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("pebble") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read example");
        let mut diags = DiagBag::new();
        let result = parse(&src, &mut diags);
        assert!(
            result.is_ok(),
            "{} 解析失败: {:?}",
            path.display(),
            diags.diags
        );
        assert!(diags.diags.is_empty(), "{} 不应有任何诊断", path.display());
        checked += 1;
    }
    assert_eq!(checked, 7, "examples/ 下应有 7 个 .pebble 样例");
}

// ---------- 期望位置不靠手数：由源码里的子串直接算出来 ----------

/// `needle` 在 `src` 中第一次出现处的（行, 列）。
fn pos(src: &str, needle: &str) -> (usize, usize) {
    let start = src
        .find(needle)
        .unwrap_or_else(|| panic!("测试源码里找不到 {needle:?}"));
    line_col(src, Span::new(start, start + needle.len()))
}

/// 一条位于 `needle` 处的错误。
fn err_at(src: &str, code: &str, needle: &str) -> Located {
    let (line, col) = pos(src, needle);
    (code.to_string(), line, col)
}

/// `needle` 在 `anchor` 之后第一次出现处的（行, 列）——同一个 token 在源码里出现多次时用。
fn pos_after(src: &str, anchor: &str, needle: &str) -> (usize, usize) {
    let anchor_at = src
        .find(anchor)
        .unwrap_or_else(|| panic!("测试源码里找不到 {anchor:?}"));
    let start = src[anchor_at..]
        .find(needle)
        .map(|k| anchor_at + k)
        .unwrap_or_else(|| panic!("{anchor:?} 之后找不到 {needle:?}"));
    line_col(src, Span::new(start, start + needle.len()))
}

/// 一条位于 `anchor` 之后第一个 `needle` 处的错误。
fn err_after(src: &str, code: &str, anchor: &str, needle: &str) -> Located {
    let (line, col) = pos_after(src, anchor, needle);
    (code.to_string(), line, col)
}

/// 解析 `src` 并返回第一条诊断（用于检查消息与提示）。
fn first_diag(src: &str) -> pebble_diagnostic::Diagnostic {
    let mut diags = DiagBag::new();
    let _ = parse(src, &mut diags);
    diags.normalize();
    diags
        .diags
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("期望至少一条诊断:\n{src}"))
}

// ---------- 残骸模式：恢复出来的「算子」要站得住脚才算数 ----------
//
// 一个根因只报一次的另一半：恢复本身不能制造新的「错误」。
// 没闭合的表达式后面如果还跟着 `.名字(`，它多半是表达式的一部分（比如方法调用），
// 而不是下一个算子。

#[test]
fn recovery_never_manufactures_diagnostics() {
    // 方法调用习惯：两个 `.abs(` 都是残骸，只有第一个（被当成算子起点的那一个）报错
    let src =
        "Table(\"a.csv\") as o\n  .filter(o.amount.abs() > 1 && o.score.abs() < 2)\n  .collect()\n";
    assert_eq!(errs(src), [err_at(src, "PAR-001", ".abs()")]);

    // `.select()` 是已知算子名，但它后面跟着 `> 1)`：这是一个元素该有的结尾吗？不是——丢弃
    let src = "Table(\"a.csv\") as o\n  .filter(o.rows.select() > 1)\n  .sort(o.amount)\n";
    assert_eq!(errs(src), [err_at(src, "PAR-001", ".select()")]);
    let p = partial(src);
    assert_eq!(p.ops().len(), 1, "sort 应被恢复出来");

    // 同理：被丢弃的 `.collect()` 不能让后面的 `.select` 背上 PAR-002
    let src = "Table(\"a.csv\") as o\n  .filter(o.rows.collect() > 1)\n  .select(o.order_id)\n";
    assert_eq!(errs(src), [err_at(src, "PAR-001", ".collect()")]);
    let p = partial(src);
    assert_eq!(p.ops().len(), 1);
    assert!(p.terminal().is_none());

    // 列表里：坏元素被一个多余的逗号截断，后半截 `0 { 1 } else { 2 }` 不是一个新的绑定
    let src = concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({\n",
        "    x = if o.amount >, 0 { 1 } else { 2 },\n",
        "    y = 2,\n",
        "  })\n",
        "  .collect()\n",
    );
    assert_eq!(errs(src), [err_at(src, "PAR-001", ", 0 {")]);
}

#[test]
fn independent_errors_in_consecutive_items_are_still_reported() {
    // 上一条规则的边界：只有「一个 token 都没读进去就失败」的元素才被当成残骸。
    // 两个各自写错了的绑定，第二个读进了 `b = o.y +` 才失败，是独立的错误，必须报出来
    let src = concat!(
        "Table(\"a.csv\") as o\n",
        "  .extend({\n",
        "    a = o.x +,\n",
        "    b = o.y +,\n",
        "  })\n",
        "  .collect()\n",
    );
    assert_eq!(
        errs(src),
        [
            err_at(src, "PAR-001", ",\n    b"),
            err_at(src, "PAR-001", ",\n  })"),
        ]
    );
}

// ---------- 流水线层：多余的东西报一次，但不挡住后面的错误 ----------

#[test]
fn stray_token_at_pipeline_level_does_not_stop_parsing() {
    // 以前：报完「期望文件结尾，遇到 `)`」就停了，第 3 行真正的错误要等下一次编译才看得见
    let src = concat!(
        "Table(\"a.csv\") as o\n",
        "  .filter(o.a > 1))\n",
        "  .select(o.b +)\n",
        "  .collect()\n",
    );
    assert_eq!(
        errs(src),
        [
            err_at(src, "PAR-001", ")\n  .select"),
            err_after(src, "PAR-001", ".select(o.b +", ")"),
        ]
    );
    assert!(
        first_diag(src).message.contains("多出了一个"),
        "{}",
        first_diag(src).message
    );

    // 词法洞不算内容：`Table("a.csv") ^ as o` 仍然带着别名，不会连带报出「多出了一个 as」
    let src = "Table(\"a.csv\") ^ as o\n  .collect()\n";
    assert_eq!(errs(src), [err_at(src, "LEX-001", "^")]);
}

#[test]
fn errors_after_a_terminal_are_still_found() {
    // PAR-002 只报一次；但它后面的算子照常解析——里面独立的笔误不能被它吞掉
    let src = concat!(
        "Table(\"a.csv\") as o\n",
        "  .collect()\n",
        "  .filter(o.typo > )\n",
        "  .select(o.c +)\n",
    );
    let errors = errs(src);
    assert_eq!(
        errors,
        [
            err_at(src, "PAR-002", ".filter"),
            err_at(src, "PAR-001", ")\n  .select"),
            err_after(src, "PAR-001", ".select(o.c +", ")"),
        ]
    );
    assert_eq!(errors.iter().filter(|e| e.0 == "PAR-002").count(), 1);
}

// ---------- 提示只说站得住的话 ----------

#[test]
fn missing_paren_hint_is_only_given_when_it_is_plausible() {
    let hint_of = |src: &str| {
        let d = first_diag(src);
        (d.message.clone(), d.hint.clone().unwrap_or_default())
    };

    // 参数在遇到 `=` 之前就已经完整：这时说「缺少右括号」是实话的反面
    let src = "Table(\"a.csv\") as o\n  .filter(o.status = \"paid\")\n  .collect()\n";
    let (message, hint) = hint_of(src);
    assert!(hint.contains("=="), "{message} / {hint}");
    assert!(!hint.contains("缺少右括号"), "{message} / {hint}");

    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1 o.b > 2)\n  .collect()\n";
    assert!(!hint_of(src).1.contains("缺少右括号"), "{}", hint_of(src).1);

    // 不接受参数的算子
    let src = "Table(\"a.csv\") as o\n  .distinct(o.city)\n  .collect()\n";
    assert!(hint_of(src).0.contains("不接受参数"), "{}", hint_of(src).0);

    // 这三种位置上，「缺少右括号」才是可信的推断
    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1\n";
    assert!(hint_of(src).1.contains("缺少右括号"), "{}", hint_of(src).1);
    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1\n  .select(o.b)\n  .collect()\n";
    assert!(hint_of(src).1.contains("缺少"), "{}", hint_of(src).1);
    let src = "Table(\"a.csv\") as o\n  .filter(o.a > 1}\n  .collect()\n";
    assert!(hint_of(src).1.contains("缺少右括号"), "{}", hint_of(src).1);
}

#[test]
fn messages_name_the_right_context() {
    // join 的右表不是「流水线的开头」
    let src = "Table(\"a.csv\") as o\n  .join(users, on = o.a == o.b)\n  .collect()\n";
    let message = first_diag(src).message;
    assert!(message.contains("join 的右表必须"), "{message}");
    assert!(!message.contains("流水线必须"), "{message}");

    // 条件里少了操作数，then 分支的 `{` 被当成了操作数：提示要指向真正的原因
    let src = "Table(\"a.csv\") as o\n  .extend({ x = if o.a > { 1 } else { 2 } })\n  .collect()\n";
    let d = first_diag(src);
    assert_eq!(d.code.as_str(), "PAR-003");
    assert!(d.hint.unwrap_or_default().contains("条件"), "{}", d.message);

    // 「期望」与反引号/英文之间留空格，与中文直接相连
    let src = "Table(\"a.csv\") as a\n  .join(Table(\"b.csv\") as b, (x == y))\n  .collect()\n";
    let message = first_diag(src).message;
    assert!(message.contains("期望 `on`，遇到"), "{message}");
    let src = "Table(\"a.csv\") as o\n  .(\n";
    let message = first_diag(src).message;
    assert!(message.contains("期望算子名，遇到"), "{message}");
}

// ---------- 资源上限：深嵌套与超长表达式报错，而不是撑爆调用栈 ----------

#[test]
fn deep_and_long_expressions_are_rejected_not_crashed() {
    let wrap = |expr: &str| format!("Table(\"a.csv\") as o\n  .filter({expr})\n  .collect()\n");

    // 100 层括号：超出上限，恰好一条诊断
    let deep = format!("{}1{}", "(".repeat(100), ")".repeat(100));
    let errors = errs(&wrap(&deep));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(first_diag(&wrap(&deep)).message.contains("嵌套过深"));

    // 60 层：在上限之内，完全合法
    let ok = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    assert!(
        errs(&wrap(&ok)).is_empty(),
        "{}",
        errs(&wrap(&ok)).is_empty()
    );

    // 右结合的一元运算符与 else-if 链同样是递归
    for deep in [
        format!("{}1", "-".repeat(100)),
        format!("{}true", "!".repeat(100)),
    ] {
        let errors = errs(&wrap(&deep));
        assert_eq!(errors.len(), 1, "{errors:?}");
    }
    let chain = |n: usize| {
        let mut s = String::from("if o.a { 1 }");
        for _ in 0..n {
            s.push_str(" else if o.b { 2 }");
        }
        s.push_str(" else { 3 }");
        s
    };
    assert_eq!(errs(&wrap(&chain(100))).len(), 1);
    assert!(errs(&wrap(&chain(30))).is_empty());

    // 一条 300 项的加法链：不嵌套，但树很深
    let long = format!("{}1", "1 + ".repeat(300));
    let errors = errs(&wrap(&long));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(first_diag(&wrap(&long)).message.contains("表达式过长"));
}

// ---------- 词法层的边角：消息里不放隐形字符，常见笔误给出针对性提示 ----------

#[test]
fn lexer_edge_cases_are_friendly() {
    // UTF-8 BOM 不是程序的一部分
    let src = "\u{feff}Table(\"a.csv\") as o\n  .collect()\n";
    assert!(errs(src).is_empty(), "{:?}", errs(src));

    // 浮点数按原样显示：`1.0` 不再被印成 `1`
    let src = "Table(\"a.csv\") as o\n  .take(1.0)\n  .collect()\n";
    assert!(
        first_diag(src).message.contains("`1.0`"),
        "{}",
        first_diag(src).message
    );

    // 全角括号、全角逗号：中文输入法下的常见笔误，肉眼几乎看不出差别
    for (src, needle) in [
        ("Table（\"a.csv\"） as o\n  .collect()\n", "全角括号"),
        ("Table(\"a.csv\") as o\n  .select(o.a，o.b)\n", "全角逗号"),
    ] {
        let d = first_diag(src);
        assert_eq!(d.code.as_str(), "LEX-001");
        assert!(d.hint.unwrap_or_default().contains(needle), "{src}");
    }

    // 看不见的字符：报码点，不往消息里塞一个隐形的东西
    let src = "Table(\"a.csv\") as o\u{0007}\n  .collect()\n";
    let d = first_diag(src);
    assert!(d.message.contains("U+0007"), "{}", d.message);
    let src = "Table(\"a.csv\")\u{00a0}as o\n  .collect()\n";
    let d = first_diag(src);
    assert!(d.message.contains("U+00A0"), "{}", d.message);
    assert!(d.hint.unwrap_or_default().contains("空白"), "{src}");

    // 行尾的反斜杠：字符串没闭合（LEX-002），不再连带报一个带着换行的 LEX-003
    let src = "Table(\"a.csv\") as o\n  .filter(o.a == \"abc\\\n  .collect()\n";
    assert_eq!(errs(src), [err_at(src, "LEX-002", "\"abc")]);

    // `.555…`（60 位）：一个 token 一个根因，只报一次
    let src = format!(
        "Table(\"a.csv\") as o\n  .take(.{})\n  .collect()\n",
        "5".repeat(60)
    );
    let errors = errs(&src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, "LEX-004");
}
