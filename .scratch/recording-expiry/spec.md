# Spec：直播间录制到期与本场保活

状态：设计完成，尚未实现  
分支：`dev`  
日期：2026-08-27

## 1. 决策摘要

本功能直接内建到 biliup，不创建外挂项目。

每个直播间可以设置一条当前生效的“录制租约”，包含约定到期时间和客户/需求备注。服务器时间
达到到期时间后：

- 如果直播间未在录制，立即暂停后续录制并发送钉钉通知。
- 如果正在录制，不因到期时间停止下载、不额外切段，继续完成当前这场直播；确认本场结束后再
  暂停后续录制并发送钉钉通知。
- 常规的按时长/大小分段继续照常执行。“不切段”专指到期事件不制造额外分段或中止点。
- 暂停只限制后续下载；本场已经产生的分段仍按现有流程完成上传、补传、投稿和后处理。
- 到期状态、允许完成的本场身份和通知状态全部持久化。服务重启不能提前终止可继续的同一场，
  也不能绕过期限开始下一场。

## 2. 背景与现状

### 2.1 现有能力

- 直播管理卡片目前有编辑、暂停/恢复、删除和配置覆写四个按钮；新增入口应成为第五个按钮。
- `PUT /v1/streamers/{id}/pause` 已能把正在工作的下载任务切换为 `Pause`，并通过
  `DownloadTask::stop` 等待下载流程退出。
- `DownloadTask::execute` 已包含断流重连和 `delay` 下播宽限期；只有连续离线超过宽限期才确认
  本场结束。因此到期保活必须复用这个“确认下播”边界，不能把一次 CDN 断流当成本场结束。
- `streamerinfo.live_session_key` 和会话续接逻辑已经支持抖音、B 站在进程重启后识别同一场直播。
- 服务启动时已经会启动持久任务的主动扫描循环；录制租约扫描可复用同类生命周期和测试方式。
- `cookie_health_webhook` 已能适配钉钉、企微和通用 Webhook，但现有发送器是 fire-and-forget，
  且未持久记录投递结果，不能直接作为到期通知的可靠性边界。

### 2.2 现有暂停接口不能直接供定时脚本调用

现有接口是“切换”而不是“设为暂停”：对一个已经暂停的房间再次调用会恢复录制。暂停状态又只
保存在 `WorkerStatus` 内存里，服务重启会重新从数据库加载房间并进入普通轮询。因此外挂脚本会
同时面临重复调用反向恢复、登录 Cookie、服务重启、重复通知、时区和进程守护问题。

### 2.3 可复用的提交历史

- `92b5a30`：现有直播间暂停/恢复入口和 `WorkerStatus::Pause`。
- `d4faf76`：断流宽限期、确认下播边界和 Webhook 健康告警。
- `969853d`：钉钉/企微机器人消息格式适配。
- `558ac79`：主动扫描、CAS 领取、attempt 历史、场次键和重启续接；这是本功能持久状态机的
  主要范例。
- `7c8ed7a`：`scripts/dev.sh` 和本机后端/前端热重载验证环境。

`558ac79` 后已经在本机 dev 环境完成过 75 分钟真实抖音直播、录制中重启、同场续接、分段上传
和一次性投稿验证。本功能的真实验收应沿用该方法，而不是只看接口返回值。

## 3. 目标与非目标

### 3.1 目标

1. 在直播管理页为单个直播间设置、延期或清除到期时间和客户备注。
2. 所有到期判断使用服务器 UTC 时间；浏览器只负责提交一个明确的时间点。
3. 到期时正在录制的本场完整保留；到期后绝不开始一个新的直播场次。
4. 服务重启后保持相同语义。
5. 真正进入到期暂停后，向现有 Webhook 发送包含客户备注的通知，并对临时失败持久重试。
6. 页面、API、数据库和结构化日志能说明当前处于“未设置、待到期、等待本场结束、已暂停”中的
   哪一种状态。
7. 升级是加法迁移；没有设置租约的历史直播间行为完全不变。

### 3.2 非目标

