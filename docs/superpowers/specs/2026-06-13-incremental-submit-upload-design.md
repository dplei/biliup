# 增量投稿（每段上传即落 B 站稿件）设计

日期：2026-06-13
状态：已评审，待实现

## 背景与问题

服务端录制→上传链路在 `crates/biliup-cli/src/server/common/upload.rs`：

- `pipeline_upload_videos` 把每段上传得到的 `Video` 引用**只攒在内存** `uploaded.videos: Vec<Video>`，
  直到**下播才一次性** `build_studio` + `submit_to_bilibili`。
- DB 的 `filelist` 表仅存本地文件路径（`file` + `streamer_info_id`），
  **不存上传后的 `Video` 引用，也不存 aid/bvid**。

后果：进程在直播进行中重启 / 崩溃 → 内存里的 `Video` 列表全部丢失 →
已上传到 B 站存储的分片成为孤儿（既无法续传，也从未投稿），整批白传。

## 目标

「上传一个视频保存一次」：每段上传成功就 durable 落到 B 站稿件里，
进程任何时刻崩溃/重启都不丢已上传内容。

## 方案选型

调研结论：B 站虽有草稿能力，但当前代码已有更稳的等价手段——**增量投稿 + 编辑追加**：

- `submit_by_app`（`x/vu/app/add`）：首段上传后调用，创建稿件并返回 `aid`。
- `edit_by_app`（`x/vu/app/edit/full`）：携带 `aid` 把后续每段追加进同一稿件。
  CLI 的 `append()`（`uploader.rs:217`）已是此模式。

选定「状态主要存在 B 站侧」：首段建稿拿 aid，后续每段 edit 追加。
aid 是恢复的唯一锚点，必须落本地库。

## 会话模型（关键前提，已核实）

`monitor.rs:107-129`：每次 `check_stream` 检测到「开播」就 INSERT 一行新的 `streamerinfo`
（自增 `id`），`Context::new(insert.id, ...)`。因此：

- `streamer_info_id` = **「每次开播检测」级别的会话 id**（非配置 id）。
- 配置侧稳定 id 是 `room.id()` / `LiveStreamer.id`（被监控的直播间，跨场次不变）。

由此三类场景的天然行为：

| 场景 | 行为 | 结果 |
|---|---|---|
| 跨天连续直播（晚10点→次日5点）| 一直 Live，只在开播建一行 streamerinfo；按 segment_time/file_size 切多个文件分段，全挂同一 streamer_info_id | 天然一稿，跨天不拆 |
| 今天 vs 明天（两场独立直播）| 中间下播，明天是新开播检测 → 新 id | 天然两稿，自动区分 |
| 进程重启正撞上直播进行中 | monitor 对同一场物理直播**重新建一行新 streamer_info_id** | 需跨重启匹配，否则一场拆两稿（本设计解决）|

## 数据模型

新增表 `upload_session`：

```
upload_session
  id                INTEGER PK
  live_streamer_id  INTEGER  -- room.id()，稳定，用于跨重启匹配
  streamer_info_id  INTEGER  -- 当前挂接的会话 id（重启续接时会被更新）
  aid               INTEGER  -- B站稿件号，NULL = 还没建稿
  bvid              TEXT
  videos_json       TEXT     -- 已成功投稿的 Video 列表(JSON)，edit 时携带
  status            TEXT     -- uploading / submitted / finalized
  created_at        ...
  updated_at        ...
```

`videos_json`：`edit_by_app` 需带**完整** videos 列表（已有+新增）。
进程存活期从会话内存拿；重启后从 `videos_json`（或 `studio_data(aid)` 回查兜底）恢复已有列表再追加。

## 上传链路改造（`server/common/upload.rs`）

`pipeline_upload_videos` 每段循环内：

```
每段 upload_single_file 成功后：
  if aid 还没有(首段):
      studio = build_studio(含 title/cover/tag/source...) with [video]   // 仅此一次
      resp = submit_to_bilibili(studio)            // submit_by_app / submit_by_bcut_android
      aid, bvid = resp
      落库: upload_session{aid, bvid, videos_json=[video], status=submitted}
      若配置了 season_section_id：此时用 aid 加入合集一次
  else(后续段):
      videos = 会话已有列表 + 本段 video
      studio_cached.aid = Some(aid); studio_cached.videos = videos
      edit_by_app(studio_cached)
      落库: videos_json = videos
  postprocessor(本段路径)   // 每段投稿成功即 durable，立即删本地
```

