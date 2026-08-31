/// 自动响度标准化样片
pub mod audio_normalization;
/// 认证相关API
pub mod auth;
/// B站API端点
pub mod bilibili_endpoints;
/// 封面背景图上传
pub mod cover_background;
/// 封面预览
pub mod cover_preview;
/// 通用API端点
pub mod endpoints;
/// 独立事件库的只读查询、实时接续与导出
pub mod log_events;
/// 录制租约与幂等录制状态端点。
pub mod recording_lease;
/// 单页应用静态文件处理
pub mod spa;
/// 主动检查直播流
pub mod stream_check;
pub mod ws;
