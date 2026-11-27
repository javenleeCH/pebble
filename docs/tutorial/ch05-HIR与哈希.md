# 第 5 章 HIR：字段身份、canonical 序列化与内容寻址

> 对应文件：`crates/pebble-hir/src/{lib.rs, canonical.rs, lock.rs}`；设计：`docs/draft/design/hir.md`。

## 本章目标

1. 理解「**名字不是身份**」：为什么 HIR 里的字段引用必须是 `FieldId` 而不是字符串；
2. 设计 canonical 序列化：同一语义计划 → 同一字节串 → 同一哈希；
3. 实现 Schema lock：把「编译针对的 Schema」变成不可变工件。

## 代码走读

### 名字不是身份

假设 join 后两表都有 `id`，输出 Schema 里有几个 `id`？答案：**两个，共享显示名但身份不同**。如果 HIR 按名字引用字段，两者立刻混淆——这正是 SEM-011 要在语义层拦下的坑。解决方案是把「身份」与「显示名」拆开：

```rust
pub struct FieldSchema {
    pub id: FieldId,          // 稳定语义身份
    pub name: String,          // 显示/绑定名（可以重复！）
    pub ty: Type,
    pub origin: FieldOrigin,  // 源字段 or 派生绑定
}
pub enum FieldOrigin {
    Source { source: SourceId, source_field: String },
    Derived { node: NodeId, binding: String },
}
```

`THirExpr::Field { field: FieldId, .. }` 只存 id。裸名解析、别名限定的全部规则都发生在语义层，lowering 之后**不再有任何名字查找**——后端永远不可能把 `u.id` 和 `o.id` 弄混，因为它们到达后端时已是两个不同的整数。

身份的另一个用途是**流敏收缩的作用域**（第 4 章）：`filter(f != null)` 后被改写类型的正是那个 FieldId 对应的字段，与任何同名者无关。

validate 时的关键不变量：**FieldId 的「创建」全局唯一，但允许跨节点复用**（passthrough）。`select` 未重命名的字段把 id 原样带下去——这正是「select as 只改显示名、不合并来源」的类型级体现：

```rust
// 非 source 节点：id 出现在输入 schema → 合法复用；否则必须是新创建
if input_ids.contains(&f.id) { continue; }
if !created.insert(f.id) { return Err(...); }
```

## canonical 序列化：可复现性的工程化

「审的那份计划 = 跑的那份计划」需要计划有**稳定指纹**。哈希对象不是「对象」，是**规范字节串**——规则全部写死在 `canonical.rs`：

**进去的**：节点拓扑序、输入边、字段（id/名/类型/来源）、表达式结构、字面量位模式（f64 用 IEEE bits）、终端选项、语义版本。
**剔除的**：Span、注释、空白、别名拼写、一切与语义无关之物。

后果立刻可测：

```rust
#[test]
fn format_comment_only_changes_do_not_change_hash() {
    let a = compile_ok(M0);
    let commented = M0.replace(".filter(", "// 审批要点：过滤已支付订单\n  .filter(");
    let b = compile_ok(&commented);
    assert_eq!(hir_hash(&a), hir_hash(&b), "注释/空白不得影响 canonical hash");
}
```

反例同样在测试里：阈值 `10000 → 9999`，hash 必变。**格式变化恒等、语义变化恒异**——这两条不成立，整个证据链（第 7 章）就没有地基。

实现上是手写的 `ByteWriter`：每个结构一个 tag 字节 + 长度前缀，没有 serde 的字段顺序/浮点格式悬念。有人会问「为什么不直接 serde_json？」——因为 JSON 的数字格式（浮点表示）和字段顺序依赖库行为，而**内容寻址要求字节级自证**；手写 200 行换来完全的控制。

## Schema lock：编译的输入也是工件

`Table("orders.csv")` 的 Schema 从哪来？如果编译时读文件元数据，那么「编译针对的 Schema」就随文件漂移——审过的计划可能对着不同的输入跑。Schema lock 把它钉死：

```json
{
  "lock_version": 1,
  "source": { "kind": "table", "locator": "tests/data/orders.csv" },
  "fields": [
    { "name": "order_id", "type": "i64", "optional": false },
    ...
  ],
  "schema_hash": "sha256:..."
}
```

三条硬规则：

1. **编译只信任 lock**；`SCH-001`（无 lock）与 `SCH-005`（hash 不一致）都是编译错误；
2. lock 自身带哈希：篡改字段列表 → `validate()` 失败；
3. **路径是定位符，不是身份**——数据内容哈希在 build 时记录、run 时复核（第 7 章的 TOCTOU）。

生成 lock 的体验就是 CLI 的一部分：

```powershell
cargo run -p pebble-driver -- lock table tests/data/orders.csv order_id:i64 status:string amount:i64? score:f64 customer_id:string --out tests/locks/orders_csv.lock.json
```

类型集合故意只有四个（i64/f64/bool/string）+ `?` 可选标记：类型系统的每个类型都必须在 lock、Type 枚举、SQL 映射、canonical 序列化四处保持一致——**类型集合越小，四处漂移的面积越小**。

## DAG 而不是链

M0 语法是线性的，但 HIR 仍是 `nodes + inputs[]` 的 DAG：join 的右表是独立 Source 节点，`Hir.root` 指向唯一终端。为什么不做成 `Vec<Op>` 链表？因为：

- 「单入口单出口」红线要求 HIR 层有**唯一根**的不变量（validate 强制）；
- 未来 `let` 子流水线、分支合并进来时，语法是链、计划是图——底层结构现在就位，语法扩展不用动 HIR。

## 跑起来

```powershell
cargo test -p pebble-hir
# 看一眼真实计划的内貌：
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks
Get-Content examples/target/pebble/00_m0_revenue/plan.txt   # 人类可读 dump
```

给 HIR 加新东西时有一条铁律：旧程序的哈希不能变。第 11 章加表达式 `Cast` 的做法是「只加不改」——新变体用新标签 `V`，既有编码一个字节不动——并用一条写死了样例哈希的测试钉住它，见[第 11 章](ch11-having与cast.md)。

## 练习

1. **反序列化**：`canonical.rs` 只有「写」。实现读（canonical bytes → Hir），加 roundtrip 测试：`hir_hash(deserialize(serialize(h))) == hir_hash(h)`。这是 M2「verifier 独立化」的前置件。
2. **破坏实验**：手动改 `ByteWriter::f64` 为 `to_string()` 序列化浮点，跑确定性测试，观察哪一步先失败；写一段分析：为什么浮点的字符串表示不适合内容寻址（提示：NaN、区域设置、`{:?}` 的最短表示）。
3. **想一个 canonical 泄漏**：找出当前序列化里你怀疑「该进去没进去」的字段（提示：`Take` 的 n？`Sort` 的键顺序？），构造两个程序证明它确实在哈希里（改了它 hash 必变）。
