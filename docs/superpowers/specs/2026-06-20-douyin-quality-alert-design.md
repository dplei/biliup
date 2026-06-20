# 抖音画质降级告警 + 录制画质 tag 设计

## 背景

抖音选流在 `crates/biliup/src/downloader/live/douyin.rs` 的 `select_stream_url`：用户配置的画质
（`douyin_quality`）若在 `stream_data` 中不存在，会自动往下选一档可用画质
（顺序 `origin > uhd > hd > sd > ld > md`），**静默降级、不报错、不通知**。当 cookie（sessionid）
失效时，抖音常常只放出较低画质，于是录制悄悄掉到非蓝光，用户无感知。

本设计要做三件事：

1. 实际画质低于「告警阈值」时通过 webhook 主动推送（阈值可配，默认蓝光 `uhd`）。
2. 把「实际录到的画质」从下载器暴露出来。
3. 直播管理页录制中的房间旁显示「实际画质 录制中」tag。

## 术语与画质映射

| 代码 | 中文 | 排名（越小越高） |
|------|------|------------------|
| origin | 原画 | 0 |
| uhd | 蓝光 | 1 |
| hd | 超清 | 2 |
| sd | 高清 | 3 |
| ld | 标清 | 4 |
| md | 流畅 | 5 |

「蓝光」= `uhd`。

## 已确认的需求决策

- **触发判定**：实际画质排名 > 告警阈值排名（即比阈值更低）时推送。
- **告警阈值是独立配置**，与录制画质 `douyin_quality` 互不影响。即便录制画质设为 origin，
  告警仍按 `douyin_quality_alert`（默认 uhd）判定。
- **作用范围**：仅抖音。
- **双层配置**：告警阈值可在空间配置（全局）与直播管理（单房间）两处设置，机制同 `douyin_quality`。
- **推送频率**：每场开播只推一次。只在开播检测那一刻判定；录制中途若画质再掉档，只刷新 tag，
  不再补推。
- **可关闭**：告警阈值下拉含「关闭通知」选项；不选（unset）= 默认蓝光 `uhd`（开启）。
- **复用现有 webhook**：用 `cookie_health_webhook`，不新增 webhook 配置项。

## 架构与数据流

降级判定逻辑天然在 `douyin.rs`（只有它同时知道请求画质与实际选到的画质），但通知动作必须在
`biliup-cli` 层（webhook 配置 `cookie_health_webhook` 与 `notify_alert` 都在该层，`crates/biliup`
下载器拿不到）。因此「实际画质」要从下载器冒泡到 cli 层。

做法：给 `LiveStream` 加一个可选字段，只有抖音填它，其它平台留空——既满足「仅抖音」，又不破坏
通用结构。

```
douyin.rs select_stream_url
  └─ 算出 selected_quality
       └─ 写入 LiveStream.recording_quality: Option<String>
            ├─ monitor.rs 开播分支（唯一推送点 + 设置 tag 数据源）
            │     ├─ Worker.recording_quality = Some(actual)      → Part 3 tag
            │     └─ 若 rank(actual) > rank(alert_threshold) → notify_alert(cookie_health_webhook, …)  → Part 1 推送
            └─ download.rs 断流复查路径：只刷新 Worker.recording_quality（不推送）
```

`Config` 用 `#[derive(Patch)]` 自动生成 `ConfigPatch` 与 `apply()`，新增 `Option<String>` 字段后，
全局 + 单房间双层覆盖（`context.rs:193 get_config` 中 `cfg.apply(override_cfg)`）自动生效，无需额外接线。

## Part 1 — 告警阈值配置与判定

### 后端
- `config.rs`：新增字段
  ```rust
  /// 抖音画质降级告警阈值：实际录到的画质低于此档时 webhook 推送。
  /// 取值同画质（origin/uhd/hd/sd/ld/md），"off" = 关闭；缺省视为 "uhd"（蓝光）。
  #[serde(default)]
  pub douyin_quality_alert: Option<String>,
  ```