- 不做多台 biliup 的中央客户/合同管理平台。
- 不做计费、续费订单、到期前多级提醒或客户 CRM。
- 首版不支持同一个直播间同时存在多条有效需求；只有一条当前有效租约，旧记录保留审计。
- 不增加单直播间独立钉钉机器人；首版复用现有全局 Webhook 配置。
- 不在到期瞬间发送“待停”消息；默认只在实际暂停后发送一次最终通知。
- 不停止上传器、不取消补传、不撤销已经进入投稿流程的本场内容。
- 不改变现有常规分段时长、分段大小和下播宽限期。

## 4. 产品语义

### 4.1 用户可见状态

| 状态 | 页面文案 | 是否允许新场次 | 到期通知 |
| --- | --- | --- | --- |
| 未设置 | 不显示到期标签 | 是 | 无 |
| `scheduled` | `录制至 08-31 23:59` | 是 | 无 |
| `grace_current_session` | `已到期 · 本场结束后暂停` | 只允许已登记的本场 | 暂不发送 |
| `expired_paused` | `已到期暂停` | 否 | 待发送/已发送 |

终态 `superseded`、`cancelled` 只用于数据库审计，不作为当前状态返回给卡片。

### 4.2 “本场”的定义

到期保活只授予在到期时已经开始录制的场次：

1. 运行中优先记录当前 `streamer_info_id`。
2. 同时记录脱敏的 `live_session_key`，用于进程重启后的同场识别。
3. 短暂断流、换拉流线路和 `delay` 宽限期内重连都留在同一个 `DownloadTask::execute` 中，仍是
   本场。
4. 服务重启后，仅当现有 `reusable_streamer_info` 规则能复用同一 `streamer_info_id` 或证明
   `live_session_key` 相同时，才继续录制。
5. 无法证明是同一场时采取保守策略：暂停，不以“可能是同一场”为理由误录下一场。

如果服务在到期前中断、到期后才恢复，但数据库中存在到期前已经开始且尚未 finalize 的同场记录，
并且平台场次键匹配，仍可进入 `grace_current_session` 续接本场。否则直接到期暂停。

### 4.3 时间语义

- 数据库存 UTC `DATETIME`，API 使用带时区的 RFC 3339，例如 `2026-08-31T15:59:00Z`。
- 页面按浏览器本地时区显示，并在选择器旁明确显示时区；钉钉消息使用服务器配置的
  `Asia/Shanghai` 展示时间，同时保留事件 ID 便于审计。
- 比较规则统一为 `server_now >= expires_at` 即到期。
- 扫描循环负责及时收敛，录制启动前的准入检查负责兜底；不能只依赖固定间隔扫描。
- 目标扫描精度为 5 秒，本地空闲房间从到期到显示暂停不超过 10 秒。

### 4.4 设置、延期和清除

- 新建租约要求到期时间晚于服务器当前时间；客户/需求备注选填，去除首尾空格后不超过 200
  个字符，留空时通知文案显示「（未填写）」。
- 保存新的时间会把旧的有效租约标记为 `superseded`，保留审计记录。
- 在 `grace_current_session` 阶段延期到未来或清除期限，会取消“本场结束后暂停”，当前录制和
  后续录制恢复普通语义。
- 在 `expired_paused` 阶段延期或清除时：只有确认本次 `Pause` 是租约施加的，才自动恢复轮询；
  如果房间原本就是人工暂停，则继续保持人工暂停。
- 清除操作必须二次确认，文案明确是否会恢复录制。
- 页面提交时携带当前租约 ID；如果期间已被其他页面更新，后端返回 `409`，避免旧页面清除或覆盖
  新租约。

### 4.5 与人工暂停的关系

- 人工暂停优先：用户在等待本场结束期间主动点击暂停，允许立即停止本场，这是明确的人为操作。
- 有 `expired_paused` 租约时禁止单独“恢复”绕过期限，返回 `409` 并提示先延期或清除期限。
- 租约到期时房间已经人工暂停，租约仍变为 `expired_paused` 并通知，但记录
  `pause_owned_by_lease = false`；之后延期不能擅自解除人工暂停。
