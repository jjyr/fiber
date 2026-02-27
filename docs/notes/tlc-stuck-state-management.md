# TLS/TLC stuck 状态管理对照分析（Fiber vs LND）

日期：2026-02-26  
范围：`crates/fiber-lib/src/fiber/channel.rs`、`crates/fiber-lib/src/fiber/payment.rs`、`crates/fiber-lib/src/fiber/network.rs` 与 `lnd` 的 `htlcswitch/link.go`、`lnwallet/channel.go`、`routing/router.go`。

## 结论（先给结论）

Fiber 在 commit 更新链路上已经有“等待 ack / 重放 / 重试”机制，但缺少一层“超时后的强制退出与回收”策略；LND 有明确的超时失败、重建与重放闭环，所以在链路异常时不太会进入长期 `stuck`。  

建议的优先修复方向是三段式：
1. 将 `CheckActiveChannel` 的超时从“仅记录/停 actor”升级为“可重放/可回收的失效处理”；
2. 将重启后的恢复路径与重试队列做“确定性收敛”（参考 LND 的 `updateLog` 与 `CommitDiff` 重建）；
3. 增加支付层的 `stale attempt` 兜底，避免 inflight 支付长期悬挂。

## 1. Fiber 当前机制（按责任点）

### 1.1 超时和等待状态的表达

- `PEER_CHANNEL_RESPONSE_TIMEOUT` 在 30s（测试/bench 为 10s）定义了等待 Ack 的上限；等待时通过 `set_waiting_ack(true)` 记录时间戳并触发 `CheckActiveChannel` 检查事件。  
  - `crates/fiber-lib/src/fiber/channel.rs:122-126`  
  - `crates/fiber-lib/src/fiber/channel.rs:4707-4717`  

### 1.2 在 `ChannelReady` 下的重建（reestablish）

- `ReestablishChannel` 里有双向/单向 owed 的分支：有 `CommitDiff` 时走确定性重放，无 `CommitDiff` 时走旧 `resend_tlcs_on_reestablish`。  
  - `crates/fiber-lib/src/fiber/channel.rs:7345-7420`  
  - `crates/fiber-lib/src/fiber/channel.rs:7437-7450`  
  - `crates/fiber-lib/src/fiber/channel.rs:7470-7480`  

- 处理重建后会把 `RevokeAndAck` 与 `CommitmentSigned` 顺序交错发送，并可能在发送提交窗口内延迟处理远端 TLC 更新（`defer_peer_tlc_updates`）。  
  - `crates/fiber-lib/src/fiber/channel.rs:7437-7449`  
  - `crates/fiber-lib/src/fiber/channel.rs:7438-7439`

### 1.3 操作阻塞逻辑

- `check_for_tlc_update` 在 TLC 命令到来时，若 `is_waiting_tlc_ack()` 或 `reestablishing`，会退回 `WaitingTlcAck`，用于阻塞新入队 TLC 命令。  
  - `crates/fiber-lib/src/fiber/channel.rs:6416-6425`  

- `apply_retryable_tlc_operations` 在等待 ACK 时会直接阻塞并继续排队，这防止了错误顺序，但没有显示“永久无效后自动清理”路径。  
  - `crates/fiber-lib/src/fiber/channel.rs:2076-2088`  
  - `crates/fiber-lib/src/fiber/channel.rs:2092-2104`  

### 1.4 CheckActiveChannel 当前行为

- `ChannelEvent::CheckActiveChannel` 只在 `peer_does_not_reply_ack_in_time()` 且未关闭时触发处理，当前路径上更像是“检测入口”；在该文件中看起来没有看到等价于“直接驱动重放/失败/重试队列重建”的闭环分支。  
  - `crates/fiber-lib/src/fiber/channel.rs:2477-2481`  
  - `crates/fiber-lib/src/fiber/channel.rs:4691-4697`

### 1.5 支付层是否有“stale attempt”兜底

- `PaymentActor` 只有周期日志型 `CheckPaymentStatus`；它在状态未完成时仅记录日志，不会对旧 attempt 做自动失败或回收。  
  - `crates/fiber-lib/src/fiber/payment.rs:1295-1298`  
  - `crates/fiber-lib/src/fiber/payment.rs:1560-1579`

## 2. LND 为什么更不容易 stuck

