# 断流重连拆稿与 21566 频控退避

来源：[dplei/biliup#60](https://github.com/dplei/biliup/issues/60)

## 结论

issue 的根因 1 描述**与当前代码不符**：`config.delay` 已经是下播宽限期，落在录制循环里
（`download.rs` `OfflineRetryState::record_unavailable`），流中断后会持续复查 `delay` 秒才判定
下播。所以「A 会话断流后立刻投稿」不是 delay 语义没落地，而是录制循环因某种原因**已经退出**
（连续离线/检查失败超过宽限、租约到期、取消），随后监控器 1 分钟后重新检出开播、开了新的
`DownloadTask`。生产日志本地拿不到，具体退出分支未核实；但无论哪个分支，后果都一样：
A 会话在关闭边界上传完尾段就立即被领取投稿并 finalize，新任务的分段只能落到新会话 B。

会话续接本身没有问题：`reusable_streamer_info`（开播检测）和 `find_or_create_session`
（分段登记）都按 `live_session_key` 优先、时钟窗口兜底找**未 finalize** 的会话。
唯一缺的是「关闭后给重连留一个窗口再投稿」。

根因 2、3 与 issue 描述一致：
- `retry_submission` 对所有明确失败走同一条 `30s × 2^n`、上限 30 min 的退避；21566 是账号级
  投稿频控，这个节奏只会持续撞墙，且每次都发一条告警。
- `SUBMIT_RETRY_BASE_SECS = 30` 小于 `PeriodicScan` 的 60 s 周期，前几次退避实际被拉平成 1 分钟。

## 最小方案

1. **关闭后保持窗口**（`download.rs` `persist_closed_session_intents`）：新写入投稿意图的会话
   同一步把 `next_submit_at` 置为 `now + delay`。`session_submit_readiness` 与调度器查询本来就按
   `next_submit_at` 判到期，不用改协调器；清 `next_submit_at` 的四条路径都在 claim/finalize 之后，
   分段登记不会碰它，窗口不会被尾段落库打断。`delay = 0` 时不写，保持老行为。
   页面 `pending_submit_action` 对「`next_submit_at` 未到但 `submit_state` 不是 failed」的行改文案，
   不再说「上次投稿明确失败」。
2. **21566 独立退避**（`upload.rs` `retry_submission`）：错误串含 `code: 21566` → 视为频控，
   退避 `15 min × 2^n`、上限 4 h；`last_submit_error` 上一条也是 21566 时不再发告警，只留日志。
   首次告警文案改为「已进入投稿频控冷却，预计 HH:MM 重试」。
3. **退避基数 30 → 60 s**：与 `PeriodicScan` 周期对齐，文档值即实际值。

## 不做

- 不做「上一会话已 finalized 则用 `/x/vu/web/edit` 追加分P」：方案 1 已覆盖 delay 内的重连；
  超过 delay 的重连按平台语义就是两场直播，追加需要新的稿件编辑流程与幂等保证，另开 issue。
- 不区分 delay 的房间级覆写：关闭窗口用全局 `config.delay`。
- 不改手动 recover 入口：窗口内点 recover 返回 `NotDue`，页面显示「下次」时间。
