# 01 — 透传 B 站最终选中候选的实际 `qn`

Status: ready-for-agent
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

待实现后填写：改动提交、自动检查、dev 验收结论及是否观察到 CDN fallback。

## Comments
