# 附录 A CI 与发布门禁

> 对应文件：`.github/workflows/{ci.yml, release.yml}`、`crates/pebble-driver/tests/docs_gate.rs`。
> 参考模式：uniEtract 仓库的 CI/Release 流水线，按 Pebble 的形态适配。

## 本章目标

1. 理解「门禁」的意义：把纪律变成机器执行，而不是靠自觉；
2. 了解 Pebble 的六道 CI 门禁与发布流水线；
3. 学会在本地跑完 CI 会跑的一切——**提交前自己就是 CI**。

## 为什么要门禁

教程驱动开发的承诺是「每章结束时的代码 `cargo test` 全绿、读者可 checkout 跟做」。承诺若只靠自觉，会在某个赶进度的深夜破掉。门禁的本质是：**把流程纪律翻译成 CI 里的一个 step，违反即红灯**。uniEtract 的模式（MSRV 锁定构建 + 双 OS 矩阵 + `-D warnings` + 冒烟）证明了这套东西在单人项目同样有价值——它的读者就是未来的你。

## 代码走读

### ci.yml 的骨架

```yaml
jobs:
  msrv:                      # 门禁 1：Rust 1.85.0 + --locked
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85.0
      - run: cargo build --locked --all-targets
      - run: cargo test --locked --all-targets

  verify:                    # 门禁 2~6：双 OS 矩阵
    strategy:
      matrix: { os: [ubuntu-latest, windows-latest] }
    steps:
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: cargo test --workspace --all-targets --locked
      # ↓ 下载 DuckDB CLI 后才真正跑端到端（本地无 duckdb 时自动跳过）
      - run: cargo test --workspace --test evidence_chain --locked
      - run: cargo test -p pebble-driver --test docs_gate --locked
      # ↓ README 快速上手的四幕命令原样执行
      - run: cargo run -q -p pebble-driver -- build examples/00_m0_revenue.pebble --locks tests/locks
      ...
```

两个 job 的分工是刻意的：`msrv` 只做「最低版本能不能构建测试」这一件事（快速失败）；`verify` 承担所有质量维度并跑矩阵。DuckDB 的下载按 OS 分两个 step（Linux 用 `unzip`，Windows 用 pwsh `Expand-Archive`），都落位到 `tools/`——与 `resolve_duckdb`（第 7 章）的查找顺序 PATH → `tools/` 对齐。

### release.yml 的关键表达式

```yaml
env:
  RELEASE_TAG: ${{ github.event_name == 'workflow_dispatch' && inputs.tag || github.ref_name }}
```

同一个工作流支持两种触发：push tag（用 `github.ref_name`）与手动 dispatch（用 `inputs.tag`）——上面的三元表达式统一了两者。版本一致性检查放在独立的首 job：失败时零构建成本，符合「越便宜的检查越早跑」。

## 六道 CI 门禁（ci.yml）

| # | 门禁 | 命令 | 防住什么 |
|---|---|---|---|
| 1 | MSRV 锁定构建 | `cargo build/test --locked`（Rust 1.85.0） | Cargo.lock 漂移、悄悄用上新 API |
| 2 | 格式 | `cargo fmt --all --check` | 风格漂移污染 diff |
| 3 | Clippy | `cargo clippy --all-targets --locked -- -D warnings` | 隐藏的坏味道（本章末讲 MSRV 配置的作用） |
| 4 | 全量测试 | `cargo test --workspace --all-targets --locked` | 语义回归（20 组，含证据链、解析器恢复矩阵、诊断契约） |
| 5 | **DuckDB 端到端** | 下载 duckdb CLI → `--test evidence_chain` → 四幕冒烟 | 证据链在真实引擎上的回归 |
| 6 | **教程契约** | `--test docs_gate` | 教程与实现失配（下一节） |

第 5 道值得展开：本仓库的端到端测试在没有 duckdb 时会自动跳过——本地绿不等于 CI 绿，反之亦然。所以 CI 显式下载 DuckDB 1.4.1 再跑一遍 `evidence_chain` 与 README 快速上手的四幕命令，**让「快速上手文档」本身成为被测对象**（文档里的命令失效，CI 就红灯）。

## 教程契约门禁：把承诺写成测试

`docs_gate.rs` 是本项目最有性格的门禁。它断言：

1. `docs/tutorial/` 的章节清单与 `CHAPTERS` 常量一致（新增章节必须同时改地图和测试——**双处同步，漏一处即红灯**）；
2. 每章必含五段（本章目标/为什么/代码走读/跑起来/练习）；例外：ch00 立项章无代码可走读，**豁免本身写在受测契约里**；
3. 教程引用的 30+ 个实现文件真实存在（教程提过的 crate 文件被删/改名，红灯）；
4. 每个 `.pebble` 样例引用的数据源在 `tests/locks` 有对应 lock（提前拦下 SCH-001）；
5. 教程里贴的**输出**与实现逐字一致：第 8、9 章「跑起来」的报告全文，要与 `cmd_build` 对同一文件的真实结果相同（第 8 章加入，第 9 章起按章节表登记）。教程里连一段过期的输出都不许有；
6. 教程里引用的**代码**与实现逐字一致：第 8、9 章的每个 rust 代码块都必须逐字出现在源码里（可以用含「（节选）」的行断开，断开后每一段仍须逐字一致——所以「节选」只能用来省略，不能用来改写）。实现一改、教程没跟上，这里就红灯。

写这个测试时自己也踩了坑：先按「五个关键词全文匹配」实现，立刻发现 ch00 没有代码、ch06 缺「为什么」段——**门禁先于人类发现了教程的真实缺陷**，这正是它的价值。

## Clippy 与 MSRV 的一个坑