- 新前端改用幂等的“设置暂停状态”接口，不再用无请求体的切换操作。旧 `/pause` 路由可保留一个
  版本作兼容，但恢复分支必须执行到期租约守卫。

## 5. 持久化设计

### 5.1 使用独立表，而不是给 `livestreamers` 加若干列

计划新增迁移 `18_add_recording_lease.sql` 和表 `recording_lease`。

选择独立表的原因：

- `livestreamers` 是直播间配置，现有配置文件导入和编辑接口会执行整行更新；把租约塞进去容易被
  不包含新字段的旧载荷静默清空。
- 客户需求需要保留延期、替换、取消和通知结果的审计历史。
- 租约状态机与主播基本配置的生命周期不同，独立后更容易做 CAS 和通知重试。

### 5.2 计划字段

```text
recording_lease
├── id                         INTEGER PRIMARY KEY
├── live_streamer_id           INTEGER NOT NULL, FK livestreamers(id) ON DELETE CASCADE
├── expires_at                 DATETIME NOT NULL
├── customer_note              TEXT NOT NULL
├── state                      TEXT NOT NULL
├── grace_streamer_info_id     INTEGER NULL
├── grace_live_session_key     TEXT NULL
├── pause_owned_by_lease       INTEGER NOT NULL DEFAULT 0
├── effective_paused_at        DATETIME NULL
├── notification_status        TEXT NOT NULL DEFAULT 'not_ready'
├── notification_claim_token   TEXT NULL
├── notification_claimed_at    DATETIME NULL
├── notification_attempts      INTEGER NOT NULL DEFAULT 0
├── next_notification_at       DATETIME NULL
├── last_notification_error    TEXT NULL
├── notified_at                DATETIME NULL
├── created_at                 DATETIME NOT NULL
└── updated_at                 DATETIME NOT NULL
```

约束和索引：

- `state` 只允许 `scheduled`、`grace_current_session`、`expired_paused`、`superseded`、
  `cancelled`。
- 每个 `live_streamer_id` 对前三种活动状态最多一行，使用 SQLite partial unique index 保证。
- 到期扫描索引覆盖 `(state, expires_at)`。
- 通知扫描索引覆盖 `(notification_status, next_notification_at)`。
- 客户备注不会写入普通结构化日志；只在受认证的 API、页面和目标 Webhook 消息中出现。

### 5.3 通知投递语义

租约进入 `expired_paused` 的同一事务中，把通知置为 `pending`。通知工作器用 claim token 领取、
发送并落库：

- HTTP 非 2xx 视为失败。
- 钉钉即使返回 HTTP 200，也必须检查响应 JSON 的 `errcode == 0`。
- 失败按 `1m → 5m → 15m → 1h` 退避，之后每小时重试；成功后不再发送。
- 服务重启会重新领取过期的 `sending` 租约，避免永久卡住。
- Webhook 未配置时不影响暂停，状态记为 `not_configured` 并在页面明确提示。

跨进程崩溃可能发生在“钉钉已收到、数据库尚未写 sent”的窄窗口，因此无法对外部机器人提供数学
意义上的 exactly-once。承诺是本地去重的 at-least-once：正常情况只发一次，成功落库后不重复；
消息携带 `租约事件 #id`，极端重复也可识别。

通知建议格式：

```text
【biliup】⏰ 客户录制到期，已暂停后续录制
客户/需求：<customer_note>
直播间：<streamer.remark>
地址：<streamer.url>
约定到期：2026-08-31 23:59:00 (Asia/Shanghai)
本场结束：2026-09-01 01:23:45        # 有本场保活时显示
实际暂停：2026-09-01 01:23:46
租约事件：#123
```

## 6. 后端状态机与并发边界

### 6.1 到期扫描

新增 `recording_lease` 模块，提供可独立调用一次的 `scan_due_recording_leases(now)` 和服务启动时
拉起的循环包装器。核心动作使用数据库条件更新/CAS，扫描器重入不会重复转换或重复通知。

对每条到期的 `scheduled` 租约：

