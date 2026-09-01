# Spec：B 站录制状态显示实际画质

Status: ready-for-agent
来源：[`dplei/biliup#23`](https://github.com/dplei/biliup/issues/23)
分支：`fix/issue23-260901-190253`

本文只定问题、方案与验收；实现收敛在 [`issues/01`](./issues/01-propagate-selected-qn.md)。

---

## 1. 目标

B 站进入录制状态后，`GET /v1/streamers` 的 `recording_quality` 返回本次实际命中流的
数字 `qn` 字符串，主播卡片显示对应中文画质，例如 `10000` 显示为「原画 录制中」。

这里的“实际”指最终成功候选携带的 `qn`：启用 `bili_cdn_fallback` 后若首选 URL 不健康，
必须报告 fallback 候选的 `qn`，不能报告配置请求值 `bili_qn`。

## 2. 现状与根因

当前 `dev`（`ae7f312`）的链路是：

1. `bilibili.rs` 的 `parse_codec_urls` / `parse_master_m3u8` 已把平台响应里的实际 `qn`
   保存在每个 `BiliStreamCandidate`。
2. `select_stream_url` 只返回 `String`。首选 CDN 或健康检查 fallback 命中后，候选身份和
   `qn` 在此丢失。
3. `check_stream` 固定构造 `recording_quality: None`。
4. 服务端录制流程已把 `LiveStream.recording_quality` 原样写入 Worker；
   `GET /v1/streamers` 也已原样返回 Worker 值。因此通用服务端不是缺口，无需改动。
5. 主播卡片已有 `qualityName` 映射和未知值原样显示的 fallback，但目前只含抖音字符串档位。

根因边界因此只有两个：B 站选流返回值丢元数据，以及前端缺少 B 站数字档位文案。

## 3. 最小方案

### 3.1 后端：选流同时返回 URL 与实际 `qn`

把 `BilibiliLive::select_stream_url` 的返回值从 `LiveResult<String>` 改为
`LiveResult<(String, u32)>`：

- 未启用 CDN fallback：返回首选候选的 `(url, qn)`；
- 首选 URL 健康：健康检查允许返回重定向或 HLS 子列表 URL，但 `qn` 仍取首选候选；
- fallback 命中：返回该次循环候选的 `(healthy_url, candidate.qn)`；
- 所有 CDN 不可用：维持现有错误。

`check_stream` 解构结果，并写入：

```rust
recording_quality: Some(selected_qn.to_string())
```

同时把 `LiveStream.recording_quality` 的字段注释从“仅抖音填充”改成平台无关描述；数据类型与
序列化行为不变。

不使用 `self.qn`：它是请求偏好，平台可能返回相邻档位；也不扩大为通用候选/线路重构，
本问题所需元数据已经存在于 `BiliStreamCandidate`。

### 3.2 前端：扩充现有静态映射

直接给主播页现有 `qualityName` 增加已在 B 站配置页使用的档位：

| `recording_quality` | 显示 |
| --- | --- |
| `30000` | 杜比 |
| `20000` | 4K |
| `10000` | 原画 |
| `401` | 蓝光-杜比 |
| `400` | 蓝光 |
| `250` | 超清 |
| `150` | 高清 |
| `80` | 流畅 |
| `0` | 最低画质 |

保留 `qualityName[value] ?? value`，平台返回未来新增档位时仍能显示原始代码。

不为这组静态文案新建共享模块：配置页目前把值写在 `<Select.Option>` 中，强行抽象会扩大到
第三个文件；两处固定表的维护成本低于新增抽象。

## 4. 数据流

```text
B 站 API / master m3u8
  → BiliStreamCandidate { url, cdn, qn }
  → select_stream_url() 返回最终 (url, qn)
  → LiveStream.recording_quality = Some(qn.to_string())
  → Worker.recording_quality
  → GET /v1/streamers
  → qualityName 映射后显示“<画质> 录制中”
```

录制循环每次重新 `check_stream` 后已经会刷新 Worker 值，所以后续重新选流也自然更新，
无需新增状态或接口字段。

## 5. 验证

### 5.1 自动检查

在 `bilibili.rs` 内补一个最小异步测试，用本地 `TcpListener` 返回可控 HTTP 状态，不新增
mock 依赖。一个测试覆盖两条关键路径：

1. 首选候选健康，返回首选 URL 的 `qn`；
2. 首选候选失败、后续候选健康，返回后续候选的 URL 与 `qn`。

随后运行：

```bash
cargo test -p biliup downloader::live::bilibili
pnpm build
```

前端映射只是静态表，仓库也没有前端测试运行器；不为九个键引入测试框架，构建检查类型和页面
编译即可。

### 5.2 dev 环境验收

用一个可录制的 B 站直播间验证：

1. 进入 `Working` 后，`GET /v1/streamers` 的 `recording_quality` 非空且等于实际候选 `qn`；
2. 卡片显示对应中文标签；
3. 使用本地可控失败或可观测 fallback 场景确认接口报告命中候选，而非配置请求值；
4. 退出录制后 Worker 仍按现有流程清空画质。

公开回写只保留 `requested_qn`、`selected_qn`、是否发生 fallback 和结论，不记录房间号或地址。

## 6. 非目标

- 不改变 B 站的请求档位选择、CDN 排序或健康检查策略。
- 不把 B 站候选接入抖音现有的运行期多路线熔断；这是另一项行为变更。
- 不修改 `/v1/streamers` DTO、Worker 或通用下载流程；只校正 `LiveStream` 字段注释。
- 不新增配置、依赖、数据库字段或持久化历史。

## 7. 风险与兼容性

- `recording_quality` 已是 `Option<String>`，从 `None` 变为数字字符串不改 API schema。
- `qn = 0` 转成字符串 `"0"` 后在 JavaScript 中为 truthy，标签能正常渲染。
- 健康检查返回的重定向/子列表 URL 可能不同于候选原 URL；画质仍归属发起该检查的候选，
  因此必须显式携带候选的 `qn`，不能再通过最终 URL 反查。