Clippy 在新版工具链上会建议 `if let A && B`（let-chain，Rust 1.88 才稳定）。如果照做，MSRV 1.85 的构建就挂。解法不是无视 lint，而是让 clippy 知道 MSRV：

```toml
# Cargo.toml [workspace.package]
rust-version = "1.85"     # 各成员 rust-version.workspace = true 继承
```

clippy 会读取 `rust-version` 作为 msrv 配置，不再建议你的最低版本写不了的写法。**门禁链是环环相扣的**：MSRV job 守住 `rust-version` 的真实性，`rust-version` 约束 clippy 的建议。

### 翻车实录：本地绿 ≠ CI 绿

第一次跑 CI 就翻车了，而且是同时挂 ubuntu 和 windows 的 Clippy step。原因值得记一辈子：

- **本地开发用 stable（1.100）的 clippy，干净通过**；
- CI pin 在 **1.85.0** 的 clippy，报了四个错：三处 `impl<'a> Parser<'a>` 的显式生命周期（新 clippy 已不再报的 lint）、一处 `map(|b| format!(...)).collect()`（`format_collect`，版本间行为漂移）。

教训：**门禁用的工具链版本就是法律，本地验证必须在同一版本上做**。修复后的本地预演命令因此升级为双工具链：

```powershell
rustup toolchain install 1.85.0 --profile minimal --component clippy
cargo +1.85.0 clippy --workspace --all-targets --locked -- -D warnings   # CI 视角
cargo clippy --workspace --all-targets --locked -- -D warnings          # stable 视角
cargo +1.85.0 test --workspace --locked                                 # MSRV 可测
```

四个修复（`impl Parser<'_>` ×3、fold 替换 map+collect）在新旧两个 clippy 下都干净——**门禁修复必须同时过两个版本的 lint，只过一边就会在下一次翻车**。

### 第二次翻车：行尾也是环境的一部分

第 8 章推上去之后又红了一次，这回只有 `Verify (windows-latest)`：其余测试全过，唯独「教程输出逐字比对」失败——比对面多出了一个空行。

原因不在代码逻辑。GitHub 的 Windows runner 以 `core.autocrlf=true` 检出，教程文件在那里是 CRLF，而我本地那份是 LF。门禁从教程里抽取输出块时，用 `trim_start_matches('\n')` 剥掉围栏后紧跟的换行；CRLF 下那里是 `\r\n`，开头的 `\r` 没被剥掉，之后再统一行尾，就多出了一个空行。读一读那段代码完全看不出问题——**它只在另一种行尾下才是错的**。

两次翻车其实是同一条教训的两种形态：**门禁除了比对你的代码，还在比对你的环境**——第一次是工具链版本，这一次是行尾。修法也同构：把环境的差异收拢在一处（第一次是 `rust-version`，这一次是门禁的 `read()` 统一把 CRLF 换成 LF），不让它散落到各个比较里。

预防的办法也只有一个：**在本地重演那个环境**。附录开头的预演命令覆盖了工具链；行尾这一次之后，清单上多了一项——用一次**全新的检出去跑**，而不是在改了又改的工作区里跑：

```powershell
git worktree add --detach target/ci-sim HEAD     # 全新检出；autocrlf=true 下全部文件是 CRLF
cd target/ci-sim
cargo +1.85.0 test --workspace --all-targets --locked
cd ../..; git worktree remove --force target/ci-sim
```

排查这类问题时，第一步永远是**先在本地复现**（把教程临时转成 CRLF，同一个测试立刻就红了），而不是盯着代码猜。复现了，修复才算验证过。

## 发布流水线（release.yml）

tag `v*` 触发，三步走：

```text
version-check:  tag v0.1.0 ↔ Cargo.toml [workspace.package].version 必须一致
build 矩阵:     linux-x86_64 / windows-msvc / macos-arm64，cargo build --release --locked
publish:        下载全部工件 → SHA256SUMS → GitHub Release（fail_on_unmatched_files）
```

两个设计决定：

1. **DuckDB CLI 不随包分发**：34MB 的引擎二进制不属于编译器发布物；包内附 `RUNTIME-DEPS.md` 说明下载途径。缺失时 build/verify 照常工作，run 得到 RUN-003 回执——这是第 7 章「失败也是证据」的延续。
2. **校验和进 Release 附件**（`sha256sum -c SHA256SUMS`）：证据链的信任链从「发布物没被篡改」开始，仓库内的 manifest/receipt 机制管运行期，SHA256SUMS 管分发期。

## 跑起来

```powershell
# 本地预演全部 CI 门禁（除了 OS 矩阵的多平台部分）：
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p pebble-driver --test docs_gate --locked
cargo test --workspace --test evidence_chain --locked     # 需要 tools/duckdb

# 预演发布：
git tag v0.1.0 && git push origin v0.1.0
```

## 练习

1. **加一个门禁**：uniEtract 有「测试计数与文档一致」的检查（check_test_count.py）。给 Pebble 写一个 Rust 版：统计 `cargo test` 可发现的测试数量，与 `tests/README.md` 声明的数量比对。先想清楚：数量漂移是 bug 还是正常演进？门槛该怎么定才不误报（提示：区间或下限，而非精确值）。
2. **CI 矩阵扩展**：给 `verify` job 加 `macos-latest`（DuckDB 下载 URL 用 duckdb_cli-osx-universal.zip）。观察：`resolve_duckdb` 的路径逻辑需要改动吗？
3. **发布演练**：不打 tag，用 `workflow_dispatch` 手动触发 release 工作流，验证 version-check 在 tag 缺失时的行为，再读一遍 `RELEASE_TAG` 环境变量的表达式语义（`github.event_name == 'workflow_dispatch' && inputs.tag || github.ref_name`）。
