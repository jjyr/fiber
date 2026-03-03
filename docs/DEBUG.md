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

---

## Update (2026-03-02): Focus on Precise Stuck-TLC Source Localization (No Fix)

### Scope

- This round focuses only on localization and evidence collection.
- No behavior-level fix is kept from this round; non-trace changes were rolled back.

### Primary artifact

- Full verbose run log:
  - `/tmp/ring_restart_debug_verbose_1772438641.log`
- This run completed end-to-end (~300s) and failed with:
  - stuck TLCs
  - balance drift assertion (`initial: 800000000000000`, `final: 800000000002001`)

### Key timeline (from logs)

1) First missing-actor remove appears on channel `af45...` at `2026-03-02T08:08:20`

- Example first hash:
  - `payment_hash=0x66d8...e69e`
  - pre-restart trace source: `sender=D`, `seq=72`

2) Later large-scale missing-actor remove storm appears on channel `33bf...` at `2026-03-02T08:08:23`

- Example first hash:
  - `payment_hash=0xfed1...d0e3`
  - pre-restart trace source: `sender=B`, `seq=18`

3) Reestablish asymmetry during restart window

- `af45...` and `5226...` show `Reestablishing channel ...` and `... reestablished successfully`.
- `33bf...` does not show successful reestablish in the same window.
- `33bf...` related reconnect attempts include dial failures (`BrokenPipe`, `ConnectionRefused`).

### What “stuck TLC source” means from this run

Not a single sender bug. It is a channel bottleneck effect:

- For residual channel `33bf...`, mapped residual payment hashes come from all senders:
  - `A=12`, `B=12`, `C=19`, `D=14` (57 unique hashes)
- Other stuck channels also show multi-sender distribution.
- Therefore, “who emitted stuck TLC” is multi-source; the dominant choke point is channel lifecycle/recovery around `33bf...`.

### Updated understanding

- The dominant failure in this run is not one isolated payment path, but:
  - missing channel actor during RemoveTlc settle path
  - repeated `Channel not found` on remove
  - retry queue growth and residual TLC accumulation
- `UnknownNextPeer` is not the main signature in this specific run; `Channel not found` during remove/settle is.

### Added diagnostics (trace-only)

- In `test_ring_self_payments_then_restart_two_nodes`, added trace-only logs:
  - per-stuck-channel sender distribution
  - per-stuck-channel sample entries (`payment_hash`, `sender`, `seq`, `tlc_id`, `status`, `forwarding_tlc`, `removed_reason`)
- File:
  - `crates/fiber-lib/src/fiber/tests/channel.rs`

### Next localization step

- Re-run the ring restart test with the new trace-only diagnostics and extract:
  - first stuck wave per channel
  - corresponding sender/seq/payment_hash tuple
  - channel recovery state around that exact window

---

## Update (2026-03-02): Root Cause Refinement for `7f90...` (No Fix)

### New deterministic chain (from code + logs)

1) Reconnect is effectively one-shot in this test window

- `MAINTAINING_CONNECTIONS_INTERVAL` is `1200s` (`crates/fiber-lib/src/fiber/network.rs`).
- `MaintainConnections` is triggered once at actor startup, then every 20 minutes.
- In this full failing run, `"Trying to connect to peers with mutual channels"` appears only 6 times total:
  - 4 initial node boots
  - 2 restart boots
  - no extra reconnect tick inside the ~300s test runtime

2) `7f90...` only got startup-time reconnect attempts, both with stale pre-restart addresses

- Node A startup-side attempt:
  - `2026-03-02T09:13:15.229759Z`
  - `Reconnecting channel 7f90... peers QmWYo... with addresses {51758,51759}`
- Node D startup-side attempt:
  - `2026-03-02T09:13:20.086516Z`
  - `Reconnecting channel 7f90... peers Qme5... with addresses {51748,51749}`
