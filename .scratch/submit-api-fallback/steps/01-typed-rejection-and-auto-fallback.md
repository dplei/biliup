# 01 · 结构化远端拒绝与自动通道降级

Status: ready-for-agent

## 目标

让共享投稿入口不再解析 Debug 字符串，并把未设置 `submit_api` 的既有“伪自动”语义落实为一次
受控的 `App → Web` fallback；显式接口选择仍保持严格。

## 改动

- `crates/biliup/src/error.rs`
  - 为投稿接口的非零响应增加带 `code`、`message` 的类型化错误；不要复用上传分块的
    `RateLimit`，也不要把整个 `ResponseData` 请求/响应体塞进错误。
- `crates/biliup/src/uploader/bilibili.rs`
  - `submit_by_app`、`submit_by_web`、`submit_by_bcut_android` 在 `code != 0` 时返回上述类型化错误。
  - 成功响应和三个接口的请求参数保持不变；不顺手改其他 API 的 `Kind::Custom`。
- `crates/biliup-cli/src/server/common/upload.rs`
  - 在调用 `change_context` 前完成投稿结果分类，保留成功、明确拒绝、本地前置失败和结果不确定四类。
  - `submit_api = None` 先调用 App；只有结构化 `code == 21566` 才记录脱敏
    `submit_fallback` 并调用一次 Web。
  - 显式 `app`、`web`、`b-cut-android` 只调用所选接口；非法值在零次远端请求下返回配置错误。
  - 不调用 BCut 作为自动 fallback，不按 message 文本或 Debug 输出做判断。
  - 页面上传与 Python / wheel 入口继续复用同一个函数；没有持久 claim 的入口遇到不确定结果时
    只返回错误，不在函数内部重试。
- `crates/biliup-cli/src/server/config.rs`
  - 更正 `submit_api` 的候选值注释；不增加配置字段。

避免为三个接口建立 trait、factory 或通用重试框架。若真实网络调用难以单测，只抽一个私有纯决策
函数接收“配置模式 + 当前接口 + 类型化结果”，测试下一步应调用哪个接口；实际函数按该决策执行。

## 测试

- `biliup`：非零 `ResponseData` 保留准确的 `code`/`message`，成功响应仍原样返回。
- `biliup-cli` 私有决策测试：
  - 未配置 + App 21566 → Web；
  - 未配置 + App 其他非零码 → 停止；
  - 未配置 + App 不确定结果 → 停止；
  - 显式三种接口的任何失败 → 不换通道；
  - 非法配置 → 零次远端调用。
- 保留现有页面上传、Python 上传和独立上传事件测试；独立 CLI 的显式 `SubmitOption` 路径不改。

## 验收

- `cargo test -p biliup uploader::bilibili --lib`
- `cargo test -p biliup-cli submit --lib`
- `cargo test -p biliup-cli --test standalone_upload_events`
- `cargo test -p biliup-cli --test page_upload_events`
- `cargo test -p stream-gears --lib`
- `rustfmt --edition 2024 --check crates/biliup/src/error.rs crates/biliup/src/uploader/bilibili.rs \
  crates/biliup-cli/src/server/common/upload.rs crates/biliup-cli/src/server/config.rs`

## 回执

实现后记录实际结果类型、共享入口的调用面、定向测试数量与命令结果。

## Comments

- 全仓 `cargo fmt --all -- --check` 当前会报告与本任务无关的既有差异；本 step 只检查触达文件。
