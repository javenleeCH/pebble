# pebble-latex

职责：从同一 typed canonical HIR 导出最小单向关系代数/LaTeX 审计视图，正确区分 `extend`=ε、`select`=π，并保留聚合及 `sort/take` 的顺序语义说明。

边界：LaTeX 是 HIR 的可读视图，不是执行正确性证明，也不读取 BackendArtifact 反推逻辑。保真往返与签名证据工件后置。

状态：目录骨架，M0 仅实现最小单向导出。