1. 读取 `Worker` 的活动录制快照。
2. 如果存在到期前开始的活动录制，CAS 为 `grace_current_session`，持久化本场 ID/场次键，不调用
   `DownloadTask::stop`。
3. 否则 CAS 为 `expired_paused`，把 Worker 设置为 `Pause`、移出轮询队列，并登记通知。
4. Worker 或数据库临时不可用时保留可重试状态并写结构化错误，不把失败伪装成已暂停。

扫描间隔不是唯一安全边界。即使扫描器晚了一轮，录制启动前也必须重新读有效租约：

- `scheduled` 且未到期：允许。
- `scheduled` 但已到期：只有已存在、开始时间不晚于到期时间的同场可获保活；禁止创建新场。
- `grace_current_session`：只允许匹配的同一场。
- `expired_paused`：拒绝。

### 6.2 Worker 活动场次快照

`Worker` 增加只用于运行时协调的活动场次快照，至少包含：

```text
streamer_info_id
live_session_key
recording_started_at
```

在 `start_download_workflow` 把状态切为 `Working` 前写入，在工作流全部退出后清理。租约判断不得从
“最近一条数据库记录”猜当前任务，避免把旧的未 finalize 会话误当作正在录制。

### 6.3 本场结束的原子边界

当前 `DownloadTask::execute` 在确认本场结束后会直接 `wake_waker`，这会把 Worker 放回检查队列。
实现时必须在该动作之前插入租约收敛：

```text
确认下播并完成当前下载 attempt 的尾段处理
→ 若 grace 的 streamer_info_id / live_session_key 匹配本场
→ CAS: grace_current_session → expired_paused
→ WorkerStatus = Pause，并保证不重新入队
→ 登记通知 pending
→ 继续现有上传、投稿、后处理
```

不能依赖另一个异步扫描器“稍后再暂停”，否则本场结束到下一轮扫描之间可能开始下一场。

### 6.4 重启恢复

启动顺序必须保证租约准入在房间开始新下载之前生效：

1. 跑完 migration，加载租约状态。
2. 对 `expired_paused` 房间初始化为 `Pause`，不先以 `Idle` 启动监控。
3. `grace_current_session` 房间允许做直播状态检查，但只允许复用持久化的同场身份。
4. 同场仍直播则继续；确认离线或发现不同场次则转为 `expired_paused`。
5. `pending/sending` 通知恢复投递。

对于场次键缺失且无法复用同一 `streamer_info_id` 的平台，重启后保守暂停；页面和日志要说明是
“无法证明同场”，不能静默开始疑似下一场。

## 7. API 设计

### 7.1 租约接口

```text
PUT    /v1/streamers/{id}/recording-lease
DELETE /v1/streamers/{id}/recording-lease/{lease_id}
```

`PUT` 请求：

```json
{
  "expires_at": "2026-08-31T15:59:00Z",
  "customer_note": "客户 A · 八月活动",
  "expected_lease_id": 123
}
```

- 首次创建时 `expected_lease_id` 为 `null`。
- 更新时 ID 不匹配返回 `409`。
- 直播间不存在返回 `404`。
- 时间无时区、时间不晚于服务器当前时间、备注为空或超过 200 字返回 `400`。
- 返回后端算好的当前状态和服务器时间，前端不自行推导权威状态。

`DELETE` 仅取消指定活动租约；ID 已变化返回 `409`，重复取消相同终态可幂等返回当前结果。

### 7.2 主播列表响应

`GET /v1/streamers` 在现有 DTO 上增加可空的 `recording_lease` 投影：

```json
{
  "id": 123,
  "expires_at": "2026-08-31T15:59:00Z",
  "customer_note": "客户 A · 八月活动",
  "state": "grace_current_session",
  "effective_paused_at": null,
  "notification_status": "not_ready",
  "notified_at": null
}
```

列表查询应批量 JOIN/聚合当前有效租约，不能按直播间做 N+1 查询。

### 7.3 幂等暂停接口

新增或改造为显式目标状态：

```text
PUT /v1/streamers/{id}/recording-state
{ "paused": true | false }
```

