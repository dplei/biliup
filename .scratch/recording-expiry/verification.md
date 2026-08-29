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

## 2026-08-29 本地 dev 手工验收

按 spec 13.3 在本机 dev 环境（`scripts/dev.sh` 的等价启动，`data/data.sqlite3`）实测。
录制对象是一个公开测试直播间，投稿走仅自己可见的本地模板，验证前已备份 dev 库。

### 场景 1：空闲房间到期（通过）

| 时刻 | 事件 | 状态 |
| --- | --- | --- |
| 10:41:43 | `recording_lease_created` | `scheduled` |
| 10:43:47 | `recording_lease_due` → `recording_lease_paused` | `expired_paused` |
| 10:43:52 | `recording_lease_notification_sent` | `sent`，`notification_attempts=1` |

约定到期与实际暂停相差 4 秒，落在 5 秒扫描间隔内。暂停后 monitor 退出（日志 `exit -> [Douyin]`），
主播状态转 `Pause`。**钉钉真实投递成功**，消息含客户备注、主播名、约定/实际时间，
时区按 `Asia/Shanghai` 渲染。通知发出后经过多轮 5 秒扫描，`notification_attempts` 稳定为 1、
`next_notification_at` 为空——成功后不重发已实测。

### 场景 2：直播中到期，本场不被打断（通过，核心不变量）

全局 `segment_time=00:15:00`，验证窗口两分钟内不会触发正常分段，因此任何新文件都只能来自
租约造成的额外切段。跨到期时刻每 5 秒采样：

| 时刻 | 文件大小 | 租约状态 |
| --- | --- | --- |
| 10:53:40 | 35.6 MB | `scheduled` |
| **10:53:45（约定到期）** | 37.4 MB | `scheduled` |
| 10:53:50 | 39.1 MB | `grace_current_session` |
| 10:55:07 | 60.6 MB | `grace_current_session` |

到期后 82 秒内文件继续增长 23 MB，速率恒定，**文件名全程未变、目录下始终只有一个录制文件**。
到期时刻 `recording_lease_grace_started` 记下本场身份（`grace_streamer_info_id`、
`grace_live_session_key`），`effective_paused_at` 为空、通知 `not_ready`——到期事件既没有中止
下载，也没有制造额外分段或中止点。

### 场景 4：grace 中重启，本场身份不分裂（通过）

在 `grace_current_session` 状态下停止并重启服务：

- `streamerinfo` 没有新增行，`live_session_key` 与场次创建时间不变；
- `upload_session` 仍只有一条绑在同一 `streamer_info_id` 上，没有分裂；
- 租约由 `grace_current_session` CAS 到 `expired_paused`，`effective_paused_at` 等于停机时刻；
- 停机时 `.part` 正确转正为完整录像，尾段随后完成音量标准化、时间戳扫描、上传与投稿，
  投稿 `submit_attempts=1`、会话 `finalized`/`ok_with_aid`。

需要说明：本次「本场结束」由进程停机触发，不是平台真正下播。真正下播的收敛路径仍以
`11.3 场次与重启测试` 的自动化覆盖为准。

### 场景 6：下一场阻断（通过）

租约进入 `expired_paused` 后，测试房间仍在直播，主动检查接口返回
`{"outcome":"paused","message":"该直播间已暂停录制，请先恢复"}`，
录制文件数与 `streamerinfo` 总数均未增加——到期后不会开始下一场。

### 场景 7：延期与清除（通过）

- 在 `expired_paused` 状态下延期：新租约 `scheduled`，旧租约转 `superseded`，
  主播状态由 `Pause` 恢复为 `Working`，租约自有的暂停被正确释放。
- 随后清除租约：接口返回空租约，租约行转 `cancelled`，主播不再受租约约束。
- 重复提交同一份创建请求（`expected_lease_id` 为空但已存在活动租约）被 409 拒绝，
  没有产生第二条活动租约——乐观并发在真实接口上同样成立。

### 本轮附带的两处修正

1. **通知模板去掉了直播间地址行**，标签「直播间」改为「主播」。地址是给机器看的，
   推送给人时只保留主播名与客户备注。`NotificationLease` 的 `url` 字段与查询里的
   `l.url` 一并移除。改后 9 个租约单元测试全通过，并由重启后的第二次真实推送验证。
2. **通知 webhook 复用的是全局配置的 `cookie_health_webhook`**，未配置时到期租约会停在
   `notification_status = 'not_configured'`——这正是此前从未看到租约通知的原因。

   一开始以为这是遗漏，查完调用点后结论相反：这个字段早已是所有运维通知的统一出口，
   cookie 失效与恢复、抖音画质降级、上传线路熔断、投稿结果、租约到期全部走它，租约复用它
   是一致做法，**不该拆成独立字段**。真正的毛病是命名和 UI 标签只提 cookie，让人根本想不到
   租约通知要靠它。因此本轮只改文案：UI 标签改为「通知推送地址」并列出全部五类事件，
   后端字段的文档注释同步说明；字段名保持不变，已有配置不受影响。

## 尚待真实 dev 验收

没有在本轮擅自启动生产数据目录或使用真实客户房间，因此以下 P0 证据仍需按 spec 第 13.3 节在
隔离测试直播间补齐，完成前不应把真实验收标为通过：

本轮（2026-08-29）已在本地 dev 补齐的：

- ~~真实直播中到期后文件继续增长、没有租约触发取消/额外切段~~（场景 2 通过）
- ~~grace 中重启后的 `live_session_key`、`streamer_info_id`、`upload_session_id` 一致性~~（场景 4 通过）
- ~~本场结束后不再产生下一场文件，但尾段、上传、投稿和后处理完成~~（场景 4/6 通过；
  「本场结束」由停机而非真实下播触发）
- ~~测试钉钉机器人一次真实成功投递~~（场景 1 通过，重启后第二次推送验证了新模板）

仍未完成：

- 页面三种状态截图与窄屏五按钮截图。本轮以 API 为主，但顺带确认了：直播管理卡片在
  `expired_paused` 下同时渲染「暂停中」与「已到期暂停」两个标签，卡片操作区存在「录制期限」
  按钮与「恢复录制」按钮。`scheduled` 与 `grace_current_session` 两态的页面呈现、弹窗表单和
  窄屏布局仍未截图。
- `delay` 宽限内断流恢复仍为同一场（需要可控的断流测试源）。
- 平台真正下播触发的收敛路径（不可控，等自然发生）。
- 钉钉先失败后恢复的真实退避证据（本地 fake webhook 已验，真实机器人未验）。
- migration 18 在生产库一致性备份副本与 Linux/amd64 镜像内冒烟。

## 残余风险

- 外部机器人没有幂等键协议；进程若在机器人已收取、`sent` 尚未落库的窗口崩溃，恢复后可能重发。
  本地保证 at-least-once，并以消息中的租约事件 ID 识别极端重复。
- 本轮验证覆盖状态机和本地 fake Webhook，但平台断流、重启续接和真实钉钉格式仍以手工 dev 验收为
  最终证据。
