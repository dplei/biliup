# 01 — 码流停顿超时可配置并调低

Status: ready-for-agent

## 背景

[`httpflv.rs:361`](../../../crates/biliup/src/downloader/httpflv.rs) 的
`timeout(Duration::from_secs(30), self.resp.chunk())` 就是"连续 N 秒没收到任何字节"的检测器
（每收到一个 chunk 就重置），只是 30 秒对高码率直播源太长了。
上游掐断后本次每一轮都白等了 19~20 秒。

对一个稳定跑 14 Mbps 的连接，**连续 6~8 秒零字节已经足以判定连接已死**。

## 改动范围

1. `Connection` 增加字段 `stall_timeout: Duration`；`Connection::new(resp)` 保持
   30 秒默认以维持现有调用方行为，新增 `Connection::with_stall_timeout(resp, timeout)`。
2. `read_frame` 用 `self.stall_timeout` 替换硬编码的 30 秒。
3. 配置项：在 [`config.rs`](../../../crates/biliup-cli/src/server/config.rs) 增加
   `stream_stall_timeout_secs: Option<u64>`，**全局默认沿用 30 保证回滚安全**，
   支持 per-streamer override（参照 `douyin_route_failover` 的既有下发链路：
   `config.rs` → [`live.rs:68`](../../../crates/biliup-cli/src/server/core/live.rs) → 下载路径）。
4. 两个 `Connection::new` 调用点都要接上：
   [`crates/biliup-cli/src/downloader.rs:103`](../../../crates/biliup-cli/src/downloader.rs)
   与 [`stream_gears.rs:96`](../../../crates/biliup-cli/src/server/core/downloader/stream_gears.rs)。
   **后者是服务端实际走的路径**，漏了它等于没改。
5. 超时触发时已有 `HttpFlvReadTimeout { buffered }` 错误与 `warn!`，日志里补上
   `stall_timeout_secs` 与 `connected_ms`，让"是超时判死还是上游报错"在日志里一眼可分。

HLS 路径（`hls.rs`）没有同类超时，不在本轮范围。

## 验收标准

1. 单测：用一个读到一半就不再产出数据的 fake response，断言
   `read_frame` 在 `stall_timeout` 后返回 `HttpFlvReadTimeout`，而不是等满 30 秒。
2. 单测：`Connection::new` 的默认值仍是 30 秒（回滚安全）。
3. 单测：正常持续产出数据的 fake response 在超过 `stall_timeout` 的总时长内**不**触发超时
   （证明计时确实按 chunk 重置，不是从连接开始算）。
4. 配置单测：未设置该项时取 30；per-streamer override 生效。
5. `cargo test -p biliup -p biliup-cli` 全绿。

生产验收（该房间设为 6~8s 后连续观察 2 场）：

6. `httpflv download failed` 的 `error` 从 `Io(...)/Reset(...)` 变为
   `HttpFlvReadTimeout`，且 `connected_ms` 比分段时刻晚 **stall_timeout + ~1.5s** 左右，
   而不是晚 ~21 秒。
7. 边界缺口（按上下段媒体时间戳差计算）从 22~40s 降到 ~11s 量级。
8. **误判计数为 0**：不出现"低码率或正常时段被超时打断"导致的额外分 P。

## 风险

真实网络抖动持续超过阈值时会触发一次多余重连，代价是多一个分 P。缓解：

- 默认值保持 30，只对确认被掐的房间下调；
- `04` 的快路径让误判的代价从 3 秒降到 <1 秒；
- 生产验收第 8 条专门盯这个。

若两场观察下来误判 > 0，把阈值上调到 10~12 秒再评估，不要直接回滚——10 秒仍比 30 秒省一半。
