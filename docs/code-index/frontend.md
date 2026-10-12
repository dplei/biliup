# Web 前端与配置表单

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

页面布局、直播管理、上传页面、全局与房间覆写、共享请求及上传线路选项。

**从这里开始：** 按页面定位，再顺着请求进入服务端端点；日志页面集中在日志与观测领域。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `app/layout.tsx` | Next.js 根布局，设置中文页面语言、全局样式和应用级 `body` 容器。 | `RootLayout` |
| `app/(app)/page.tsx` | 应用根页面的路由入口，直接把用户重定向到直播管理页。 | `Home` |
| `app/(app)/layout.tsx` | `(app)` 分组的侧边导航布局：Semi `Layout`/`Nav` 骨架、路由映射、折叠态与主题切换入口；只渲染布局结构，`html`/`body` 由根布局独占。 | `Layout`、`isSub` |
| `app/(app)/streamers/page.tsx` | 直播管理卡片列表：聚合主播运行状态与画质标签，并提供编辑、暂停、删除和单房间配置覆写入口。 | `Home` |
| `app/ui/StreamerActions/RecordingLeaseModal.tsx` | 录制期限的创建、延期、清除与状态/通知反馈弹窗，负责把浏览器本地选择转换为明确 UTC 时间点。 | `RecordingLeaseModal` |
| `app/ui/StreamerActions/CheckStreamButton.tsx` | 直播管理卡片上的「立即检查直播流」按钮：调一次主动检查接口并按结论提示，随后刷新主播列表。 | `CheckStreamButton` |
| `app/ui/StreamerActions/PauseButton.tsx` | 以显式目标状态暂停/恢复单个直播间；到期暂停的恢复入口会禁用并提示先处理期限。 | `PauseButton`、`setRecordingState` |
| `app/(app)/uploads/page.tsx` | 上传列表页：每个分段从登记起就在这里，首传与失败重试同表；独立轮询待投稿会话和分段列表，按状态在单个「详情」列展示进度/去向/错误，并触发会话恢复、会话丢弃（不投稿）、立即上传、换线重传、停止、删除与本场补扫。 | `UploadList`、`renderDetail`、`AttemptHistoryPanel` |
| `app/ui/OverrideModal.tsx` | 主播级「配置覆写」弹窗：顶部 JSON 文本框与各分区控件合成同一份 `override`，提交时控件值覆盖文本框的同名键。`entityFields` 里的键是 livestreamers 表上的真实列，不进 override。音量一组由「为这个房间单独设置音量」独占，与全局同值且原先未覆写的项不写入，override 保持最小。Form 带 `key`，每次打开重建，否则 Semi 保留的折叠面板不会重新应用 initValues。 | `OverrideModal`、`handleOk`、`AudioOverrideSection`、`CoverSection`、`AUDIO_OVERRIDE_FIELDS`、`AUDIO_OVERRIDE_TOGGLE` |
| `app/ui/AudioNormalizationControl.tsx` | 响度标准化的表单控件，空间配置页与主播覆写弹窗共用同一套界面：开关、磁盘保留线、保留原片、竖向音量推子，以及基于 WebAudio 增益的样片试听。样片全局唯一，覆写弹窗传 `showSample={false}` 隐藏其更新/删除按钮。 | `AudioNormalizationControl`、`prepareAudio`、`STATUS_URL`、`SAMPLE_URL` |
| `app/lib/use-upload-lines.ts` | 两处线路下拉共用的 `/v1/upload-lines` hook：不含 auto、按字母排序求稳定，已保存却不在索引里的 key 保留为选项，已知 key 附中文说明。 | `useUploadLines`、`lineLabel`、`UploadLines` |
| `app/lib/api-streamer.ts` | 前端统一的 fetch 封装与错误处理边界：401 跳登录，JSON 错误透传，HTML/空正文按状态码翻译成中文提示。 | `fetcher`、`sendRequest`、`handleResponse`、`describeError` |
| `app/ui/plugins/global.tsx` | 全局下载与上传配置表单，复用动态上传线路选项和响度标准化控件。 | `Global` |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `app/ui/plugins/global.tsx`、`app/(app)/uploads/page.tsx` → `app/lib/use-upload-lines.ts` → `crates/biliup-cli/src/server/api/endpoints.rs`（`get_upload_lines`）→ `crates/biliup/src/uploader/line.rs`（`Probe::index_keys`）：线路下拉不再写死，B 站 `preupload?r=probe` 索引里新增/下线的 upcdn key 自动反映到页面；显式选线只靠 key（`Line::explicit`），代码里没有登记表。
- `app/(app)/layout.tsx`、`app/(app)/logviewer/page.tsx` → `app/lib/log-view-config.ts`（`LOG_NAV_ENTRIES`、`LOG_EVENTS_IS_DEFAULT`）：导航条目与默认页由同一个开关决定，切换默认页不需要改路由或页面代码。
- `app/(app)/uploads/page.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`get_missing_uploads`、`get_pending_submit_sessions`、`recover_session_uploads`、`discard_empty_upload_session`、`discard_session`）：页面分别读取分段聚合视图和独立待投稿会话；不确定 claim 不显示危险操作，其他已关闭且无在飞分段的会话可原子丢弃为不投稿，保留会话与分段账本。
- `app/ui/OverrideModal.tsx` → `crates/biliup-cli/src/server/config.rs`（`ConfigPatch`、`Config::apply`）：弹窗写的 `override` JSON 落在 livestreamers 表的 `override` 列，反序列化成 `struct_patch` 生成的 `ConfigPatch`。`Config` 的裸 `bool`/数字在 patch 侧都是 `Option`，因此**「键不存在」才表示跟随全局**，写 `false` 是显式覆写成关闭——界面上要区分这两态，靠的是额外的「是否单独设置」开关而不是字段本身。整份 override 每次提交整体替换；平台面板会把旧 override 灌进表单，提交时只能让相对旧值真正变化的控件覆盖顶部 JSON，否则未做成控件的字段永远改不动。
- `app/ui/StreamerActions/RecordingLeaseModal.tsx` → `crates/biliup-cli/src/server/api/recording_lease.rs`（租约 mutation）：弹窗提交明确 UTC 时间、当前租约 id 和客户备注，后端返回权威状态与服务器时间。

## 跨领域继续阅读

- [录制与直播检测](recording.md)
- [上传、投稿与补传](upload.md)
- [Web 服务、配置与业务存储](server.md)
- [日志与观测](observability.md)