- 判定纯函数（放 `cookie_health.rs` 或新建小模块，便于单测）：
  ```rust
  /// 实际画质是否低于告警阈值（应推送）。threshold 为 None/空 时按默认 "uhd"；"off" 关闭。
  fn quality_below_alert(actual: &str, threshold: Option<&str>) -> bool
  ```
  画质排名表 `["origin","uhd","hd","sd","ld","md"]`，未知画质排到最低。

### 前端
- `app/ui/plugins/douyin.tsx` 加：
  ```tsx
  <Form.Select field="douyin_quality_alert" label="画质降级告警阈值（douyin_quality_alert）"
    extraText="实际录到的画质低于此档时通过 webhook 推送提醒（常见于 cookie 失效）。默认蓝光。"
    showClear>
    <Select.Option value="off">关闭通知</Select.Option>
    <Select.Option value="origin">原画（origin）</Select.Option>
    <Select.Option value="uhd">蓝光（uhd）</Select.Option>
    <Select.Option value="hd">超清（hd）</Select.Option>
    <Select.Option value="sd">高清（sd）</Select.Option>
    <Select.Option value="ld">标清（ld）</Select.Option>
    <Select.Option value="md">流畅（md）</Select.Option>
  </Form.Select>
  ```
  该组件同时被空间配置（全局）与直播管理（单房间）复用，两处自动生效。

## Part 2 — 暴露实际录制画质

- `crates/biliup/src/downloader/live/mod.rs`：`LiveStream` 加字段
  ```rust
  /// 实际选中的画质代码（origin/uhd/...）。仅抖音填充，其它平台为 None。
  pub recording_quality: Option<String>,
  ```
  所有构造 `LiveStream` 的地方补上 `recording_quality: None`（抖音填 `Some(selected_quality)`）。
- `douyin.rs check_stream`：把 `select_stream_url` 选到的画质一并返回（重构 `select_stream_url`
  返回 `(url, selected_quality)` 或单独提供选中画质），写入 `LiveStream.recording_quality`。
  注意 `douyin_true_origin` 提前返回分支对应实际画质 `origin`。

## Part 3 — 直播管理页录制画质 tag

### 后端
- `Worker`（`infrastructure/context.rs`）加 `recording_quality: RwLock<Option<String>>`，默认 None。
  - 开播进入 Working 时写入实际画质；下播/Idle 时清空（与 `downloader_status` 同生命周期管理，
    在 `change_status(Download, …)` 离开 Working 时清空，或在录制工作流收尾处清空）。
  - 断流复查（`download.rs`）拿到新的 `LiveStream.recording_quality` 时刷新该值。
- `LiveStreamerResponse`（`api/endpoints.rs`）加 `recording_quality: Option<String>`，
  `get_streamers_endpoint` 从 Worker 读取并返回。

### 前端
- `app/(app)/streamers/page.tsx`：当 `live.status === 'Working'` 且 `recording_quality` 有值时，
  在「直播中」tag 旁追加一个画质 tag，文案 `<中文画质> 录制中`，如 `蓝光 录制中`、`超清 录制中`。
  画质代码→中文映射在前端维护一份小表。

## 文案

- 标题：`⚠️ 抖音 未录到蓝光画质`
- 正文：`{主播备注/URL}：当前录制画质为 超清(hd)，低于告警阈值 蓝光(uhd)，可能是 cookie（sessionid）失效，建议检查更换。`

## 测试

- `select_stream_url`（或其返回选中画质的部分）单测：
  - stream_data 只含 `hd`、配置 origin → 选中画质为 `hd`，`LiveStream.recording_quality == Some("hd")`。
  - stream_data 含 `origin` → 选中 `origin`，`recording_quality == Some("origin")`。
- `quality_below_alert` 纯函数单测：
  - `("hd", Some("uhd")) == true`
  - `("uhd", Some("uhd")) == false`
  - `("origin", Some("uhd")) == false`
  - `("hd", Some("off")) == false`
  - `("hd", None) == true`（缺省按 uhd）
- `notify_alert` 在 webhook 为 None/空 时静默（已有测试覆盖）。

## 非目标（YAGNI）

- 不做其它平台的画质降级告警（仅抖音）。
- 不为画质告警新增独立 webhook 配置（复用 `cookie_health_webhook`）。
- 不做录制中途画质掉档的二次补推（每场仅开播判一次）。
