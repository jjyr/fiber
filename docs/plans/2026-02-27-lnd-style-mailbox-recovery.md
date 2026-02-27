# 2026-02-27 LND 风格 TLC mailbox 恢复修复计划（可执行版）

## 目标

目标：在不改变现有 `ChannelActor` 重建核心语义的前提下，修复 `tlc stuck` 的闭环缺口，使 `waiting_ack`、`retryable_tlc_operations` 与重连恢复行为收敛。

该计划基于现状约束：
- `deferred_peer_tlc_updates` 当前为 `VecDeque<DeferredPeerTlcUpdate>` 且带 `#[serde(skip)]`。
- 现有重建主路径以 `CommitDiff` 为主、legacy 兜底。
- `pending_replay_updates` 已存在并可复用。
- 回归以 `TDD` 为准。

## 里程碑约束

- 本计划先做正确性最小实现，再扩展到极端场景。
- 每个 Task 采用 `Failing test -> Implementation -> Pass`。
- 每个 task 输出日志与字段要可追踪：`waiting_ack、pending_replay、deferred_peer_tlc_updates`。
- 不做 API 破坏，不改现有对外 RPC。

---

## Task 0. 建立统一回归入口与观察指标（先决）

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`

### 目标
- 把当前“修复后可追踪性”标准化，避免后续任务只看到现象看不到原因。

### 动作
1. 在 `ChannelActorState::log`/路径中补充 3 个 debug 字段：
   - `waiting_ack`
   - `deferred_peer_tlc_updates_len`
   - `pending_replay_updates_len`
2. 对以下事件新增 trace：
   - 进入/退出 `waiting_ack`
   - 进入重连（`Reestablish`) 并开始/结束 replay
   - 处理 `CheckActiveChannel` 时的超时判定

### 验收
- 通过现有单元测试编译通过。
- 日志在相关场景可打印上述字段（不要求格式化为 JSON）。

---

## Task 1. 先修复持久化兼容：deferred peer TLC 仅恢复“可见队列”，不能丢已存在更新

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`

### 目标
- `deferred_peer_tlc_updates` 需要在持久化/反序列化时可见，避免重启后再次进入重复 replay/丢失 replay 语境。

### 动作
1. 保持字段类型先不替换为复杂结构，新增兼容字段策略：
   - 新增 `deferred_peer_tlc_updates: VecDeque<DeferredPeerTlcUpdate>` 的持久化兼容定义（保持现有主类型）。
   - 如果存在临时序列化元数据，需要用 `#[serde(skip)]` 注解仅配合重构版本；本任务目标是去掉导致失去值的 `skip` 并保证回放可继续。
2. 在反序列化兼容上补一段迁移逻辑（必要时通过私有 helper），将老快照继续解析到新结构。
3. 如果当前 `ChannelActorState` 的序列化测试使用 `bincode`，新增 1 个“重启快照保留 deferred 更新”的回归。

### 验收
- 新增测试覆盖 `bincode` 重建不丢 deferred 更新。
- 序列化兼容测试通过：老状态可反序列化。

---

## Task 2. 建立“可恢复的 mailbox 过滤源”：统一从 `collect_replay_peer_tlc_updates` 生成待重放更新列表

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`
- `crates/fiber-lib/src/fiber/tests/channel.rs`

### 目标
- 避免重连/重建直接重放已在 commit 确认或本地已处理的 TLC 更新。

### 动作
1. 新建内部 helper：
   - `collect_replay_peer_tlc_updates(state, remote_commitment_sigs...) -> Vec<DeferredPeerTlcUpdate>`
   - 输入优先使用 `pending_replay_updates`，不足时回退到 `deferred_peer_tlc_updates`。
2. 在该 helper 内做“去重 + 确认态过滤”：
   - 同一 `channel_id`/`tlc_id` 同类更新只保留最后一次语义有效值。
   - 过滤掉已确认已提交或已清空的更新。
3. 重连重放入口引用该 helper，取代直传全量 `peer_tlcs` 的逻辑。

### 验收
- 新增测试：
  - 重建时不会重放已提交更新。
  - 重放列表内不会存在可确认幂等重复项。

---

## Task 3. 重放后清理规则：commit 已推进时清理 mailbox 条目

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`

### 目标
- 防止 deferred 队列在多次重连后膨胀。

### 动作
1. 新增 `prune_deferred_peer_tlc_updates`，依赖 commit 与本地状态判断：
   - `Add`：对应 tlc 已变为已确认状态则可清理。
   - `Remove`：对应 tlc 已 settled/无 pending 时可清理。
2. 在以下时机调用：
   - `commitment_signed`/`revoke_and_ack` 成功落盘后。
   - `CheckActiveChannel` 的超时恢复分支执行后。
   - 重连重放完成后。
3. 仅做收缩，不引入新状态字段。

### 验收
- 新增测试：
  - commit 落盘后 deferred 队列长度下降。
  - 重连后不会重复发重复更新。

---