### 2.1 Commit/dance 的超时强制失败（首屏护栏）

- `htlcswitch` 用 `PendingCommitTicker` 检测“本端发起但对端未完成 revoke 的 dance”；
- 触发后直接 `failf(ErrRemoteUnresponsive, LinkFailureDisconnect)`，会断开链路，避免无限等待。  
  - `lnd/htlcswitch/link.go:193-196`  
  - `lnd/htlcswitch/link.go:1388-1394`

### 2.2 消息与内存包的收敛（mailbox + 重放）

- 重连时会 `ackDownStreamPackets` 清理已提交到承诺中的包，随后 `reset` mailbox，最后 `resolveFwdPkgs` 重放可继续处理的 forwarding package。  
  - `lnd/htlcswitch/link.go:1892-1900`  
  - `lnd/htlcswitch/link.go:1901-1940`  
  - `lnd/htlcswitch/link.go:3931-3952`

### 2.3 `lnwallet` 的可恢复提交模型

- LND 在持久化状态里保留提交/更新日志，并提供 `restoreCommitState`、`logUpdateToPayDesc`、`localLogUpdateToPayDesc`、`remoteLogUpdateToPayDesc` 等恢复路径，用于把未完整提交的更新重新映射回内存状态。  
  - `lnd/lnwallet/channel.go:1497-1505`  
  - `lnd/lnwallet/channel.go:1064-1069`  
  - `lnd/lnwallet/channel.go:1222-1227`  
  - `lnd/lnwallet/channel.go:1326-1333`

### 2.4 连接恢复时的协商输出

- `ProcessChanSyncMsg` 明确返回 `CommitSig+Updates` 或 `RevokeAndAck` 两类修复路径，并配套输出需要清理的 opened/closed circuits。  
  - `lnd/lnwallet/channel.go:4327-4334`  
  - `lnd/lnwallet/channel.go:4349-4354`  

### 2.5 支付层 stale attempt 终结

- 路由器存在 `failStaleAttempt`：对不再可重试的 inflight attempt 进行主动失败，避免“结果丢失后永远卡住”。  
  - `lnd/routing/router.go:1515-1538`  

## 3. 对 Fiber 的直接结论（推断）

从对比上看，Fiber 现有实现更接近“状态保存后盲等待”，而不是 LND 的“保存后可判定失败+自动回收+可重放恢复”。这在以下场景可能导致 `tlc stuck`：

1. `waiting_ack` 长期为真但无法清理（例如重启后 nonce/ack 信息不完整）；
2. `retryable_tlc_operations` 在重试窗口不触发时持续堆积；
3. 交易方重建后没有像 LND 那样把所有未落盘/未 ACK 的链路操作统一打通到状态收敛流程；
4. 支付层缺少 inflight 任务“失活后失败”动作，导致上层看起来长期 pending。

## 4. 建议实施清单（按优先级）

1. **高优先级：把 `CheckActiveChannel` 做成可恢复失败路径**  
   - 在超时后标记为需要重连/重置，并触发与重建逻辑耦合的 fallback（至少要能让 channel actor 从单一 `wait` 状态退出），而不是只记录。  
   - 对应点：`crates/fiber-lib/src/fiber/channel.rs:2477-2481`、`463x-4717`。

2. **高优先级：统一 `TlcAck` 超时模型**  
   - `is_waiting_tlc_ack` 的判定建议只围绕“是否有未确认承诺差异”而非依赖多个可丢失字段；若某些状态字段丢失，启用保守失败/重新握手恢复。  
   - 对应点：`crates/fiber-lib/src/fiber/channel.rs:6424+`（以及与 6416 的判定路径）。

3. **中优先级：重建时引入更强日志恢复能力**  
   - 参考 LND 的 `ProcessChanSyncMsg` 风格，明确返回「需要重放」「只需 Revoke」「重建清算」等模式；把 `CommitDiff` + `retryable` 的分支统一成单一收敛函数。  
   - 对应点：`crates/fiber-lib/src/fiber/channel.rs:7345-7575`、`crates/fiber-lib/src/fiber/network.rs:3731-3775`（channel 重启路径触发点）。