相同请求重复执行保持相同状态。`paused: false` 遇到 `expired_paused` 返回 `409`。直播管理页迁移到该
接口；旧的 toggle 路由若暂时保留，也必须执行同一守卫。

## 8. 前端设计

### 8.1 第五个按钮

在现有四个按钮末尾增加时钟/日历图标，tooltip 为“录制期限”。点击打开
`RecordingLeaseModal`。

弹窗包含：

- 当前服务器时间与显示时区。
- 日期时间选择器，精确到分钟。
- 选填的“客户/需求备注”，最多 200 字。
- 当前状态、实际暂停时间和通知状态。
- “保存/延期”“清除期限”动作；清除需要二次确认。
- `grace_current_session` 时显示醒目说明：本场不会被中断，下播后停止录制下一场。
- `expired_paused` 且通知失败时显示错误摘要和“后台将自动重试”，首版不提供手动刷通知按钮。

### 8.2 卡片展示

- `scheduled`：浅蓝标签 `录制至 MM-DD HH:mm`。
- `grace_current_session`：橙色标签 `已到期 · 本场结束后暂停`。
- `expired_paused`：粉色标签 `已到期暂停`。
- 客户备注不常驻完整展示，避免卡片拥挤；按钮 tooltip 或弹窗内查看。
- 到期暂停时播放按钮禁用或点击后显示明确错误，引导先延期/清除，不能无提示地无效。

页面所有 mutation 都复用 `app/lib/api-streamer.ts` 的统一错误处理，成功后刷新
`/v1/streamers`。

## 9. 计划文件落点

计划新增：

```text
crates/biliup-cli/migrations/18_add_recording_lease.sql
crates/biliup-cli/src/server/common/recording_lease.rs
crates/biliup-cli/tests/recording_lease.rs
app/ui/StreamerActions/RecordingLeaseButton.tsx
app/ui/StreamerActions/RecordingLeaseModal.tsx
```

计划修改：

```text
crates/biliup-cli/src/server/common/mod.rs
crates/biliup-cli/src/server/app.rs
crates/biliup-cli/src/server/common/cookie_health.rs
crates/biliup-cli/src/server/common/download.rs
crates/biliup-cli/src/server/core/monitor.rs
crates/biliup-cli/src/server/infrastructure/context.rs
crates/biliup-cli/src/server/infrastructure/dto.rs
crates/biliup-cli/src/server/api/endpoints.rs
crates/biliup-cli/src/server/router.rs
app/(app)/streamers/page.tsx
app/lib/api-streamer.ts
CODE_INDEX.md
```

实际实现时，如果端点逻辑使 `endpoints.rs` 继续膨胀，应把租约端点拆为
`server/api/recording_lease.rs` 并挂子路由；不要为了严格匹配本清单把独立领域重新塞回大文件。

## 10. 分步实施计划

### 步骤 1：迁移、模型与仓储函数

1. 新增 migration 18、约束和索引。
2. 定义 `RecordingLease`、状态枚举、API 投影和时间字段。
3. 实现创建/替换、取消、查当前有效租约、CAS 状态转换、通知 claim/完成/失败函数。
4. 所有 SQL 使用条件更新和事务，禁止先查后无条件写。

完成门槛：迁移可在空库和现有库上执行；旧直播间读取正常；并发创建只能留下一个活动租约。

### 步骤 2：纯状态机和可控时钟

1. 把“到期后该立即暂停、允许本场还是拒绝新场”的判断提取为纯函数。
2. 扫描函数显式接收 `now`，测试不依赖真实 sleep 或修改系统时间。
3. 定义 `scheduled → grace → expired_paused` 和替换/取消路径。
4. 锁住边界条件 `now == expires_at`。

完成门槛：状态表的每条边都有单元测试；重复执行同一事件不产生额外状态变化。

### 步骤 3：活动场次快照与录制准入

1. 在 Worker 设置、读取和清理活动场次快照。
2. 在开始录制前增加最后一道租约准入检查。
3. 到期时运行中的旧场次进入 grace，不调用 stop；到期后新检测到的场次被拒绝。
4. 人工暂停仍可立即停止，且不会被 grace 自动恢复。