## Task 4. `apply_retryable_tlc_operations` 与 `deferred` 的协同：避免永久停顿

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`

### 目标
- 若 `waiting_ack` 收敛或队列已无实际未决项，retryable 循环应退出/进位，而非卡住。

### 动作
1. 增加 mailbox 感知退出条件：
   - 当 `pending_replay_updates` 已处理完且 deferred 队列已过滤为空时，不再把重试循环卡在 `WaitingTlcAck`。
2. 在阻塞路径记录 `reason`，区分：
   - “真正需等待对端 ack”。
   - “仅本地重放已耗尽，需继续状态机。”

### 验收
- 新增集成测试：
  - waiting_ack 超时/重连后 retry loop 能继续，不再长期阻塞。

---

## Task 5. `CheckActiveChannel` 变为可恢复节点（不单纯记录）

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`

### 目标
- 保持现有 `shutdown fallback`，但在超时后尝试触发 mailbox/重放收敛。

### 动作
1. 在超时分支补充 deterministic 的 fallback：
   - 触发一次 mailbox/重放 reconcile。
   - 清理/重置等待相关状态后再走现有兜底。
2. 保持行为等价（不会阻塞 actor），但确保下一次事件循环有可复用 replay 入口。

### 验收
- 新增/更新已有测试：
  - `waiting_ack` 超时后通道会进入可恢复状态。
  - 未决更新不会因超时被直接遗弃。

---

## Task 6. 完善重建差异路径：`CommitDiff` 与 legacy 均走同一收敛语义

### 修改文件
- `crates/fiber-lib/src/fiber/channel.rs`
- `crates/fiber-lib/src/fiber/tests/channel.rs`

### 目标
- 消除 dual-owed/双向重放路径在 commitDiff 与 fallback 的行为差异。

### 动作
1. 为两条路径共用 `collect_replay_peer_tlc_updates`。
2. 重放顺序保持既有先后，但输入由 “过滤后的待重放集合” 统一产出。
3. 不改动外部链路：`ReestablishChannelMessage` 及其回执字段保持不变。

### 验收
- 新增测试覆盖至少一个双向未决、重启、无重复重放场景。

---

## Task 7. 扩展回归测试（TDD 扩展）

### 修改文件
- `crates/fiber-lib/src/fiber/tests/channel.rs`

### 推荐新增测试（按优先级）
1. `test_reestablish_mailbox_filters_settled_tlcs`
2. `test_replay_peer_tlc_updates_not_duplicate_after_restart`
3. `test_waiting_ack_timeout_replays_pending_deferred_updates`
4. `test_apply_retryable_tlc_operations_unblocks_when_mailbox_drained`
5. `test_dual_owed_restart_mailbox_replay_once`

### 验收
- 任一任务均有失败用例先红绿再归一。

---

## Task 8. 回归命令与文档更新

### 目标
- 确认修复不回归既有重建与支付路径。

### 执行命令（建议按顺序）
1. `cargo nextest run -p fnn test_reestablish_replay`（或对应新增用例名）
2. `cargo nextest run -p fnn test_stale_inflight_attempt_ids test_no_stale_inflight_attempt_ids_before_timeout`
3. `cargo nextest run -p fnn --no-fail-fast`
4. `cargo fmt --all -- --check`

### 文档
- 更新 `docs/notes/tlc-stuck-state-management.md`：
  - 已完成项
  - 已知剩余边界
  - 回归结论

### Task 8 执行结论（2026-02-27）

- 状态：已完成。
- 已执行命令与结果：
  - `cargo nextest run -p fnn test_reestablish_replay`（与仓库内可用测试名对齐执行，结果通过）
  - `cargo nextest run -p fnn test_stale_inflight_attempt_ids test_no_stale_inflight_attempt_ids_before_timeout`（通过）
  - `cargo nextest run -p fnn test_collect_replay_peer_tlc_updates_filters_settled_tlcs test_collect_replay_peer_tlc_updates_dedupes_latest_update`（通过）
  - `cargo nextest run -p fnn test_waiting_ack_timeout_replays_pending_deferred_updates test_apply_retryable_tlc_operations_unblocks_when_mailbox_drained test_send_payment_with_node_restart_then_resend_add_tlc test_node_reestablish_resend_remove_tlc test_reestablish_bidirectional_pending`（通过）
  - `cargo nextest run -p fnn test_legacy_fallback_dual_owed_no_commit_diff test_legacy_fallback_single_owed_no_commit_diff --no-fail-fast`（通过）
  - `cargo nextest run -p fnn --no-fail-fast`（`736 tests run: 736 passed, 7 skipped`）
- 关键修正：`handle_reestablish_channel_message` legacy dual-owed 无 `CommitDiff` 分支，`resend_tlcs_on_reestablish(..., force_send_commitment_signed=true)`，解决 CN 落后导致的重建不收敛。

---

## 决策点

你之前要求“遇到决策点询问你”，本次计划固定采用：
- 不新增 mailbox 持久化序号/元数据结构，仅先以“去重+过滤+清理”修复主干逻辑。
- 重建主路径优先保留 `CommitDiff`，legacy 仅作为可验证的兼容回退。

如果你希望改为“带 sequence 的 mailbox 元数据模型”（例如含 in_flight/seq 字段），我可以再给第二版计划。

### 任务执行状态更新

- Task0~Task7：按文档路线完成并已覆盖对应回归。
- Task8：回归完成，建议进入收口阶段（`docs/notes/tlc-stuck-state-management.md` 已更新）。