4. **中优先级：给 `CheckPaymentStatus` 加上 stale-attempt 兜底**  
   - 若 Payment actor 在多次检查中仍无链上/下游结果，按策略 fail 掉无效 attempt，避免上层 inflight 永久驻留。  
   - 对应点：`crates/fiber-lib/src/fiber/payment.rs:1560-1579`，对标 `lnd/routing/router.go:1515-1614`。

5. **低优先级：测试建议（避免回归）**  
   - 增加重放压力：`waiting_ack` 长期保留 + 节点重启 + 双向未完成更新复现；
   - 增加超时后恢复用例：检查 `waiting_ack` 会退回可重试/失败状态；
   - 检查重启后 `retryable_tlc_operations` 不再无界增长。

## 5. 参考文件

- `crates/fiber-lib/src/fiber/channel.rs`
- `crates/fiber-lib/src/fiber/network.rs`
- `crates/fiber-lib/src/fiber/payment.rs`
- `lnd/htlcswitch/link.go`
- `lnd/lnwallet/channel.go`
- `lnd/routing/router.go`

## 6. 已按此文档实施（2026-02-26）

- `crates/fiber-lib/src/fiber/channel.rs`
  - `ChannelEvent::CheckActiveChannel` 改为超时收敛路径：打详细 ack 日志、清空 `waiting_ack`（清理 `waiting_peer_response`）、尝试触发 `apply_retryable_tlc_operations` 后继续走现有 shutdown fallback，避免长时间卡在等待 ack 状态。
  - `ChannelActorState::is_waiting_tlc_ack` 改为只依赖 `tlc_state.waiting_ack`，移除对 `remote_revocation_nonce_for_send/verify` 缺失字段的硬阻塞依赖，降低重启后长期挂死概率。
- `crates/fiber-lib/src/fiber/payment.rs`
  - 新增 inflight 兜底阈值 `PAYMENT_STALE_ATTEMPT_TIMEOUT_MS`（120s）。
  - 周期检查 `handle_check_payment_status` 中增加 stale attempt 检测与失败收敛（`TemporaryNodeFailure`、`retryable=false`），并在清理后写回 `PaymentSession`，防止 inflight attempt 永久驻留。
  - 新增单元测试覆盖 `stale_inflight_attempt_ids` 的边界识别逻辑（`inflight`/非 `inflight` 与超时边界）。

### 回归建议

- 先跑单元回归：`cargo test -p fiber-lib test_stale_inflight_attempt_ids test_no_stale_inflight_attempt_ids_before_timeout`
- 再跑高层场景：`do_test_add_tlc_waiting_ack` 与支付重启场景，重点关注 `waiting_ack` 从 `true` 到收敛后的重试队列行为。

### 2026-02-27 执行更新（Task4/5）进度

- 已完成：  
  - `apply_retryable_tlc_operations` 在 `waiting_ack=true` 时先清理可落地的 `deferred_peer_tlc_updates`，然后基于 `pending_replay_updates` / `deferred_peer_tlc_updates` 判定是否仍有 mailbox backlog；若 backlog 已清空则解除 `waiting_ack` 并继续重试流。  
  - `ChannelEvent::CheckActiveChannel` 超时分支新增 `reconcile_peer_tlc_mailbox_on_timeout`：`mem::take(pending_replay_updates)` + `collect_replay_peer_tlc_updates` + `send_replay_peer_tlc_updates` + `prune_deferred_peer_tlc_updates`，并保留 shutdown fallback。
  - `ChannelActorState` 增加 `has_replay_backlog`，用于统一重放 backlog 判断。
  - `crates/fiber-lib/src/fiber/tests/settle_tlc_set_command_tests.rs` 新增 `collect_replay_peer_tlc_updates` 的单测，覆盖：
    - 已提交/已确认的 TLC 不会被重复回放；
    - pending/replay 重放列表保留“同一条目最后一次更新”。

- 已补齐：  
  - 与 `Task7` 的 `ChannelActor` 级恢复场景已补齐：`test_waiting_ack_timeout_replays_pending_deferred_updates`、`test_apply_retryable_tlc_operations_unblocks_when_mailbox_drained`、`test_send_payment_with_node_restart_then_resend_add_tlc`、`test_node_reestablish_resend_remove_tlc`、`test_reestablish_bidirectional_pending`。

### 已执行回归（建议最小集）

