# 01 — 透传 B 站最终选中候选的实际 `qn`

Status: resolved
Blocked by: —

## 背景

见 [`spec.md`](../spec.md)。`BiliStreamCandidate` 已有实际 `qn`，但
`select_stream_url` 只返回 URL，导致 `LiveStream.recording_quality` 固定为空。

## 做法

只改三个产品文件和同文件测试：

1. `crates/biliup/src/downloader/live/bilibili.rs`
   - `select_stream_url` 返回 `(String, u32)`；
   - 首选健康和 CDN fallback 都返回实际命中候选的 `qn`；
   - `check_stream` 写入 `recording_quality: Some(selected_qn.to_string())`；
   - 用本地 HTTP listener 覆盖首选与 fallback 两条路径，不新增依赖。
2. `crates/biliup/src/downloader/live/mod.rs`
   - 把 `recording_quality` 的注释改成平台无关的实际画质代码说明。
3. `app/(app)/streamers/page.tsx`
   - 在现有 `qualityName` 中增加九个 B 站数字 `qn` 文案；
   - 保留未知值原样显示。

不要改通用 Worker/API 链路，不要把 B 站候选改造成 `StreamCandidate`，不要顺手增加运行期
线路熔断或抽取前端共享画质模块。

## 验收标准

1. 首选候选健康时，选流返回该候选的 URL 和 `qn`。
2. 首选失败、fallback 候选健康时，选流返回 fallback 候选的 URL 和 `qn`。
3. B 站 `LiveStream.recording_quality` 是最终选中 `qn` 的十进制字符串，不是请求配置值。
4. 主播页对 `30000/20000/10000/401/400/250/150/80/0` 显示 spec 中对应中文，未知值仍原样显示。
5. `cargo test -p biliup downloader::live::bilibili` 与 `pnpm build` 通过。
6. dev 环境实录时接口与卡片显示实际画质；回写证据不含房间号、URL、Cookie 等账号信息。

## 回执

实现已完成并提交到本任务分支：

- `select_stream_url` 现在返回最终 URL 与该候选的实际 `qn`，`check_stream` 将其写入
  `recording_quality`；首选失败时使用 fallback 候选自己的 `qn`。
- 主播页沿用现有映射与未知值 fallback，补齐九个 B 站画质代码。
- `cargo test -p biliup downloader::live::bilibili`：通过（1 passed）。
- `./node_modules/.bin/next build`：通过（编译、类型检查与静态页面生成均成功；保留一条既有
  login 页面 `<img>` lint warning）。
- `pnpm build` 在本机 pnpm 11 的依赖状态检查阶段被 `@parcel/watcher` 构建脚本审批门禁阻止，
  尚未进入 Next.js；未为本任务改动依赖审批配置。
- [PR #24](https://github.com/dplei/biliup/pull/24) 已合入 `dev`。
- dev 实网调用现有 `check_stream` 检查当前开播的公开房间，成功返回数字 `qn`；未下载内容，
  也未记录房间号或 URL。CDN fallback 由本地 HTTP 回环测试覆盖，未人为干扰真实 CDN。

## Comments
