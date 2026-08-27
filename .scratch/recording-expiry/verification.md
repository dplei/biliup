# 录制期限实现验证

日期：2026-08-27  
分支：`dev`

## 已实现范围

- migration 18：独立 `recording_lease` 审计表、活动租约 partial unique index、到期和通知索引。
- 持久状态机：创建/替换/取消、`scheduled → grace_current_session → expired_paused` CAS、乐观并发。
- Worker 活动场次快照、开播前最终准入、下播确认后且重新入队前的原子暂停边界。
- 启动恢复、5 秒到期扫描、5 秒通知扫描和 shutdown 清理。
- 可等待的 Webhook 传输；HTTP 非 2xx、钉钉非零 `errcode`、超时均作为失败；claim 超时回收和
  `1m → 5m → 15m → 1h` 退避持久化。
- 租约 PUT/DELETE、幂等 recording-state、旧 pause toggle 的到期恢复守卫、主播列表批量租约投影。
- 直播管理页第五按钮、期限弹窗、三种状态标签、时区/服务器时间、清除二次确认与通知反馈。

## 自动化证据

以下命令在本工作树通过：

```text
SQLX_OFFLINE=true cargo check --workspace
cargo test --workspace
npx tsc --noEmit
npm run lint
npm run build
```

结果摘要：

- Rust workspace：全部测试通过；`biliup-cli` 250 passed / 1 ignored，其他 crate、集成测试和
  doctest 均通过。
- 录制租约定向测试：9 passed，覆盖到期等号边界、替换/取消、并发创建只保留一条活动租约、
  级联删除、stale expected id、grace 同场一次性暂停、旧任务不能关闭延期租约、Webhook 500
  首次一分钟退避、成功后重复扫描不重发。
- 前端类型检查和生产构建通过。
- lint 只有既有 `app/(auth)/login/page.jsx` 的 `<img>` 警告；生产构建另有既有 Semi CSS
  `align-items: end` 的 autoprefixer 警告，没有新增错误。

fake Webhook 自动化观测：

- 成功端点：连续扫描两次，请求计数保持 1，数据库状态为 `sent`。
- 500 端点：请求计数为 1，数据库状态为 `failed`、`notification_attempts = 1`，
  `next_notification_at = now + 1m`。

## 尚待真实 dev 验收

没有在本轮擅自启动生产数据目录或使用真实客户房间，因此以下 P0 证据仍需按 spec 第 13.3 节在
隔离测试直播间补齐，完成前不应把真实验收标为通过：

- 页面三种状态截图与窄屏五按钮截图。
- 真实直播中到期后文件继续增长、没有租约触发取消/额外切段。
- `delay` 宽限内断流恢复仍为同一场。
- grace 中重启后的 `live_session_key`、`streamer_info_id`、`upload_session_id` 一致性。
- 真正下播后不再产生下一场文件，但尾段、上传、投稿和后处理完成。
- 测试钉钉机器人一次真实成功投递，以及先失败后恢复的截图/日志。
- migration 18 在 `data/data.sqlite3` 的一致性备份副本与 Linux/amd64 镜像内冒烟。

## 残余风险

- 外部机器人没有幂等键协议；进程若在机器人已收取、`sent` 尚未落库的窗口崩溃，恢复后可能重发。
  本地保证 at-least-once，并以消息中的租约事件 ID 识别极端重复。
- 本轮验证覆盖状态机和本地 fake Webhook，但平台断流、重启续接和真实钉钉格式仍以手工 dev 验收为
  最终证据。