- Corresponding dial errors:
  - `2026-03-02T09:13:16.060324Z` dial `51758` failed (`ConnectionReset`)
  - `2026-03-02T09:13:20.673959Z` dial `51748` failed (`ConnectionRefused`)

3) Fresh addresses exist later, but no second reconnect window

- Node-0 restarted with new addr `52155/52156` at `2026-03-02T09:13:15.227978Z`.
- Node-3 restarted with new addr `52164/52165` at `2026-03-02T09:13:20.084557Z`.
- New node announcements are observed in logs (including later processing of node-0 new `52155/52156`), but `7f90...` has no further reconnect attempt lines after the two startup attempts.

4) Therefore `7f90...` never reestablishes in this run

- No `trying to reestablish channel 7f90...`
- No `reestablish_channel request ... channel=7f90...`
- No `channel 7f90... reestablished successfully`
- Meanwhile other channels (`f75...`, `9332...`) do show full reestablish success logs.

5) Missing-actor remove storm is downstream, not first cause

- `send_command_to_channel no running channel actor for 7f90...` and `missing-actor remove context ...` continue after the missed reconnect window.
- These residual removes then propagate through adjacent channels and become large-scale stuck TLCs.

### Code-level supporting gap

- `ServiceHandle::handle_error` only logs `DialerError` and has a TODO:
  - `ServiceError::DialerError => remove address from peer store`
- There is currently no immediate retry or retry scheduling on dial failure in this path.

### Current status

- This update is localization-only.
- No behavior fix is applied in this round.

---

## Update (2026-03-02): Trace Round for Exact Imbalance Source (No Fix)

### What was added (trace-only)

- `crates/fiber-lib/src/fiber/channel.rs`
  - Added `[trace][balance] remove_fulfill_apply ...` in `remove_tlc_with_reason`:
    - channel id
    - payment hash
    - tlc id/status
    - amount
    - old/new local+remote balances
    - waiting_ack / reestablishing flags
- `crates/fiber-lib/src/fiber/tests/channel.rs`
  - Added `[trace][balance]` summaries before total assertion:
    - final endpoint balances for A/B/C/D
    - per-channel pair sum `initial => final` for AB/BC/CD/DA

### Repro run

- Command:
  - `RUST_LOG=debug cargo nextest run -p fnn test_ring_self_payments_then_restart_two_nodes --run-ignored ignored-only --nocapture`
- Log:
  - `/tmp/tlc_stuck_retrace_round2.log`
- Result:
  - failed with balance drift
  - `initial: 800000000000000`
  - `final:   800000000001001`

### Deterministic localization result

1) Imbalance is isolated to the `C-D` channel pair

- `[trace][balance] per_channel_pair_sum ... CD=200000000000000=>200000000001001`
- AB/BC/DA remained conserved in this run.

2) Concrete first imbalance sample

- Channel: `0xae5e...` (C-D side in this run)
- Payment hash: `0xce6daa2b...9b3885`
- Observed apply:
  - `[trace][balance] remove_fulfill_apply ... channel=0xae5e... payment_hash=0xce6d... tlc_id=Received(24) amount=1001 ...`
- Missing counterpart on same channel:
  - no corresponding `tlc_id=Offered(24)` apply for the same hash on `0xae5e...`
- Residual confirms one-sided stuck:
  - node D on `0xae5e...` has `payment_hash=0xce6d...`, `tlc_id=Offered(24)`, `status=Outbound(RemoveWaitAck)`, `removed_reason=RemoveTlcFulfill`

3) Upstream remove dispatch for this same hash is repeatedly blocked by missing actor

- On channel `0x624a...`:
  - repeated logs:
    - `send_command_to_channel missing-actor remove context: ... payment_hash=0xce6d... tlc_id=Received(11) reason=RemoveTlcFulfill ... tlc_status=Inbound(Committed) ... forwarding_tlc=(0xae5e..., 24)`
  - duplicate retryable removes are ignored while actor is missing:
    - `duplicate retryable remove ignored for missing actor`