完成门槛：可证明“到期扫描”和“开始录制”并发时，按录制开始时间/场次身份得到确定结果。

### 步骤 4：确认下播后的暂停边界

1. 在 `DownloadTask::execute` 确认本场结束、重新入队之前调用租约完成逻辑。
2. 匹配 grace 场次时先持久化 `expired_paused`，再设置 Worker Pause，禁止重新入队。
3. 不匹配的旧任务不能关闭新租约。
4. 保证尾段处理、上传、投稿和后处理继续运行。

完成门槛：到期本场没有 `task is cancelled`，到期时不多出一个人为分段；本场结束后没有下一次
录制检查启动。

### 步骤 5：启动恢复和后台扫描

1. 在 `ApplicationController::serve` 同级启动租约到期扫描和通知扫描。
2. 调整房间初始化顺序，先应用到期状态再启动普通监控。
3. 实现重启后同场续接、不同场拒绝、无稳定场次键保守暂停。
4. 后台任务随服务 shutdown 正常退出，不遗留孤儿任务。

完成门槛：三种重启测试通过：到期前、grace 同场、已经 expired_paused。

### 步骤 6：可靠通知

1. 把现有 Webhook 传输拆成可 await、可返回错误的底层发送函数；现有健康告警可继续用异步包装。
2. 增加通知 claim、超时回收、退避和成功落库。
3. 校验钉钉 HTTP 状态和 `errcode`。
4. 消息加入客户备注、房间、约定到期、实际暂停、本场结束时间和事件 ID。

完成门槛：模拟 500、超时、钉钉 `errcode != 0` 后会重试；成功后多轮扫描不再发送。

### 步骤 7：API 与幂等暂停

1. 增加租约 PUT/DELETE 和 DTO 聚合。
2. 增加字段、时间、备注、直播间存在性和 optimistic concurrency 校验。
3. 页面暂停操作改为显式目标状态；到期暂停不能被普通恢复绕过。
4. 返回 400/404/409 的可读中文错误。

完成门槛：接口重复请求、并发旧页面请求和无权限/未登录路径行为确定。

### 步骤 8：第五按钮、弹窗和状态标签

1. 增加按钮、表单、时区提示、状态说明和清除确认。
2. 卡片展示三种租约状态。
3. 错误走统一 Notification/SWR 边界。
4. 检查窄屏下五个按钮和标签不溢出卡片。

完成门槛：前端 lint、类型检查和生产构建通过；本地浏览器完成创建、延期、清除和冲突提示。

### 步骤 9：可观测性与文档收尾

增加结构化事件：

```text
recording_lease_created
recording_lease_due
recording_lease_grace_started
recording_lease_same_session_resumed
recording_lease_new_session_blocked
recording_lease_paused
recording_lease_notification_retry
recording_lease_notification_sent
recording_lease_cancelled
recording_lease_superseded
```

日志带 `live_streamer_id`、`streamer_info_id`、`lease_id` 和状态，不带客户备注全文、不带 Webhook
密钥。更新 `CODE_INDEX.md`；实现与验证过程记录到本目录后续的 `verification.md`。

完成门槛：只看日志即可解释为什么某房间继续本场、何时暂停、通知是否成功。

## 11. 自动化测试计划

### 11.1 迁移与仓储测试

- 空库迁移和既有库升级。
- 租约完整往返，删除直播间级联清理。
- 配置文件重复导入/主播编辑不会覆盖租约。
- partial unique index 拒绝同房间两条活动租约。
- 创建、替换、取消和 stale lease ID 的 409/CAS 行为。

### 11.2 状态机测试

- `now < expires_at` 不动作；`now == expires_at` 到期。
- 空闲、Pending、人工 Pause、Working 四种 Worker 快照。
- 到期前开始的 Working 进入 grace；到期后才开始的任务被拒绝。
- grace 同场结束转 paused；旧场次结束不能关闭后来延期的新租约。
- grace 期间延期/清除不会在旧任务结束时被再次暂停。
- 重复扫描、重复结束回调只产生一条通知事件。

### 11.3 场次与重启测试

