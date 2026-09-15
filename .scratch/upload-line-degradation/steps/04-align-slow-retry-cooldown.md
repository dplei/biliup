# 04 · 让慢传输冷却覆盖自动补传

Status: ready-for-agent

来源：[issue #52](https://github.com/dplei/biliup/issues/52)

## 根因

`upload_enrolled_with_watchdog` 把 `slow_transfer` 作为 `UploadFailureKind::RequestTimeout` 记录。
`record_failure` 对第一次普通失败只冷却线路 1 分钟；`fail_enrolled_attempt` 却固定把生命周期行的
`next_retry_at` 设为 10 分钟后。due-row scanner 真正领取补传时，原线路已不在冷却中，选路器可能
再次选回它。

这也解释了为什么 dev 验收只证明了中止出口，没有证明「重试换线」：没有等待到 10 分钟后的真实
补传，便没有覆盖两个计时器的组合行为。

## 最小方案

给 watchdog 的慢速判据一个明确的失败类别，仍走现有 `record_failure` 和 `cooldown_until` 通道，
但冷却时长复用已有 `SLOW_COOLDOWN = 30min`。其它 `request_timeout`、`no_progress_timeout` 和
`total_upload_timeout` 保持普通失败梯度不变。

这样不新增表、配置、调度器或逐行避让状态：失败行仍按现有 10 分钟退避，首次补传读取现有冷却
快照时必然跳过刚判慢的线路。30 分钟后线路自然恢复候选，避免永久拉黑。

## 顺带澄清诊断

`chunk=... chunk_elapsed_secs=...` 目前描述的是最近一次已确认进度，不是并发窗口里真正卡住的
分片；在触发 `slow_transfer` 的同一条进度回调中，它自然可能显示 0 秒。UPOS 层已经逐请求记录
`chunk_index`、`attempt`、`elapsed_ms` 和错误，足够定位具体请求。不要为 watchdog 再造一套并发
分片跟踪器；只需把聚合诊断字段改成不声称它找到了“卡住的分片”的措辞。

## 验证

- 单测：慢速失败写入 30 分钟冷却，且失败类别可从健康接口辨认。
- 组合测试：同一时刻产生 `slow_transfer` 与失败行，推进到 10 分钟后的首次 due scan，断言选路器
  跳过原线路。
- 回归：普通 request timeout 仍使用 1/5/15/60 分钟梯度；30 分钟后慢线路重新可选。
- `cargo test -p biliup-cli`。

## 不做

- 不绕过投稿门禁；补传成功后已有 `persist_segment` 唤醒投稿。
- 不增加重试次数上限；上限耗尽仍会留下阻塞投稿的 `failed` 行。
- 不做 UPOS 断点续传或跨线路复用分片；本问题只需让既有重传选到另一条线路。

## Comments