4) This run's missing-actor remove is large-scale, not a one-off

- Total `missing-actor remove context` lines: `405`
- Distribution by channel:
  - `0x624a...`: `206`
  - `0xae5e...`: `181`
  - `0x362e...`: `18`

### Current conclusion

- In this run, the first deterministic break is not `reestablish` send/recv delivery.
- The deterministic break is **remove path dispatch hitting missing channel actor**, then accumulating retry queue + residual TLCs.
- Balance drift is a downstream consequence of one-sided fulfill progression after these missing-actor remove breaks.

---

## Update (2026-03-02): Actor Map / Retry Replay Trace Round (No Fix)

### Added trace (this round)

- `crates/fiber-lib/src/fiber/network.rs`
  - `[trace][actor_map] on_channel_created start/inserted/skipped_insert_no_session`
  - `[trace][actor_map] channel_actor_stopped start/done`
  - extended `missing-actor remove context` with:
    - `remote_peer`
    - `peer_connected`
    - `peer_session`
    - `session_contains_channel`
    - `running_channels`
    - `session_map_entries`
    - `outpoint_mapped`
    - `pending_outpoint_mapped`

### Repro run

- Command:
  - `RUST_LOG=debug cargo nextest run -p fnn test_ring_self_payments_then_restart_two_nodes --run-ignored ignored-only --nocapture`
- Log:
  - `/tmp/tlc_stuck_retrace_round3.log`
- Result:
  - still failed by stuck TLC assertion
  - **no balance drift in this run**:
    - `AB/BC/CD/DA` all conserved (`initial == final`)

### Deterministic observations

1) `missing-actor remove` happens during disconnected window (not fake map state)

- Repeated lines show:
  - `peer_connected=false`
  - `peer_session=None`
  - `session_contains_channel=None`
  - `outpoint_mapped=false`
- Example channel ids in this run:
  - `b874...`
  - `e43d...`

2) `on_channel_created skipped_insert_no_session` is **not** observed

- All observed `on_channel_created` in this run have `session=Some(...)` and go through `inserted`.
- So this round did not reproduce a direct "spawned but not inserted because no session" branch.

3) `b874...` path: actor is eventually reestablished, but replay drain is too slow before test ends

- `missing-actor` storm happens first.
- Later `on_channel_created ... inserted` appears for `b874...` again.
- Then `reestablish ready branch exit` shows large backlog:
  - e.g. `retryable_ops=25`
- `apply_retryable_tlc_operations` for `b874...` repeatedly logs:
  - `blocked ... waiting_tlc_ack=true`
  - occasional `pop op` / `op succeeded`, then back to blocked
- Near test end it still has large remaining queue (e.g. `remained_ops=24`), so residual TLC assertion still fails.

### Current conclusion (this round)

- In this round, the key failure is **not** "reestablish did not happen".
- Reestablish eventually happens for the missing channel actor.
- The stuck outcome is from **long disconnected missing-actor window + large retryable queue + ack-gated replay progress**, which is not drained before assertion time.
- Therefore this run localizes to replay throughput/ordering under backlog, rather than map insertion failure semantics.

---

## Update (2026-03-03): Increase Test Wait Window to 120s and Reobserve

### Change

- Test `test_ring_self_payments_then_restart_two_nodes`:
  - `Wait for reestablish and TLC settlement` changed from `30s` to `120s`.
  - file: `crates/fiber-lib/src/fiber/tests/channel.rs`

### Repro result

- Command:
  - `RUST_LOG=debug cargo nextest run -p fnn test_ring_self_payments_then_restart_two_nodes --run-ignored ignored-only --nocapture`
- Log:
  - `/tmp/tlc_stuck_retrace_round4_wait120.log`
- Result:
  - `PASS` (`1 passed`)
  - balance conserved:
    - `AB/BC/CD/DA` all `initial == final`
  - no `Node still has stuck TLCs after restart`

### How logs represent real replay window and blocked time