- 重启前后同 `streamer_info_id`。
- 同一 `live_session_key` 续接。
- 不同 key 视为下一场并拒绝。
- key 缺失且无法证明同场时拒绝。
- `delay` 宽限期内 Offline → Live 不触发租约暂停。
- 连续 Offline 超过宽限期才触发暂停。

### 11.4 通知测试

- 本地 fake HTTP 服务收到完整消息和客户备注。
- 非 2xx、超时、非法 JSON、钉钉非零 errcode 均落失败并退避。
- 成功落库后扫描十次仍只收到一条。
- `sending` claim 过期后由新进程收割重试。
- Webhook 未配置时暂停照常、状态可观测。

### 11.5 API 测试

- 正常创建、更新、清除和列表投影。
- 不带时区、过去时间、空备注、超长备注、主播不存在。
- stale `expected_lease_id` 和 stale DELETE。
- expired 状态下显式 resume 返回 409。
- 认证开启时未登录返回 401。

建议复用 `migrated_pool()`、Axum `oneshot` 和可注入的 fake clock/通知 transport；测试不得靠
真实等待几分钟来推进状态。

## 12. 验收标准

以下 P0 条件必须全部满足才能认为功能完成。

### A. 空闲到期

1. 给未直播房间设置两分钟后的期限和客户备注。
2. 到期后 10 秒内页面变为“已到期暂停”。
3. Worker 不再进入直播检查队列。
4. 钉钉只收到一条最终通知，备注和时间正确。

### B. 直播中到期，本场不被打断

1. 在测试直播已经稳定录制后设置短期限。
2. 到期前后下载任务 ID/`streamer_info_id` 不变。
3. 日志不存在因租约触发的 `task is cancelled`/stop。
4. 到期事件不制造额外切片；常规配置分段仍可继续产生。
5. 到期后页面显示“已到期 · 本场结束后暂停”，录像文件仍继续增长。
6. 短暂断流后在 `delay` 宽限期内恢复，仍继续同一场，不通知、不暂停。

### C. 下播后的收敛

1. 确认下播后，grace 租约在 Worker 重新入队前变为 `expired_paused`。
2. 本场尾段、上传、投稿和后处理完成，不因暂停丢失。
3. 钉钉消息包含客户备注、约定到期和本场实际结束时间。
4. 直播间再次开播时不产生新录像文件、不创建新的 `streamerinfo`/上传会话。

### D. 重启可靠性

1. `scheduled` 到期前重启，期限仍存在且按时生效。
2. 到期后的当前直播中重启，若场次键相同则复用同一 `streamerinfo` 并继续本场。
3. 重启后检测到不同场次时不开始录制。
4. `expired_paused` 重启后仍暂停。
5. 通知发送前重启会恢复投递；发送成功后重启不重复发送。

### E. 延期、清除和人工暂停

1. grace 期间延期，本场结束后不暂停。
2. expired 后延期，只有租约拥有的 Pause 才自动恢复；原本人工暂停的不自动恢复。
3. expired 状态下直接点击恢复被 409 拒绝并有明确 UI 提示。
4. 两个浏览器页面同时编辑时，旧页面不能覆盖新租约。

### F. 升级兼容

1. 未设置租约的所有现有直播间行为、录制、上传和页面操作不变。
2. 配置文件模式和数据库模式都能启动。
3. migration 18 在现有 `data/data.sqlite3` 的备份副本上成功执行。
4. 旧版本配置中没有任何新字段时仍能正常反序列化。

## 13. 本地 dev 验证流程

### 13.1 自动化门禁

```bash
SQLX_OFFLINE=true cargo check --workspace
cargo test --workspace
npx tsc --noEmit
npm run lint
npm run build
python3 scripts/check_code_index.py
```

如果 `SQLX_OFFLINE=true` 因新增静态 SQL 缓存缺失而失败，应按仓库现行 SQLx 策略更新离线元数据，
不能用移除检查绕过。

### 13.2 启动环境

