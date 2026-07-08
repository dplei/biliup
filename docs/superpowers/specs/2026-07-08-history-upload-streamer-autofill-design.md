# 投稿管理·历史文件上传自动回填主播信息（设计）

日期：2026-07-08
分支：dev
范围：仅本仓 Rust 现役后端 + Next.js 前端（`biliup/`）

## 背景与问题

在「投稿管理」页选历史文件上传时（前端 `app/(app)/upload-manager/page.tsx` 的 `handleOk` → `POST /v1/uploads`），
后端 `post_uploads`（`crates/biliup-cli/src/server/api/endpoints.rs:569`）当前**伪造了一个占位 `StreamerInfo`**：

```rust
StreamerInfo::new(
    &upload_config.template_name,  // name  = 模板名（非真实主播名）
    "stream_title",                // url   = 字面量
    "",                            // title = 空
    Utc::now(),                    // date  = 当前时间（非录制时间）
    "",
)
```

`build_studio` → `Recorder::format` 用这个 `StreamerInfo` 替换标题/简介模板里的
`{streamer}`（=name）、`{title}`（=title）、`{url}`（=url）与日期占位符
（`crates/biliup-cli/src/server/common/util.rs:44-49, 75-81`）。
于是真实主播名、直播信息全部丢失，稿件退化成「抖音通用模板」。

**这不是数据库关联损坏**：录制时每出一段就往 `filelist` 表写一行
`InsertFileItem { file, streamer_info_id }`（`crates/biliup-cli/src/server/common/upload.rs:1119`，
`file = prev_file_path.display()`、`streamer_info_id = ctx.id()`）。
`streamer_info` 表里存着真实的主播名/直播标题/直播间 url/开播时间
（`crates/biliup-cli/src/server/infrastructure/models.rs:14-28`）。
映射链路本就存在：`磁盘文件名 → filelist.file → streamer_info_id → streamer_info`。
问题只是 `post_uploads` 没去查这层映射，而是硬编码了占位值。

## 目标

选历史文件上传时，**按第一段文件（P1）反查真实主播信息**，填入投稿的标题/简介模板；
并在前端弹窗提示匹配到的主播，让用户上传前有可见反馈。零新依赖、UI 改动最小。

## 非目标（明确排除）

- 不改「多个文件合并成一个多 P 稿件」的既有行为（现在默认就是这样）。
- 不新增 P 排序 UI：顺序由既有可拖拽 `Transfer` 控制，`upload()` 按传入顺序保序
  （`crates/biliup-cli/src/server/common/upload.rs:821`）。
- 不处理 ffmpeg 下载器：ffmpeg 录制后把文件重命名去掉扩展名
  （`crates/biliup-cli/src/server/core/downloader/ffmpeg_downloader.rs:275-282`），
  `get_videos` 只列带扩展名文件 → ffmpeg 文件本就不出现在历史列表，属独立老问题。
  本设计以 **stream-gears**（文件名带扩展名）为前提。
- 问题 1（手动缺失补传后不删本地文件）不在本 spec 范围，另行处理。

## 前提

用户将下载器设为 **stream-gears**。此时 `filelist.file` 为带扩展名的录制文件名，
可与 `get_videos`（`entry.file_name()`，带扩展名 basename）对上。

## 方案：后端按 files[0] 反查 + 前端信息型弹窗

### 数据流

```
用户在 Transfer 勾选/拖拽文件 → 点「上传」
  → POST /v1/uploads { files, params(模板) }        # 请求体不变
  → 后端 post_uploads（在 spawn 上传前，同步执行）：
       1. 取 files[0]，用 match_streamer_by_filename 在 filelist 反查 streamer_info_id
       2. 命中 → get_streamer_info(pool, id) 载入真实 StreamerInfo
          未命中 → 沿用现有占位 StreamerInfo（兜底，不报错）
  → 用解析出的 StreamerInfo 构建 Recorder → build_studio（模板占位全填真值）
  → 返回 { matched: bool, streamer_name: Option<String> }，随后 spawn 真正上传
  → 前端据返回值弹 Semi 弹窗：
       matched=true  → Toast.success「将以主播【X】的信息投稿」
       matched=false → Notification.warning「未匹配到主播，使用模板默认值上传」
     两种情况上传都照常进行（信息型提示，不阻断）。
```

