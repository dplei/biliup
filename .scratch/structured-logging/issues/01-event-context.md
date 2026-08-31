# 01 — 统一事件模型和任务关联

Status: needs-triage
Blocked by: —

来源：[spec](../spec.md)，[GitHub #2](https://github.com/dplei/biliup/issues/2)。仅拆分，尚未实现。

## 范围

- 定义级别、业务分类、稳定事件名、单位字段、版本和中文摘要约定。
- 区分 `streamer_info_id` 与 `upload_session_id`，明确下载/上传 attempt 和 CLI task 身份。
- 在文件创建时生成分段身份，通过关闭回调、登记、预处理和补传传递并保存映射。
- 优先贯通 `common/download.rs` → `core/downloader/stream_gears.rs` → `httpflv.rs`，
  以及 `common/upload.rs` → `uploader/line.rs`；其余支持的下载路径列明覆盖清单。
- 为 spawn、Actor 消息、回调和阻塞任务传递上下文，事件采集时取得不可变快照。
- 定义敏感字段允许列表与截断规则，转换原始错误链时也执行。

## 验收

1. 并发两场录制，反复切片/重连，DTS 与下载错误能正确归属，不依赖线程 ID 或文案。
2. 分段登记前后、正常上传与补传、临时产物替换后均能关联到同一个原分段。
3. 正确处理 span 创建后补字段；子任务不串上下文；未知字段与不适用字段明确区分。
4. 独立 CLI 无主播信息时仍有可追踪 task 身份；事件名不随文案修改。

## Comments

优先解决事故关联，不顺手重构现有业务状态机。