- Real missing-actor window:
  - from first to last `missing-actor remove context` for the same channel id
- Actor rejoin point:
  - first `[trace][actor_map] on_channel_created inserted channel=...` after missing start
- Replay start:
  - first `apply_retryable_tlc_operations pop op` after actor rejoin
- Replay blocked time:
  - sum of durations from each `... blocked ...` to the next `... pop op ...` (same channel)

### Quantified comparison (30s vs 120s)

- 30s run (`/tmp/tlc_stuck_retrace_round3.log`, representative channel `b874...`):
  - missing window: `~24.98s`
  - rejoin after missing start: `~26.71s`
  - replay pops after rejoin before assert: `3`
  - last pop to assertion: `~0.56s`
  - outcome: stuck assertion failed

- 120s run (`/tmp/tlc_stuck_retrace_round4_wait120.log`, representative channel `8e49...`):
  - missing window: `~29.38s`
  - rejoin after missing start: `~29.66s`
  - replay pops after rejoin before assert: `63`
  - last pop to assertion: `~35.63s`
  - outcome: pass

### Conclusion

- Enlarging wait window alone can make this test pass consistently in at least this run.
- This confirms "window too short" is a real contributor.
- Replay remains ack-gated and can spend long time blocked under backlog; efficiency is still a separate optimization topic.

---

## Update (2026-03-03): Why `waiting_ack` Is Long (Layered Attribution, No Fix)

### Scope

- This round only localizes the reason for long `waiting_ack`.
- No behavior fix is applied in this update.

### Target sample (longest observed window in this run)

- Channel: `8e497f6606aa93307994062969d3a0b1507996d5a1140826e83df8879e0a064c`
- Log: `/tmp/tlc_stuck_retrace_round4_wait120.log`
- Long window:
  - `set_waiting_ack(true)` at `2026-03-03T03:38:58.538696Z`
  - `set_waiting_ack(false)` at `2026-03-03T03:39:31.628878Z`
  - duration: `33.090s`

### Layered timeline for this window

1) Channel/peer dropped first (actor not alive)

- `ChannelActor stopped ... reason: PeerDisConnected` appears for `8e49...` at:
  - `2026-03-03T03:39:00.022181Z`
- Network actor map cleanup confirms actor removed and outpoint mapping cleared:
  - `channel_actor_stopped ... outpoint_mapped_after=false`

2) Reconnect phase dominates elapsed time

- During the long window, repeated dial failures occur:
  - `DialerError ... ConnectionRefused` (multiple times)
  - `Handshake ... ConnectionReset` (for peer side)
- Representative failure sequence (same restart window) shows widening gaps:
  - `03:39:05.079` -> `03:39:07.390` (`+2.31s`)
  - `03:39:07.390` -> `03:39:10.165` (`+2.77s`)
  - `03:39:10.269` -> `03:39:15.535` (`+5.27s`)
  - `03:39:18.702` -> `03:39:30.541` (`+11.84s`)
- This is consistent with backoff-governed reconnect delay being the major contributor before reestablish can start.

3) Reestablish itself is comparatively short

- Reestablish for `8e49...` starts around:
  - `init_flag_set`: `2026-03-03T03:39:30.134724Z`
  - `handle_enter`: `2026-03-03T03:39:30.925260Z`
  - final `set_waiting_ack(false)`: `2026-03-03T03:39:31.628878Z`
- Phase split for this single window:
  - `waiting_ack(true) -> init_flag_set`: `~31.596s`
  - `init_flag_set -> waiting_ack(false)`: `~1.494s`

### Deterministic conclusion

- In this sample, long `waiting_ack` is primarily **reconnect-not-ready time** (peer offline/backoff/dial failures), not slow channel reestablish logic.
- Once reestablish enters message exchange, `waiting_ack` clears quickly (order of ~1-2s in this run).
- Therefore, the first layer to optimize/verify is reconnect availability and retry/backoff behavior, not `waiting_ack` state semantics.
