# 03 — 结束边界与分段完成唤醒

Status: resolved
Blocked by: 02

## 背景

只在 `persist_segment` 看到“当前全部 succeeded”并不代表直播结束；只在上传消费者退出后再写结束状态，
又无法覆盖尾段长时间预处理期间的进程退出。需要把“生产端不会再产生新分段”和“已有分段上传完成”
作为两个独立事件持久化并汇合。

## 改动范围

1. `SegmentEventProcessor` 在 durable enrollment 成功时记录本场涉及的 `upload_session_id`。
2. 确认本场结束并 flush 所有尾部事件后、关闭上传 channel 前，对涉及会话调用
   `request_session_submit`。该写入必须早于等待所有分段上传完成。
3. 结束边界写入成功后唤醒统一提交协调器：
   - 若账本已完整，立即投稿；
   - 若仍有 pending/uploading/failed，记录 blocked 后返回。
4. `persist_segment` 提交事务后检查父会话是否已经有投稿意图；若有，则异步唤醒协调器。
   远端投稿不得发生在 `persist_segment` 的 SQLite 事务内。
5. 恢复任务批量按 `segment_order` 执行时，可以只在每次成功后做廉价唤醒；重复调用交给 submit claim
   去重，不用在调用者复制“我是不是最后一段”的脆弱判断。
6. 失败开放边界要明确：投稿意图落库失败应告警并保留会话可由人工 recover；不得因此删除本地文件
   或把会话误标 finalized。

## 时序要求

```text
尾段 durable enrollment
  -> request_session_submit
  -> 首次 reconcile（可能 blocked）
  -> 尾段 persist succeeded
  -> 再次 reconcile
  -> claim + submit
```

## 验收

- 最后一段比首次提交检查晚任意时间完成，完成后都能自动投稿。
- 在 `request_session_submit` 后、尾段成功前 kill 进程，投稿意图仍留在数据库。
- 直播过程中分段之间即使账本暂时全 succeeded，只要没有投稿意图就绝不投稿。
- 同时完成多个分段不会产生重复稿件。

## 测试

- 精确复现 issue 评论中的 11 秒竞态。
- 在投稿意图落库后模拟进程退出，断言数据库可供启动协调器恢复。
- 活跃直播无投稿意图的反例测试。

## Answer

`SegmentEventProcessor` 已记录每个 durable enrollment 的 session id，并在 flush 尾段后先持久化投稿
意图、再关闭上传 sender、最后异步唤醒统一协调器。`persist_segment` 在 SQLite 事务和 attempt 历史
完成后，仅对已有投稿意图的父会话异步唤醒。测试覆盖 close 边界重开数据库后意图仍在、blocked 后
尾段成功会再次进入协调器，以及活跃直播无意图时绝不触发投稿。