- `cargo nextest run -p fnn test_stale_inflight_attempt_ids test_no_stale_inflight_attempt_ids_before_timeout`
- `cargo nextest run -p fnn test_collect_replay_peer_tlc_updates_filters_settled_tlcs test_collect_replay_peer_tlc_updates_dedupes_latest_update`
- `cargo nextest run -p fnn test_waiting_ack_timeout_replays_pending_deferred_updates test_apply_retryable_tlc_operations_unblocks_when_mailbox_drained test_send_payment_with_node_restart_then_resend_add_tlc test_node_reestablish_resend_remove_tlc test_reestablish_bidirectional_pending`

### 2026-02-27 执行更新（Task6/8）

- 已完成：  
  - 针对 `test_legacy_fallback_dual_owed_no_commit_diff`（`ReestablishChannel` legacy dual-owed、无 `CommitDiff` 场景）新增问题定位并修复：重建回退时强制走一次 `resend_tlcs_on_reestablish(..., force_send_commitment_signed=true)`，避免 `local commitment number` 与 `remote commitment number` 长期不一致。  
  - 该修改与既有 `dual-owed` replay 逻辑兼容，属于超时收敛后 fallback 场景修复，不改外部消息接口。
- 回归状态：  
  - 先跑 `cargo nextest run -p fnn test_legacy_fallback_dual_owed_no_commit_diff test_legacy_fallback_single_owed_no_commit_diff --no-fail-fast`（通过）。  
  - 再跑全量 `cargo nextest run -p fnn --no-fail-fast`（`736 tests run: 736 passed, 7 skipped`）。

### 已知边界（截至 Task8）

- 尚缺少针对部分极端 mailbox 时序的 stress 回归（如多次快速重连叠加 `dual-owed` 的非对称乱序），可作为下一轮压测目标。  
- 当前修复仍偏向“恢复能力增强”，尚未引入 LND 风格序列化 mailbox 元数据（`in_flight`/`seq`）模型。

## 2026-02-27 执行更新（Task9：仅日志定位）

- 本次只做排障性日志补齐（未改业务分支），目标是追踪 stuck 的首次阻断点。补齐位置如下：
  - `check_for_tlc_update`：记录每次检查入参、通道状态、方向校验、`waiting_ack/reestablishing` 阻塞点。  
  - `handle_add_tlc_peer_message` / `handle_remove_tlc_peer_message`：记录入站 Add/Remove 的快照与本地处理结果。  
  - `handle_add_tlc_command` / `handle_remove_tlc_command`：记录命令入参、创建/移除 tlc 关键字段、发送对端消息前后。  
  - `register_retryable_tlc_operation` / `apply_retryable_tlc_operations`：记录去重、入队、阻塞、弹出/执行与执行结果。  
  - `post_add_tlc_command`：记录 `AddTlcResult` 分支是走重试队列还是直接通知。  
  - `record_pending_replay_update`：记录 `pending_replay_updates` 队列长度与内容变化。  
  - `register_retryable_relay_tlc_remove`：记录中继重试删除链路是否触发及分支理由。  
- 与网络层、支付层同步补齐日志：
  - `crates/fiber-lib/src/fiber/network.rs` 增补了 `resume_payment_actor_and_send_command`、`start_payment_actor`、`send_command_to_channel` 等入口日志，便于定位 actor 重建与命令转发路径。  
  - `crates/fiber-lib/src/fiber/payment.rs` 增补了 `handle_check_payment_status`、`handle_add_tlc_result_event`、`handle_remove_tlc_event` 的日志，便于追踪 inflight attempt 在支付层是否超时、是否被重放。

### 诊断建议（仅采样日志）

- 只关注 `trace/debug/warn` 中以下关键词组合：
  - `WaitingTlcAck`
  - `apply_retryable_tlc_operations`
  - `record_pending_replay_update`
  - `replay`
  - `handle_add_tlc_command start / post_add_tlc_command`
  - `check_for_tlc_update rejected`
- 当看到链路卡住时，优先核对顺序：
  - `check_for_tlc_update` 是否返回 `WaitingTlcAck`；
  - `retryable_tlc_operations` 是否持续入队但没有弹出；
  - `pending_replay_updates / deferred_peer_tlc_updates` 是否长期不降；
  - `CheckActiveChannel` 是否再次触发而未触发后续转移。
