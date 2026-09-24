# 02 · dev 限速实跑 + 生产高峰窗口验收

Status: ready-for-human

Blocked by: 01（已 resolved）

## dev 实跑（合并前）

本机起 `biliup server` + `pnpm dev`，本地 sqlite，投稿模板只对自己可见。本机上行带宽有限，
**必须显式指定上传线路**，不要走 AUTO 探测。

1. 用 macOS 的 `dnctl` + `pfctl` 把到 `*.bilivideo.com` 的出向限到 2 Mbit/s，模拟整机限速。
2. **无证据路径**：清空 `upload_line_health` 中的冷却，上传一个大于 200 MB 的分段。
   预期：90 秒后出现 `verdict="abort"`，行为和旧版一致。
3. **有证据路径**：往本地库给**另一条**线路写一条 `last_failure_kind='slow_transfer'`、
   `cooldown_until` 在未来的记录，重新上传。预期：出现 `verdict="tolerate" reason="machine_wide"`，
   上传持续推进，最后以成功收尾；如果超过 2 小时，能看到总时长豁免的 `info!`。
4. 解除限速，确认下一次上传不受影响。

每一步都把日志链（去掉标识符）记到本文件的 `## 结果` 下。

## 生产验收清单（合并后贴到 issue #82，标 `awaiting-verification`）

在至少一个完整的高峰限速窗口里，下面几项都成立，才能关闭：

- [ ] 出现 `watchdog="slow_transfer" verdict="tolerate" reason="machine_wide"`，而且 `evidence_lines` 非空。
- [ ] 同一个限速窗口里，`verdict="abort"` 最多出现一次，也就是第一次判慢。
- [ ] 被忍住的 attempt 最后打出 `upload attempt completed`（成功路径没有 `outcome=` 字段），而不是 `total_upload_timeout`；大分段跨过 2 小时时能看到 `reason="machine_wide_throttle"` 的续期日志。
- [ ] 窗口结束后，被挡住的会话能正常投稿（`submitted=[...]` 里包含它）。
- [ ] 非限速时段，单线劣化仍然会 `verdict="abort"` 并换线，没有被误判成整机限速。

## 结果

### dev 实跑（已完成）

分支 `fix/issue82-*` 的 debug 构建，把 `data/` 复制到临时目录后在那里起 `biliup server`（不动真实库）。
通过手动补传入口 `POST /v1/uploads/missing/{id}/retry` 显式指定 `bda2`，走的是带 watchdog 的
真实上传路径。素材是 ffmpeg 合成的 940 MB FLV，放在一条 `source_missing` 行的原路径上。

**替代手段（和 upload-line-degradation 的 dev 实跑相同）**：没有用 `dnctl`/`pfctl` 限速（需要 sudo），
改为**抬高基线**。判据是纯比值，给一条不参与上传的线路写 `avg_mbps = 40`，判慢门槛就变成
10 MB/s，而本机实测只有约 3.4 MB/s，效果等同于把带宽压到约 1/12。另外关掉了响度标准化，
省掉和判据无关的预处理时间。为了不产生稿件，把同一会话的另一行置为 `failed`，
`next_retry_at` 设到很远的将来，挡住投稿闸门。

| # | 场景 | 期望 | 实测 |
| --- | --- | --- | --- |
| A | 基线 40，**无证据** | 90 秒时中止，和旧版一致 | `watchdog="slow_transfer" verdict="abort" window_secs=90 window_mbps=3.23 baseline_mbps=40 uploaded_bytes=293601280 total_bytes=939989606`（31%）→ `line failure recorded kind="slow_transfer" cooldown_remaining_secs=1800` → `attempt ended outcome="failed"` ✅ |
| B | 基线 40，**另一条线路（tx）有 `slow_transfer` 冷却** | 90 秒时忍住，传完 | `verdict="tolerate" reason="machine_wide" evidence_lines=["tx"] line=bda2 window_secs=90 window_mbps=3.37 uploaded_bytes=304087040`（32%，和 A 被中止的位置相同）；之后没有再判慢，`Upload completed ... cost 274.74s, 3.42 MB/s` → `upload attempt completed` ✅ |
| C | 解除限速：删掉抬高的基线，清空冷却 | 不判慢，正常传完 | 全程没有任何 `verdict=`，`Upload completed ... cost 274.71s, 3.42 MB/s` → `upload attempt completed`；bda2 没有冷却（3.42 MB/s 高于门槛 3.43/4）✅ |

B 的补充观察：
- 传完后 `record_success` 按现有逻辑给 bda2 写了一条 `slow_throughput` 冷却
  （`throughput 3.42 MB/s < baseline 40.00/4 MB/s`，30 分钟），这正是 spec 说的「证据自动续上」。
- A 留下的 bda2 自身的 `slow_transfer` 冷却在 B 之前已手动清掉，这样显式线路不会被冷却挡回。
  A 开始时库里没有任何冷却，所以「自己的冷却不算证据」只有单测
  （`own_or_non_slow_cooldowns_are_not_evidence`）覆盖，dev 没有单独跑这条。
- 会话投稿被预设的那一行挡住（`blocked ... failed=1`），没有产生稿件。

三次上传都只传到了 UPOS，没有产生稿件。跑完后已停掉服务，并删除临时目录（包括合成素材），真实的 `data/` 没有被改动。

### 未覆盖

**总时长豁免没有在 dev 触发。** 要触发它，得在限速状态下连续传 2 小时以上，按本机带宽需要几十 GB
的素材，而且代码里没有能缩短这个时长的配置项。这条逻辑只有「匹配守卫 + 打日志 + 重置计时器」
三行，靠生产验收清单里「大分段跨 2 小时」那一条观察。

## Comments
