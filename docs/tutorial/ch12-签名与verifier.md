# 第 12 章 证据签名与独立 verifier（M2）

> 对应文件：`crates/pebble-hir/src/sign.rs`、`crates/pebble-hir/src/canonical.rs`（反序列化部分）、
> `crates/pebble-verifier/`（新 crate）、`crates/pebble-driver/src/{lib,main}.rs`（keygen/sign 子命令）。
> 前置阅读：第 7 章「诚实边界」——本章就是把那一节里承认的两个洞补上。

## 本章目标

1. 堵住 M1 证据链的两个洞：**自洽伪造**与**既当运动员又当裁判**；
2. 实现 ed25519 manifest 签名（审批 = 密码学行为）与 canonical HIR 反序列化 roundtrip；
3. 建立独立 verifier（`pebble-verifier`）：只凭工件 + 外部公钥下结论，不加载编译器本体。

## 为什么需要签名：M1 的攻击推演

第 7 章的 verify 是三次相等比较——全是**内部一致性**检查。推演一个攻击：

```text
1. 篡改 plan.sql（sum → sum-1）
2. 重新计算哈希，生成一份新的 manifest.json
3. 跑一次，生成配套的新 receipt.json
→ verify()：三个哈希全部匹配 —— APPROVED ✅（攻击成功）
```

哈希防「改了没跟着改」，防不了「全都重新生成」。根因：**证据文件谁都能造**，
哈希绑内容不绑身份。签名把「审批」从「人记下一个哈希值」变成「人用私钥签了一个名」——
攻击者重造 manifest 后拿不出合法签名，verify 用公钥一验就穿。

**为什么公钥不入 manifest**：公钥写在被验对象里等于没验（攻击者换 manifest 时连公钥一起换）。
公钥必须来自可信的外部渠道——`--pubkey` 参数或密钥文件，这是 verifier 的输入，不是被验的数据。

## 为什么需要独立 verifier

第二个洞更隐蔽：M1 的 verify() 跑在 `pebble` 这一个二进制里——**造证据的程序和验证据的
程序是同一个**。编译器被篡改时，它既造假证据又给自己盖章。

解法是用另一条代码路径核验。`pebble-verifier` 的独立性声明写在 crate 文档第一段：

- 运行时依赖面 = `pebble-hir`（数据模型 + canonical 解析 + 签名原语）+ JSON，**不加载**
  parser / semantics / codegen / driver——验证编译器产物，不需要（也不应当）信任编译器主路径；
- 只读工件、不执行流水线、不写任何文件。

诚实边界（写进 crate 文档，不许省）：canonical 解析与签名原语仍与编译器共享 `pebble-hir`。
「完全独立的第二套实现」是未来的加固项——当前独立性指**独立于编译管线**。

## 代码走读

### 1）签名：签主体，不签自身；签名后哈希必变

`sign.rs` 的两个关键决定：

```rust
fn manifest_body_bytes(manifest: &ExecutionManifest) -> Vec<u8> {
    let mut body = manifest.clone();
    body.signature = String::new();      // 签名覆盖主体（不含自身）
    body.canonical_bytes()
}
```

- **签名覆盖主体、不覆盖自身**——否则鸡生蛋问题（签名里含签名）；
- `signature` 字段用 `#[serde(default, skip_serializing_if = "String::is_empty")]`——
  **未签名 manifest 的字节与 M0/v0.1.1 完全一致**，旧 receipt 对旧 manifest 的核对
  不受本次升级影响。向后兼容不是碰运气，是序列化选择做出来的。

`manifest_hash` 仍然覆盖**含签名的整体**（`compute_hash` 不动）——所以：

```text
build → manifest_hash = H₁
sign  → manifest_hash = H₂ ≠ H₁（签名字段进来了）
```

由此得出**顺序契约**：`build → sign → run → verify`。签名必须发生在执行之前——
receipt 绑定的是「run 时读到的 manifest」，签完再跑，回执绑的才是已审版本。
签完再跑会怎样？receipt 记录的还是 H₁，verify 重算 manifest 得 H₂ → 核对失败。
这不是 bug，是「改了没跟着改」被正确拦下：**语义上那就是一份新 manifest**。

密钥文件（`pebble keygen`）自带一致性自检：从 seed 派生的公钥必须与文件里的
`public` 一致，防手抄错。

### 2）canonical HIR 反序列化：格式规范只有一个归属

`canonical.rs` 此前只会「写」。verifier 需要「读」——否则核对 `hir_hash` 就得
重新跑一遍编译器，独立性破功。反序列化器与写入器**同文件**维护：格式的规范
只有一个归属，写、读、改格式三处必须同步。

三个值得学的实现细节：

```rust
fn count(&mut self, what: &str) -> Result<usize, String> {
    let n = self.u32()? as usize;
    if n > self.bytes.len() {          // 恶意字节防御
        return Err(format!("{} 数量 {} 超过剩余字节，拒绝解析", what, n));
    }
    Ok(n)
}
```

1. **计数防御**：长度前缀指向敌对输入（verifier 读的是可能被篡改的文件），
   疯狂的 count 不能让它预分配疯载数组的内存——上限为剩余字节数；
