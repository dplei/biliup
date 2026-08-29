# 07 — 平台场次标识（live session key）的提取与落库

Status: resolved
Model: Sonnet 5 —— 工作面广（逐平台挖 JSON 字段）但判断简单，且有明确的 fallback 规则兜底；
真正需要判断力的合并语义在 09。

## 背景

对应评估报告 D 的前置条件。09 要把会话续接从「30 分钟时钟窗口」换成「同一场直播」，前提是
系统里存在一个能跨进程重启稳定复现的场次标识。现在没有。

## 现状（已在源码确认，实现前不要再假设「用开播时间就行」）

- `LiveStream`（[live/mod.rs:312](crates/biliup/src/downloader/live/mod.rs:312)）**没有**场次字段。
- **`stream.date` 不是开播时间，是检测时刻**：全部 22 个平台实现都写 `date: Utc::now()`
  （抖音 [douyin.rs:148](crates/biliup/src/downloader/live/douyin.rs:148)、
  B 站 [bilibili.rs:123](crates/biliup/src/downloader/live/bilibili.rs:123)）。重启后再次检测就是
  一个新时间，拿它当场次键等于没有键。
- `attempt_id`（[live/mod.rs:331](crates/biliup/src/downloader/live/mod.rs:331)）是**单次选流尝试**
  的关联 ID，每次尝试都不同，同样不是场次键。
- 抖音已经有现成可用的键：`room.id_str` 被
  [douyin.rs:380](crates/biliup/src/downloader/live/douyin.rs:380) 读进 `self.room_id`，并已用于
  弹幕源（[douyin.rs:506](crates/biliup/src/downloader/live/douyin.rs:506)）。**主人 2026-08-27
  决定直接采用它，不做「是否跨场变化」的前置实测**（理由与残余风险见下）。

## 改动范围

1. `LiveStream` 增加 `live_session_key: Option<String>`（脱敏，不含 URL / Cookie / 签名参数）。
2. 逐平台填充，优先级按本仓库实际使用的平台排：
   - **抖音（必做）**：直接用 `self.room_id`（`room.id_str`）作为场次键，不必先做跨场实测。
   - **B 站等其它在用平台（尽力）**：能拿到就填，拿不到就留 `None`。
   - 不为不使用的平台专门做调研。
3. 落库：`streamer_info` 增加场次键列（新 migration），由
   [`streamer_info()`](crates/biliup-cli/src/server/core/live.rs:153) 与
   [monitor.rs:121](crates/biliup-cli/src/server/core/monitor.rs:121) 的插入路径写入。
4. **本任务不改任何续接/合并逻辑**，只保证键被采集、被持久化、可观测（日志 + 接口可见）。
   `None` 必须是合法值，09 会为它定义 fallback。

## 为什么 room_id 就够（主人 2026-08-27 定）

本地磁盘上只会存在当日录像，历史录像由主人手动处理。因此场次键只需要在「当天」这个尺度上
可靠，不需要全局唯一。

即便 `room_id` 在同一房间跨场不变，被误合并的窗口也很窄，因为续接还有一道已经存在的护栏：
09 的续接只认 `status != 'finalized'` 的会话
（[`select_recovery_candidate`](crates/biliup-cli/src/server/common/upload_session.rs:19)、
[`find_or_create_session`](crates/biliup-cli/src/server/common/segment_enrollment.rs:340) 都带这个
条件）。上一场正常下播即 finalize，第二场开播时挂不上去。只有「上一场压根没投稿成功、残留
`uploading`」时才可能跨场合并 —— 那种状态本来就需要人工介入。

**残余风险**（接受，不在本任务内处理）：同一天内上一场投稿失败残留 + 当天再次开播 → 两场
可能进同一个稿件。若日后要收紧，最廉价的做法是在场次键上再拼一个当地日期。

## 验收

- 同一场直播中途重启服务，两次检测拿到的 `live_session_key` 相同。（这是本任务的核心验收项。）
- 键为 `None` 时全链路不 panic、不误合并，行为与今天一致。
- 键不包含任何签名参数或 Cookie。

## 测试

- 单元：抖音 room 响应 fixture → 期望键；缺字段 → `None`。
- 手动：对着真实直播间做一次「录制中重启」，确认前后拿到同一个键，观察结果记进 `## Comments`。

## Answer

已实现（migration `17_add_live_session_key.sql`）。

- `LiveStream` 增加 `live_session_key: Option<String>`（`#[serde(default)]`），22 个平台实现默认填 `None`。
- 抖音取 `room.id_str`：抽出纯函数 `live_session_key_from_room`，`get_room_info` 用它写 `self.room_id`，
  构造 `LiveStream` 时直接带上。B 站取 `profile.room_id`。
- `streamerinfo` 与 `upload_session` 各加一列 `live_session_key`；`core::live::streamer_info()`
  通过新的 `StreamerInfo::with_live_session_key` 带上它，monitor 的插入路径写入。
- 本任务只负责采集与落库，续接语义在 09。`None` 是合法值，全链路按缺键处理。

测试：`douyin::live_session_key_tests` 三条（正常取值、缺字段/空串为 `None`、键不含 URL/Cookie/签名）。
真实直播间的「录制中重启」手工验证留待部署后进行，结果补记到本节。
