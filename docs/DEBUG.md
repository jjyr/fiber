# Debug Record: `test_ring_self_payments_then_restart_two_nodes` (TLC stuck / channel restart)

Date: 2026-02-27
Branch: working tree at `repro-stuck-tlc-upstream`
Commit: `be7dd52e`

## Background

Target issue:

- 4-node ring ring `A-B-C-D-A` self-payment stress.
- Restart nodes `A` and `D` during in-flight load.
- Reproduction behavior: stale/stuck TLCs after restart, balance drift, and `outpoint_channel_map`/channel消失异常。

## Code changes made for diagnosis

Only test-only change was added in:

- `crates/fiber-lib/src/fiber/tests/channel.rs`

What was added:

- `RingTlcTrace` to track each sent self-payment:
  - sender
  - sequence
  - payment hash
  - request amount
  - initial status
  - planned route (path, in outpoint strings)
- Pre-restart trace snapshot collection using `get_payment_session` and first attempt route.
- Post-restart residual reconciliation map by `payment_hash`:
  - `residual_by_hash: payment_hash -> Vec<(node, channel_id, tlc_id, status)>`
- Additional logs:
  - per-node residual TLC dump
  - pre/post reconciliation summary
  - per-trace post-restart session status
  - explicit `UnknownNextPeer` + residual location dump
  - detection of traces that disappeared with no residual entries
- Failed assertion ordering adjustment:
  - collect and log all stuck info first
  - then fail at end

Related commit:

- `be7dd52e` "test: add route-level tlc reconciliation for restart stuck repro"

## Reproduction commands

- Run:
  - `cargo nextest run -p fnn test_ring_self_payments_then_restart_two_nodes -- --ignored`

Environment note:

- Some runs may fail early with:
  - `listen tentacle: Io(Os { code: 1, kind: PermissionDenied, message: "Operation not permitted" })`
- In this environment, one full long run still completed and produced reproducible failure traces.

## Key observed output (stable signature)

- Channel/session residual summary (example from 1/2+ run):
  - `Stuck TLCs detected after restart: [ ... ]`
  - multiple `("A", Hash256(...), n), ("B", ...), ...` entries across 4 nodes
- Balance assertion fail:
  - `initial: 800000000000000`
  - `final: 800000000002003` (or `...2004` in another full run)
- Repeated error signatures:
  - `Generating TlcErr from error WaitingTlcAck to error_code TemporaryChannelFailure for channel ...`
  - `Failed to remove tlc ... for channel ...: Channel not found error: ...`
  - `Failed to handle fiber network command: Channel not found error: ...`
  - `Channel id not found in outpoint_channel_map with ..., are we connected to the peer?`

## Root-cause trace (inference, not patch)

1) Restart path can lead to `outpoint_channel_map` missing entries

- In send path:
  - `handle_send_onion_packet_command`
  - resolves first-hop channel via `state.outpoint_channel_map.get(&channel_outpoint)`
- If absent, returns `TlcErr::new_channel_fail(UnknownNextPeer, ...)`
- Log point:
  - `crates/fiber-lib/src/fiber/network.rs:2388` (around map lookup)

2) Later settlement/cleanup also reports missing channel actors

- `send_command_to_channel` on a missing actor returns `ChannelNotFound(channel_id)`.
- This appears during remove/fulfill retries and leaves pending residual state.
- Log points:
  - `crates/fiber-lib/src/fiber/network.rs:3685` (`send_command_to_channel`)
  - `crates/fiber-lib/src/fiber/network.rs:1346` (`Failed to remove tlc ...: {}`)

3) Consequence observed in this test

- Residual TLCs remain visible in channel actor states on restart.
- Attempted cleanup cannot complete for those paths because channel resolution fails.
- That maps to:
  - session retries failing with `UnknownNextPeer`/`TemporaryChannelFailure`
  - stale TLC count/non-zero `all_tlcs`
  - balance drift assertion fail

## Direct answer to your original question

“outpoint_channel_map 找不到 channel”不只是警告；在当前复现中，它是最关键的触发点之一：

- `outpoint_channel_map` 缺失 -> 不能按 path 继续转发 -> `UnknownNextPeer`
- 之后清理流程找不到 channel actor -> `Channel not found`
- 结果是 TLC 不能正确收敛 -> stuck + 余额异常

因此，当前测试可支撑“是 due to TLC 无法在 restart 后回收”而非单纯发送阻塞；堵塞只会短期暂停，但最终应重试恢复；这里重试链路本身因为 channel 映射/状态丢失而失效。

## Repro artifacts / next actions

- If needed, export logs next time using:
  - `/tmp/fib-ring-restart.log`
  - `/tmp/fib-ring-restart2.log`
  - `/tmp/fib-ring-restart-filter.log`
- Next debugging focus for fix phase:
  - verify restart时 `outpoint_channel_map` 的完整重建与入驻时机
  - verify `ChannelReady`/reestablish race 与 remove/fulfill retryable 队列交互
  - ensure stale in-memory channel state does not block `retryable_tlc_operations` 的可恢复路径