1. 停止现有服务，对 `data/data.sqlite3` 做一致性备份，或准备不含生产客户房间的测试副本。
2. 使用测试投稿模板和测试直播间，禁止拿正在履约的客户房间做首次验证。
3. 运行 `scripts/dev.sh --web`。
4. 后端固定为 `127.0.0.1:19159`，前端热重载为 `http://localhost:3000`。
5. 第一轮通知使用本地 fake Webhook 观察请求；自动化通过后再用测试钉钉机器人做一次真实冒烟。

### 13.3 手工场景

1. **空闲房间**：设置两分钟后到期，核对标签、数据库状态、日志和通知。
2. **正在直播**：开始录制后设置两分钟后到期；观察到期前后文件持续增长，没有额外取消/切段。
3. **断流宽限**：在可控测试源上短断流并恢复，确认仍是 grace 同场。
4. **录制中重启**：grace 状态重启 dev 服务，核对 `live_session_key`、`streamer_info_id` 和
   `upload_session` 没有分裂。
5. **真正下播**：确认状态在重新入队前变为 expired，尾段与投稿完成，通知只发一次。
6. **下一场阻断**：让同一测试源再次开播，确认不生成新文件/新会话。
7. **延期/清除**：分别在 scheduled、grace、expired 三种状态操作，核对旧任务不会反向暂停新租约。
8. **通知失败**：fake Webhook 先返回 500 后恢复 200，确认退避、重试、成功停止重发。

验证证据记录到 `.scratch/recording-expiry/verification.md`，至少包含：

- 页面三种状态截图。
- 关键结构化日志时间线。
- 租约行的状态/时间/通知字段快照（客户备注可在分享时打码）。
- 同场重启前后的 `live_session_key`、`streamer_info_id`、`upload_session_id`。
- 录像文件在到期后仍增长、本场结束后不再产生下一场文件的证据。
- fake Webhook 请求次数和一次真实钉钉冒烟截图。

### 13.4 发布前额外门禁

- 按 `BUILD_AND_DEPLOY.md` 做 Linux/amd64 镜像构建与容器内 `biliup --version` 冒烟。
- 在迁移后的数据库副本上启动 release 档 `scripts/dev.sh --release`。
- 检查日志中不包含客户备注全文、Webhook URL 查询参数、Cookie 或签名参数。
- 发布说明明确 migration 18 和回滚限制：旧二进制会忽略租约表，降级前应先确认没有依赖租约暂停的
  客户房间。

## 14. 风险与对应措施

| 风险 | 对应措施 |
| --- | --- |
| 到期扫描与新场启动竞态 | 启动前最终准入检查；本场结束前完成 CAS 和 Pause，不靠下一轮扫描 |
| 服务重启后误把下一场当本场 | 持久化 `streamer_info_id` + `live_session_key`；不能证明同场就暂停 |
| 配置编辑清空期限 | 独立 `recording_lease` 表，不进入 `LiveStreamer::update_all_fields` |
| 重复请求把暂停变恢复 | 新增幂等目标状态 API；旧 toggle 恢复路径加租约守卫 |
| 钉钉 HTTP 200 但业务失败 | 解析 `errcode`；失败持久退避重试 |
| 进程崩溃造成重复通知 | claim token + 超时回收 + sent 状态；消息带事件 ID，承诺 at-least-once |
| 客户备注泄漏日志 | 普通日志只写 lease/streamer ID，错误截断脱敏 |
| 到期后长场持续数小时 | 这是确认的产品语义；页面明确“等待本场结束”，不偷偷强停 |
| 到期暂停误伤上传 | 只改变 Download stage；上传、补传和投稿状态机保持独立 |

## 15. 完成定义

只有在以下条件全部成立时才把本功能标记为完成：

1. 步骤 1–9 全部完成并有对应自动化测试。
2. 第 12 节 P0 验收项全部通过。
3. 第 13 节真实 dev 验证至少覆盖“直播中到期、录制中重启、确认下播、下一场阻断、钉钉通知”。
4. 没有设置租约的旧房间完成一轮录制/上传回归。
5. `verification.md` 记录了证据和任何接受的残余风险。
6. 代码索引、迁移说明和发布历史在实现完成后同步更新。