2. **Span 占位**：canonical 编码不含 Span（第 5 章规则），反序列化一律
   `Span::new(0,0)` 占位——哈希不覆盖 Span，占位不影响 roundtrip 哈希；
3. **严格尾随检查**：解析完 `pos != bytes.len()` 即拒绝——不接受尾随垃圾。

roundtrip 测试的三连断言（`pebble-hir` 单测 + verifier 集成测试双保险）：

```rust
assert_eq!(canonical_bytes(&back), bytes);          // 逐字节一致
assert_eq!(roundtrip_hash(&bytes).unwrap(), hir_hash(&h));
assert!(back.validate().is_ok());                  // 结构不变量
```

### 3）verifier 的六项检查：失败必须可定位

`verify_build_dir` 产出的报告是六项独立检查，**每项单独红绿**——篡改 plan.sql
只亮「工件一致性」，签名检查依旧绿。价值在于审计员能立刻说出**哪一环坏了**：

| # | 检查 | 挡什么 |
|---|---|---|
| 1 | 审批签名（ed25519） | 自洽伪造（重造整套 manifest/receipt） |
| 2 | 工件一致性（plan.sql） | 执行工件被偷换 |
| 3 | 计划一致性（HIR roundtrip） | canonical.hir 被篡改/损坏 |
| 4 | 回执绑定（receipt ↔ manifest） | 跑的不是审的那份 |
| 5 | 数据快照复核（TOCTOU） | build 后数据被换 |
| 6 | HIR 结构不变量（validate） | 结构性破坏 |

一个容易想错的设计点：receipt 的**执行状态**（Ok/Failed）不是核验失败项。
一次被 RUN-002 拒绝的执行同样是合法证据——「数据被篡改且被拒」正是审计想要
看到的结果。报告如实转述状态，但不据此判红绿灯。

### 4）测试：攻防矩阵

`pebble-verifier/tests/verifier.rs` 把第 7 章的篡改实验全部自动化，并新增签名相关攻击。
开发中还踩了一个真实的坑（写进了测试注释）：最初密钥文件写在一个**共享路径**，
并行测试互相覆盖导致「EOF while parsing」——**共享可变路径 + 并行 = 空读竞争**，
修法是每个测试一个沙箱目录。数据篡改测试同样用私有数据副本，绝不碰共享 fixture。

## 跑起来

```powershell
# 完整签批流程（五幕）
cargo run -p pebble-driver -- keygen --out approver.json          # 0. 生成审批密钥（seed 保密！）
cargo run -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks
cargo run -p pebble-driver -- sign examples/target/pebble/00_m0_revenue/manifest.json --key approver.json
cargo run -p pebble-driver -- run   examples/target/pebble/00_m0_revenue
cargo run -p pebble-driver -- verify examples/target/pebble/00_m0_revenue/manifest.json examples/target/pebble/00_m0_revenue/receipt.json --pubkey approver.json

# 独立核验（审计员视角：不需要 pebble 编译器）
cargo run -p pebble-verifier --   examples/target/pebble/00_m0_revenue --pubkey approver.json

# 看攻击效果：篡改 manifest 任一主体字段再核验 → 签名检查 FAIL，其余仍绿
cargo test -p pebble-verifier
```

预期：verifier 输出六行 `[ok]` + `EVIDENCE VERIFIED`。

## 诚实边界（M2 之后仍然不解决的）

- **私钥管理**：seed 泄露即任何人都能签出「合法审批」；本章只提供原语，不提供
  密钥轮换/HSM/门限——那是部署侧的事；
- **签名前的欺骗**：build 机器在审批人查看工件之前被入侵，仍可能骗过审批人
  （缓解：人审 LaTeX/SQL 视图后才签名，签名发生在审批侧机器）；
- **执行侧自证性**：receipt 说「我运行了 X」，签名后变成「审批者批准过 X」——
  但「X 真的被运行过」仍是执行环境的自我声明。M4 原生后端时代这会更严重，
  届时可能需要执行侧也持密钥签回执（executor signature），已列入 M4 议题；
- DuckDB 仍在执行信任基内。

## 练习

1. **攻击推演**：构造一个「攻击者能改 manifest、receipt、plan.sql、数据文件中的任意
   一个，但不能拿到私钥」的攻防矩阵，逐格填「哪个检查项亮红」。有没有任何一格能穿透？
   （提示：先想 `engine_version_requirement` 空时的引擎版本。）
2. **执行侧签名**：给 receipt 也加签名（执行环境持自己的密钥），verify 侧核验。
   想清楚它与审批签名的**威胁模型差异**：一个防「假审批」，一个防「假执行」。
3. **公钥分发**：本章公钥靠 `--pubkey` 手工传入。设计一个最小可信分发方案
   （提示：公钥指纹 + 带外核对；对比 SSH 的 known_hosts 与 TOFU 模型）。
4. **完全独立的第二实现**：把 canonical 解析在 verifier 里重新实现一遍（不 import
   pebble-hir），对比两边对同一 canonical.hir 算出的哈希。评估：值得吗？
   差分哈希（两套实现必须一致）和单点信任（一套实现可能有同一个 bug）之间怎么权衡？
