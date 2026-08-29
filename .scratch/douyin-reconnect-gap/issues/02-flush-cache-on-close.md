# 02 — 连接收尾时刷出缓存，保住分段后那一小段

Status: ready-for-agent

## 背景

分段时压进 `flv_tags_cache` 的是 `onMetaData + AAC seq header + H264 seq header + 当前关键帧`
（[`httpflv.rs:214-258`](../../../crates/biliup/src/downloader/httpflv.rs)），
这批数据要等**下一个关键帧**才随刷盘写入文件（[`httpflv.rs:196-212`](../../../crates/biliup/src/downloader/httpflv.rs)）。
连接死在下一个关键帧之前，新文件就停在 13 字节（9 字节 FLV header + 4 字节 PreviousTagSize0），
被判 `HeaderOnly` 删除。

实测该源约 1.5 秒的内容（~2.6 MB）就这么没了，而且分 P 边界上留下一个"什么都没有"的空文件记录。

## ⚠️ 前置条件：必须同时开启短分段保留

刷出来的是个 ~1.5 秒的短片段。默认配置下
`preserve_recoverable_short_segments` 是 **false**
（见 [`config.rs:758`](../../../crates/biliup-cli/src/server/config.rs) 的断言），
`MediaValidation::RecoverableShort` 会走
`remove_invalid_segment("recoverable short segment rejected by rollback configuration")` 被删掉——
**等于白做**。

因此本 ticket 的落地必须与"该房间开启 `preserve_recoverable_short_segments`"配套，
让片段进入既有的合并管线（`merged_recovery_outputs` / `deferred_recovery_batches`）。
这一条写进验收，不要漏。

## 改动范围

1. `parse_flv` 目前把 `flv_tags_cache` 声明在内层 `async` block 之外、循环之内使用。
   把它提到能被收尾代码访问的位置（内层 block 改为借用 `&mut cache`），
   使 `result` 返回后仍能读到未刷盘的缓存。
2. 在 `out.finish(close_reason)` **之前**插入条件刷盘。

### 刷盘的触发条件必须收窄

**只在"文件刚 `create_new` 且尚未写入任何 tag"时刷。**

理由：中途死亡时缓存里是"上一个关键帧之后的若干非关键帧"，刷出去会让文件以不可独立解码的帧结尾；
虽然多数播放器会忽略尾部，但收益接近零而风险不为零。本 ticket 要解决的场景恰好是
"新文件一个 tag 都没有"，把条件收窄到这个场景，判据清晰、回归面最小。

实现上需要一个"本文件已写入 tag 数"的计数（`FlvFile` 侧加一个字段，或复用
`segment.set_size_position(9 + 4)` 之后的 size 是否仍等于 13）。用显式计数更不容易读错。

3. 刷盘复用现有的 `out.write_tag(...)` 与 `segment.increase_size(...)` 路径，
   保持 `prev_timestamp` 的非单调告警逻辑不变。
4. 刷盘后打一条 `info!(event = "flushed_pending_tags", file, tags, bytes)`。

## 验收标准

1. 单测：构造"header → 分段 → 仅压入 metadata/seq headers/关键帧 → 连接断开"的字节流，
   断言收尾后文件包含这些 tag、大小 > 13、**不**被判 `HeaderOnly`。
2. 单测：构造"文件已写入若干 tag → 中途连接断开"，断言**不**触发刷盘（条件收窄生效），
   行为与改动前逐字节一致。
3. 单测：正常分段（下一个关键帧正常到达）路径不受影响，文件内容与改动前一致。
4. `cargo test -p biliup` 全绿。

生产验收（需与 `preserve_recoverable_short_segments: true` 同时生效）：

5. 分段边界不再出现 `discarding invalid media segment ... reason=HeaderOnly`。
6. 日志出现 `queueing recoverable short media segment`，且该片段最终进入合并输出
   （`merged_recovery_outputs` 计数增加），不是被丢弃。
7. 成片检查：合并后的分 P 在边界处比未修复前多出约 1.5 秒内容，且能正常播放。

## 风险

- 开启 `preserve_recoverable_short_segments` 本身会改变短分段的处理路径，属于既有功能的
  灰度开关。开启前确认该房间的合并管线跑通（`merged_recovery_outputs` 有历史记录）。
- **已确认不存在时长下限**：`HeaderOnly` 的判据是 `file_len <= 13`
  （[`util.rs:508`](../../../crates/biliup-cli/src/server/common/util.rs)），
  `RecoverableShort` 的判据是 `size < min_size`（默认 100 MB，
  [`util.rs:430`](../../../crates/biliup-cli/src/server/common/util.rs)），两者都不看时长。
  刷出的 ~2.6 MB 片段会稳定落在 `RecoverableShort`，收益成立。
