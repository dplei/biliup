# 短分段恢复与上传 601 冷却

## 安全行为

- `upload_rate_gate_enabled = true` 时，全进程的自动上传、缺失补传、手动恢复和网页上传共享同一个 `pre_upload` Gate。
- B 站返回 `601` 后，Gate 立即进入冷却；冷却状态写入 `upload_rate_gate`，重启不会清空剩余冷却。
- 冷却结束只放行一个探测请求。成功后恢复，仍为 `601` 时按指数退避继续冷却。
- 进程内上传分段通道有 64 项上限；冷却或上传积压触顶时，新文件先写 durable `Deferred` 清单，不会占用无界内存或覆盖旧任务。
- 可恢复短片先按媒体参数分组。直接 concat 失败后会逐片 remux 再 concat；全部失败时转为 `Deferred`，不会逐片上传原文件。

## 配置

```toml
preserve_recoverable_short_segments = true
recoverable_short_segment_mode = "merge_or_defer"
recoverable_short_batch_max_files = 60
recoverable_short_retry_interval_secs = 900

upload_rate_gate_enabled = true
upload_min_request_interval_secs = 2
upload_601_initial_cooldown_secs = 60
upload_601_max_cooldown_secs = 1800
```

关闭 `preserve_recoverable_short_segments` 会恢复为拒绝低于体积阈值的短片；不会恢复“合并失败后自动逐片上传”的危险旧路径。关闭 `upload_rate_gate_enabled` 才会绕过上传 Gate，生产环境不建议关闭。

## 状态与审计

- `GET /v1/health/upload-rate`：Gate 状态、连续 601 次数、剩余冷却、等待任务数、累计及最近一分钟 `pre_upload` 计数。
- `GET /v1/recovery-batches`：已写入数据库的短片恢复批次。
- `.biliup-recovery-<batch-id>.json`：与原片同目录的 fsync 后 durable 清单。即使数据库索引暂时失败，这个清单仍是恢复依据。
- `recoverable_short_batch`：恢复批次状态、原文件列表、下次建议重试时间和最近错误。

只有合并产物上传成功且视频引用 durable 落库后，后处理才会接触原始短片。`Deferred` 批次不会自动删除原片。

## 灰度 FLV → HLS

只对目标主播覆写以下配置，确认抖音响应里确实有同画质 HLS 候选后启用：

```yaml
route_health_enabled: true
douyin_route_failover: true
douyin_protocol_fallback: true
douyin_quality_fallback: false
```

同一 FLV RouteKey 在故障窗口内连续两次传输失败后会熔断并优先选同画质 HLS。观察 `download_resilience_session_summary` 中的 `flv_to_hls_switches`、`successful_flv_to_hls_switches` 和短片数量。

## 敏感日志

抖音请求错误在进入日志和 Cookie 健康状态前会删除 URL query，并遮盖 Cookie、Token、`msToken`、`a_bogus`、`verifyFp`、签名和过期字段。只有明确的 401/403、登录态无效或风控拒绝会累积 Cookie 鉴权告警；网络、证书、DNS、超时和 5xx 分别计数但不会提示更换 Cookie。
