//! 静态成本模型（ch13，M3 资源配额）：最坏情况输出行数上界。
//!
//! 定位与边界（写在最前面，防止将来被误用）：
//! - 这是**政策层**，不是语言语义层——放在 driver 而不是 semantics，
//!   因为「要不要拒绝一个程序」取决于宿主的配额，不取决于语言。
//! - **为什么非图灵完备的语言做得到**：所有迭代绑定有限集合，输出规模
//!   与输入规模之间存在可计算的函数关系。这是「必然终止」与「有界资源」
//!   之间的桥——同一份有限性，先给了我们终止性，再给了我们成本上界。
//! - max_rows 是 lock 里的**承诺**（进主体哈希），不是统计值：静态预算的
//!   可信度等于宿主承诺的可信度。
//!
//! 推导规则（最坏情况，宁可高估不可低估）：
//! | 算子 | 输出上界 |
//! |---|---|
//! | 源 | lock.max_rows（None → 预算不可用） |
//! | filter / extend / select / distinct / sort | ≤ 输入 |
//! | take(n) | min(输入, n) |
//! | join | 左 × 右（**唯一的乘法算子**；无统计时不猜） |
//! | 分组聚合 | ≤ 输入（组数 ≤ 行数） |
//! | 全局聚合 | = 1（空输入恰好一行） |

use pebble_diagnostic::Span;
use pebble_hir::{Hir, HirNode, SourceKind, TransformKind};
use std::collections::BTreeMap;

/// 预算结果：上界 + 超限时的「肇事算子」span（用于诊断定位）。
#[derive(Clone, Debug)]
pub struct Budget {
    /// None = 有源未声明 max_rows，预算不可用（配额检查只能拒绝执行，不能放行）。
    pub bound: Option<u128>,
    /// 首个使上界超过配额的节点 span（配额检查失败时用于诊断定位）。
    pub culprit: Option<Span>,
}

/// 逐节点估计最坏情况输出行数。`max_rows_of` 按源身份查 lock 的行数上界。
pub fn estimate(
    hir: &Hir,
    max_rows_of: &dyn Fn(&SourceKind) -> Option<u64>,
    quota: Option<u64>,
) -> Budget {
    // 逐节点计算上界；join 是树形 DAG，右源可能被多路径共享——按节点缓存
    let mut memo: BTreeMap<u32, Option<u128>> = BTreeMap::new();
    let mut over_quota_span: Option<Span> = None;
    let root_bound = node_bound(
        hir,
        hir.root,
        max_rows_of,
        quota,
        &mut memo,
        &mut over_quota_span,
    );
    Budget {
        bound: root_bound,
        culprit: over_quota_span,
    }
}

fn node_bound(
    hir: &Hir,
    id: u32,
    max_rows_of: &dyn Fn(&SourceKind) -> Option<u64>,
    quota: Option<u64>,
    memo: &mut BTreeMap<u32, Option<u128>>,
    over_quota_span: &mut Option<Span>,
) -> Option<u128> {
    if let Some(cached) = memo.get(&id) {
        return *cached;
    }
    let node = &hir.nodes[id as usize];
    let bound: Option<u128> = match node {
        HirNode::Source { kind, .. } => max_rows_of(kind).map(|n| n as u128),
        HirNode::Transform { base, kind } => {
            let left = node_bound(
                hir,
                base.inputs[0],
                max_rows_of,
                quota,
                memo,
                over_quota_span,
            );
            match kind {
                TransformKind::Take { n } => left.map(|l| l.min((*n).max(0) as u128)),
                // join：唯一乘法算子。两个有限集的最坏情况连接是笛卡尔积；
                // 我们没有基数统计，不猜——宁可高估
                TransformKind::Join { .. } => {
                    let right = node_bound(
                        hir,
                        base.inputs[1],
                        max_rows_of,
                        quota,
                        memo,
                        over_quota_span,
                    );
                    match (left, right) {
                        (Some(l), Some(r)) => Some(l.saturating_mul(r)),
                        _ => None,
                    }
                }
                // 行级算子与排序/去重都不放大行数
                _ => left,
            }
        }
        HirNode::Aggregate {
            base, group_keys, ..
        } => {
            let input = node_bound(
                hir,
                base.inputs[0],
                max_rows_of,
                quota,
                memo,
                over_quota_span,
            );
            if group_keys.is_empty() {
                Some(1) // 全局聚合：空输入恰好一行
            } else {
                input.map(|i| i.min(u64::MAX as u128)) // 组数 ≤ 输入行数
            }
        }
        HirNode::Terminal { base, .. } => node_bound(
            hir,
            base.inputs[0],
            max_rows_of,
            quota,
            memo,
            over_quota_span,
        ),
    };
    // 记录首个超限节点（诊断定位：指出是哪个算子把预算打爆的）
    if let (Some(q), Some(b)) = (quota, bound) {
        if b > q as u128 && over_quota_span.is_none() {
            *over_quota_span = Some(node.base().span);
        }
    }
    memo.insert(id, bound);
    bound
}