要点：

1. `build_studio` 只在首段执行一次（封面上传/自动封面渲染开销大，稿件级元数据建稿后固定）。
   首段产出的 `Studio`（元数据部分）缓存进会话状态 / `UploadContext`，后续 edit 只换 `aid` + `videos`。
2. `Studio` 已有 `aid: Option<u64>` 字段（`bilibili.rs:126`），`edit_by_app` 直接吃，无需改结构体。
3. `process_with_upload` 原「下播后一次性 submit」整段删除，改为流结束后把 `status` 标记为 `finalized`。
4. 合集（season）触发点从下播后提前到「首段拿到 aid 后」。

## 本地文件删除时机

增量投稿下，每段 submit/edit 成功即 durable，**每段投稿成功后立即执行 postprocessor 删本地**。
磁盘峰值≈单切片，并顺带解决「重启后旧会话文件无人删」的孤儿文件问题。

注：这相对默认的「下播后删」是行为变化；增量模式下统一为「每段成功后删」。

## 重启恢复

正常收尾：`pipeline_upload_videos` 的 `rx` 流结束（下播）→ `upload_session.status = finalized`。
finalized 会话不再参与重启匹配。

恢复时机：monitor 检测到开播、`Context::new` 之后、消费分段之前，做一次匹配：

```
on 开播(room_id, 新 streamer_info_id):
    candidate = upload_session
        where live_streamer_id = room_id
          and status != 'finalized'
          and updated_at 在最近 recovery_window 分钟内
        order by updated_at desc limit 1
    if candidate:
        复用 candidate.aid / videos_json，更新 candidate.streamer_info_id = 新会话 id
        后续段走 edit 追加
    else:
        本会话首段时新建稿
```

- `recovery_window`：默认 **30 分钟**，可配。按本仓库 override 习惯：优先读录播设置（per-streamer），
  留空回退全局配置。
- 用时间窗口而非仅看 status：防止很久以前未 finalize 的脏会话被错误复用到一场全新直播上。
- 孤儿稿件兜底：既非 finalized 又超窗口的会话 → 仅 warn 日志，**不自动删稿**
  （已上传内容留在 B 站，用户可手动处理）。

## 边界与兼容

1. **submit_api 兼容**：
   - `App`：建稿 `submit_by_app` + 追加 `edit_by_app`，完整闭环。
   - `BCutAndroid`：必剪无对应 edit。处理：BCut 建稿，但追加一律走 `edit_by_app`（同账号 token 通用）。
     **需实测**；若 BCut 建的稿不能被 app edit，则 fallback：BCut 模式退回「下播一次性 submit」并 warn。
2. **edit 需完整 videos 列表**：存活期从内存拿；重启后从 `videos_json` 或 `studio_data(aid)` 回查。
3. **审核中稿件能否 edit**：B 站应允许审核中改稿，但**需实测确认**（实现期风险点）。
4. **edit 频次**：每段一次，符合诉求；暂不批处理（YAGNI）。

## 测试策略

纯函数单测（不依赖网络，沿用现有 `#[cfg(test)]` 风格）：

1. 恢复匹配逻辑：抽成纯函数（输入 session 记录列表 + room_id + now，输出选中项）——
   finalized 不选 / 超 30min 窗口不选 / 多候选选 updated_at 最新 / 无候选返回 None。
2. 会话状态转换：首段→submitted 且 aid 落库；后续段→videos_json 追加；流结束→finalized。
3. studio 复用：首段 build_studio 后，后续 edit 复用的 studio 仅 aid/videos 变化，元数据不变。

手动/集成验证（实测风险点，非自动化）：

- 审核中稿件能否 `edit_by_app` 追加。
- BCut 建稿能否被 `edit_by_app` 追加。
- 真机：建稿→追加 2 段→重启进程→确认续接同一 aid 而非新建。

回归：现有 `resolve_source` / `segment_paths` 等单测保持通过。

## 不做（YAGNI）

- 不改既有「中断当作下播再开播」的录制判定逻辑。
- 不做 edit 批处理。
- 不做孤儿稿件自动清理。