### 组件与改动

**1. 新增纯函数 `match_streamer_by_filename`（可单测，核心）**
位置：`crates/biliup-cli/src/server/common/upload_session.rs`（与 `get_streamer_info` 同处）。

签名建议：
```rust
pub async fn match_streamer_by_filename(
    pool: &ConnectionPool,
    file: &Path,
) -> AppResult<Option<i64>>   // 返回匹配到的 streamer_info_id
```

匹配规则（容错 stream-gears 可能带的路径前缀/扩展名差异）：
- 取传入 `file` 的 **basename 去扩展名词干**（stem）。
- 在 `filelist` 中找 `file` 列 basename 去扩展名后与之相等的行；
  取其 `streamer_info_id`。多行命中取任意一行（同一场同名段唯一，实际不冲突）。
- 无命中返回 `None`。

匹配的字符串归一化建议抽成**独立同步纯函数**（如 `filename_stem(path) -> String`），
便于单测，不碰 DB。SQL 侧可先把候选拉出再在 Rust 里按 stem 比对，避免依赖 SQLite 的路径函数。

**2. 后端 `post_uploads` 改造**（`crates/biliup-cli/src/server/api/endpoints.rs:569`）
- 在 `tokio::spawn` **之前**，同步解析：
  - `let sid = match_streamer_by_filename(&pool, &files[0]).await?;`
  - 命中：`let info = get_streamer_info(&pool, id).await?;` → `streamer_info = info`、
    `matched = true`、`streamer_name = Some(info.name.clone())`。
  - 未命中：`streamer_info = 占位 StreamerInfo`（沿用现有构造）、`matched = false`、`streamer_name = None`。
- 把解析出的 `streamer_info` 传入 `Recorder::new(upload_config.title.clone(), streamer_info)` 
  （替换现在 595-604 的伪造段），后续 `build_studio` 不变。
- 上传逻辑仍在 `tokio::spawn` 内异步跑（保持「立即返回、后台上传」的现状）。
- 返回体从 `{}` 改为 `{ "matched": bool, "streamer_name": Option<String> }`。
- `files` 为空时按现状短路（不解析、返回 matched=false）。

**3. 前端 `handleOk` 改造**（`app/(app)/upload-manager/page.tsx:66-74`）
- 读 `sendRequest('/v1/uploads', …)` 的返回值：
  - `matched` → `Toast.success('将以主播【' + streamer_name + '】的信息投稿')`
  - 否则 → `Notification.warning({ title:'未匹配到主播', content:'将使用模板默认值上传' })`
- 用 Semi 自带 `Toast` / `Notification`（项目已在用，如 `app/ui/UserList.tsx:66`），**不装新库**。
- 关掉 Modal 的行为不变。

### 错误处理

- 反查 DB 出错 → 冒泡为 500，前端 `sendRequest` 抛错走既有 catch。
- 匹配未命中 → **不是错误**，走占位兜底 + warning 提示，上传继续。
- `get_streamer_info(id)` 查不到（filelist 指向的 streamer_info 已被删）→ 视为未命中，占位兜底。

### 测试

- 单元测试（Rust，随 `upload_session.rs` 现有测试风格）：
  - `filename_stem`：`"a/b/小黄人2026-07-08.flv"` → `"小黄人2026-07-08"`；
    带路径前缀、带/不带扩展名都归一到同一 stem。
  - `match_streamer_by_filename`：
    - filelist 存 `小黄人2026-07-08.flv`，传入同名 → 命中对应 `streamer_info_id`；
    - 传入 basename 与存储含路径前缀 → 仍命中；
    - 无对应行 → `None`。
    （用内存/临时 sqlite pool，或按仓内既有 DB 测试夹具。）
- 手动验证（stream-gears 环境）：录一场 → 停 → 投稿管理选该场文件上传 →
  确认 B 站稿件标题/简介含真实主播名、弹窗提示主播名正确。
- 回归：选一个不在 filelist 的文件 → 弹「未匹配」warning 且仍以模板默认值上传成功。

## 约定与边界（写死在实现里）

- 整个多 P 稿件的主播信息**以 `files[0]`（P1）为准**；混选不同场次时按第一段那场算。
- 未命中一律占位兜底、非阻断。
- 仅 stream-gears 前提下保证可匹配。
