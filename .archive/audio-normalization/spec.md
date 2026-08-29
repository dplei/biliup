# Spec：自动响度标准化与样片音量推子

状态：已实现并完成本机 FFmpeg 冒烟验收  
分支：`dev`

## 1. 目标

录制源的声音普遍偏小时，用户不需要理解 LUFS、真峰值、响度范围、AAC 码率等专业参数，
只需要：

1. 打开「自动增强录音音量」。
2. 播放系统保留的一段样片。
3. 像操作音量推子一样上下拖动，试听到满意为止。
4. 保存；后续录像自动保持相近的最终听感。

产品内部继续使用 FFmpeg `loudnorm` 做自动响度标准化，而不是简单地对每段录像固定乘以一个
音量倍数。这样不同直播源、不同场次原本的声音大小发生变化时，最终响度仍能收敛到同一个目标。

## 2. 最终用户流程

### 2.1 首次开启

1. 用户进入全局设置，打开「自动增强录音音量」。
2. 系统立即使用推荐档位处理后续分段；推荐档位对应内部目标 `-16 LUFS`。
3. 如果当前没有样片，页面显示「还没有样片」。
4. 用户点击「从下一段录像更新样片」。
5. 页面显示「等待下一段完整录像」，该状态在服务重启后仍保留。
6. 任意主播的下一段有效录像完成后，系统从录像中部截取 30 秒音频，生成新的推荐档位样片。
7. 页面状态变为「样片已就绪」，用户可以播放和调节。

打开增强开关与生成样片互不阻塞：没有样片时也按推荐档位正常标准化、上传。

### 2.2 调节音量

界面只展示一个纵向推子：

```text
       更响
        ▲
        │
        ●
        │
        ▼
       更轻

    ▶ 播放样片
    恢复推荐音量
```

- 中点为「推荐」，内部偏移量为 `0 dB`。
- 向上最多 `+4 dB`，向下最多 `-6 dB`。
- 步长为 `1 dB`。
- 默认界面不显示 LUFS；鼠标提示或折叠的高级信息可显示「推荐 +2 dB」等描述。
- 拖动时使用浏览器 Web Audio API 即时调整播放增益，不调用服务端、不重新编码样片。
- 松开推子只更新表单值；仍由现有全局设置的「保存」动作持久化，避免每次拖动都写配置。
- 点击「恢复推荐音量」把偏移量恢复为 `0 dB`。

### 2.3 更新与删除样片

- 「从下一段录像更新样片」：创建一个持久的待截取标记。
- 「取消等待」：删除待截取标记，不影响现有样片。
- 新样片只有在完整生成并校验成功后才原子替换旧样片。
- 截取失败时保留旧样片，并继续等待下一段有效录像。
- 「删除样片」：删除样片文件，但不关闭自动响度标准化。
- 全局始终最多保留一个样片，不按主播保存多份。

## 3. 产品与技术决策

### 3.1 用户配置只保留两个字段

在 `Config` 中加入：

```yaml
audio_normalization_enabled: false
audio_normalization_offset_db: 0
```

语义：

- `audio_normalization_enabled`：默认 `false`，升级后行为不变。
- `audio_normalization_offset_db`：整数，后端强制限制在 `-6..=4`。
- 实际目标响度为 `-16 + offset_db` LUFS。
- 字段由现有 `ConfigPatch` 自动支持主播 `override`；首版前端只做全局设置，不在主播弹窗暴露。
- 不新增数据库列：全局配置继续存在 `configuration` JSON，主播覆盖继续存在 `livestreamers.override`。

以下参数是代码内安全常量，不出现在普通界面和配置文件中：

```text
BASE_TARGET_LUFS = -16.0
LOUDNESS_RANGE_LU = 11.0
TRUE_PEAK_DBTP = -1.5
AUDIO_CODEC = aac
AUDIO_BITRATE = 192k
SAMPLE_RATE = 48000
NORMALIZATION_CONCURRENCY = 1
SAMPLE_DURATION_SECONDS = 30
SAMPLE_AUDIO_BITRATE = 96k
```

如果以后确实需要高级调节，再另开范围；首版不提前暴露参数。

### 3.2 处理时机

标准化不进入拉流过程，也不依赖当前未接通的 `opt_args`。统一放在完整分段生成之后：

```text
分段完成
→ 现有 segment_processor
→ 尝试生成待更新的样片
→ 自动响度标准化
→ 现有时间戳检测/修复
→ 上传
→ durable 落库
→ 现有 postprocessor
```

理由：

- 不让 FFmpeg 音频处理失败扩大成录制中断。
- 同时覆盖 `stream-gears`、`sync-downloader`、FFmpeg 等下载器。
- 在自定义 `segment_processor` 之后处理，确保上传的最终媒体满足目标响度。
- 在时间戳修复之前处理，让时间戳修复仍然检查实际准备上传的最终文件。

### 3.3 双遍 loudnorm

每个分段使用双遍 `loudnorm`：

1. 测量遍只解码主音轨，把 JSON 输出到 stderr。
2. 转换遍复制视频及其它兼容流，只将主音轨重编码为 AAC。

测量遍需要解析：

```text
input_i
input_lra
input_tp
input_thresh
target_offset
```

任一字段缺失、为 `inf`/`-inf`/`nan` 或超出合理范围，都视为测量失败。

转换遍使用测量结果并设置 `linear=true`；若 FFmpeg 判定无法线性达到目标，它会退到动态模式。

### 3.4 失败时上传原片

响度增强是附加能力，不能阻断录制和上传：

- 未发现音轨：跳过，上传原片。
- FFmpeg 不存在：记录错误，上传原片。
- 测量失败：删除临时件，上传原片。
- 转换失败或产出为空：删除临时件，上传原片。
- 上传失败：删除本次产生的全部临时件，缺失队列仍登记原片路径。
- 上传成功但 durable 落库失败：删除临时件、保留原片。
- 上传并落库成功：删除临时件，原片按现有 postprocessor 规则处理。

不把标准化结果覆盖到原片，也不把标准化临时路径写入缺失分段队列。这样每次补传都从原片重新生成，
不会对 AAC 临时件反复有损编码，也不需要增加 `normalized` 数据库状态。

## 4. 文件与模块落点

### 4.1 Rust 后端

新增：

```text
crates/biliup-cli/src/server/common/audio_normalization.rs
crates/biliup-cli/src/server/api/audio_normalization.rs
```

修改：

```text
crates/biliup-cli/src/server/common/mod.rs
crates/biliup-cli/src/server/config.rs
crates/biliup-cli/src/server/common/upload.rs
crates/biliup-cli/src/server/api.rs（或当前实际集中挂载路由的文件）
```

### 4.2 前端

新增：

```text
app/ui/AudioNormalizationControl.tsx
```

修改：

```text
app/ui/plugins/global.tsx
app/lib/api-streamer.ts（如全局配置类型在其它文件，则改实际定义处）
```

### 4.3 配置示例与文档

修改：

```text
public/config.yaml
public/config.toml
out/config.yaml
out/config.toml
docs/config.toml
```

`out/` 若由构建脚本生成，则只改源模板并运行既有生成流程，不手工维护生成物。

## 5. 具体实现步骤

### 步骤 1：加入配置字段和边界校验

在 `Config` 增加：

```rust
#[serde(default)]
pub audio_normalization_enabled: bool,

#[serde(default)]
pub audio_normalization_offset_db: i8,
```

增加一个纯函数：

```rust
fn effective_audio_target_lufs(config: &Config) -> f64
```

行为：

1. 将偏移量限制为 `-6..=4`。
2. 返回 `-16.0 + offset`。
3. 保存配置时也执行校验，非法值返回 4xx，而不是仅在使用时静默截断。
4. 反序列化缺少新字段的旧配置时，得到 `false` 和 `0`。

测试：

- 空 YAML/JSON 使用关闭状态和零偏移。
- `enabled: true` 可正常往返序列化。
- `-6`、`0`、`4` 通过。
- `-7`、`5` 被配置保存接口拒绝。
- 主播 `ConfigPatch` 可以覆盖这两个字段。

完成标准：仅加入字段、不接入处理时，现有所有配置测试仍通过。

### 步骤 2：抽象 FFmpeg 执行边界

在 `audio_normalization.rs` 定义可替换的执行接口，避免单元测试依赖本机 FFmpeg：

```rust
#[async_trait]
pub trait AudioFfmpegRunner: Send + Sync {
    async fn probe(&self, input: &Path) -> AppResult<AudioProbe>;
    async fn measure(&self, input: &Path, target: LoudnessTarget) -> AppResult<String>;
    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        target: LoudnessTarget,
        measured: &LoudnessMeasurement,
    ) -> AppResult<()>;
}
```

生产实现使用 `tokio::process::Command`：

- 参数逐项传入，禁止拼接 shell 命令。
- `stdin` 设为 null。
- `kill_on_drop(true)`。
- stderr 设置长度上限，日志不得无限放大。
- 错误信息包含退出码和经过截断的 stderr，但不包含可能带 token 的输入 URL；这里输入应始终是本地路径。

`probe` 至少返回：

```rust
struct AudioProbe {
    duration_seconds: Option<f64>,
    primary_audio_stream: Option<usize>,
}
```

完成标准：Fake runner 能在不启动 FFmpeg 的情况下驱动后续全部状态分支。

### 步骤 3：实现 loudnorm JSON 解析

定义：

```rust
struct LoudnessMeasurement {
    input_i: f64,
    input_lra: f64,
    input_tp: f64,
    input_thresh: f64,
    target_offset: f64,
}
```

解析逻辑不能假设 stderr 只有 JSON。FFmpeg 会在 JSON 前后输出普通日志，应从最后一个完整 loudnorm JSON 对象中解析。

校验：

- 五个字段都存在。
- 字符串数值可以转换为 `f64`。
- 所有值 `is_finite()`。
- `input_i` 等明显异常值被拒绝。

测试夹具覆盖：

- 标准 FFmpeg 输出。
- JSON 前后存在日志。
- 缺字段。
- 非数字字符串。
- `-inf`、`inf`、`nan`。
- stderr 中存在其它 JSON，确保不会误取。

完成标准：解析器是纯函数，并有独立单元测试。

### 步骤 4：实现单文件标准化

对外接口：

```rust
pub async fn normalize_for_upload<R: AudioFfmpegRunner>(
    source: &Path,
    target_lufs: f64,
    runner: &R,
) -> NormalizationOutcome;
```

返回：

```rust
enum NormalizationOutcome {
    Original { reason: OriginalReason },
    Normalized {
        path: PathBuf,
        artifact: TempArtifact,
        measurement: LoudnessMeasurement,
    },
}
```

实现顺序：

1. 检查源文件存在且非空。
2. 探测主音轨；无音轨返回 `Original(NoAudio)`。
3. 获取全局标准化 semaphore，首版并发固定为 1。
4. 执行测量遍。
5. 解析并校验测量结果。
6. 在原片同目录创建带随机后缀且保留真实扩展名的临时输出，例如：
   `foo.audio-normalized-a1b2c3.part.flv`。
7. 执行转换遍：`-map 0`、默认 `-c copy`，只覆盖主音轨为 AAC 192k/48k。
8. 检查输出存在且非空。
9. 再探测一次，确认输出仍包含视频流（音频文件输入除外）、主音轨和有效时长。
10. 返回临时路径及负责清理它的 artifact guard。

容器兼容：

- FLV 输出保持 FLV。
- MP4 输出补齐现有需要的 `aac_adtstoasc`/`faststart` 规则时，应以真实 FFmpeg 冒烟测试为准，
  不机械复制仅适用于其它转换方向的 bitstream filter。
- TS/MKV 保持原容器。
- 不改变原文件名、mtime 或内容。

完成标准：成功产生可播放的标准化临时文件；任何失败都返回原片分支且不残留临时件。

### 步骤 5：重构上传前媒体准备流程

将当前 `upload_single_file_with_repair` 拆成两个阶段：

```rust
prepare_media_for_upload(...)
upload_prepared_media(...)
```

建议的数据结构：

```rust
struct PreparedMedia {
    original_path: PathBuf,
    upload_path: PathBuf,
    artifacts: Vec<TempArtifact>,
    normalization: NormalizationSummary,
    timestamp_repair: RepairSummary,
}
```

接入规则：

1. 功能关闭时直接进入现有时间戳处理，行为与当前完全相同。
2. 功能开启时先生成标准化临时文件。
3. 时间戳检测以标准化结果为输入；标准化失败则仍以原片为输入。
4. 时间戳修复产生的临时文件也登记到 `PreparedMedia.artifacts`。
5. 网络上传 permit 只包住真正上传阶段，不包住响度分析和转码。
6. `PreparedMedia` 在所有返回路径显式执行清理；`Drop` 再做一次同步 best-effort 兜底。
7. 缺失队列始终使用 `original_path`。

必须替换三处既有调用：

- 正常录制分段上传。
- `recover_due_missing_segments` 自动补传。
- `manual_recover_missing_segment` 手动补传。

补传路径同样读取对应主播应用 override 后的有效配置，不能只拿全局 `Config`；否则正常上传和补传会出现不同响度。

完成标准：三条路径共用同一媒体准备函数，不复制 FFmpeg 流程。

### 步骤 6：实现样片文件存储与一次性认领

样片目录使用工作目录下的固定位置，不把 `/opt` 写死在代码中：

```text
<working-directory>/audio-normalization/
├── sample.m4a
├── capture-next
└── capture-in-progress-<uuid>
```

职责：

- `sample.m4a`：唯一持久样片。
- `capture-next`：用户要求从下一段录像更新样片的空标记文件。
- `capture-in-progress-*`：某个分段通过原子 rename 认领后的标记。

实现接口：

```rust
struct AudioSampleStore { root: PathBuf }

impl AudioSampleStore {
    async fn status(&self) -> AppResult<AudioSampleStatus>;
    async fn arm_capture(&self) -> AppResult<()>;
    async fn cancel_capture(&self) -> AppResult<()>;
    async fn try_claim_capture(&self) -> AppResult<Option<CaptureClaim>>;
    async fn commit_sample(&self, claim: CaptureClaim, temp: &Path) -> AppResult<()>;
    async fn retry_later(&self, claim: CaptureClaim) -> AppResult<()>;
    async fn delete_sample(&self) -> AppResult<()>;
}
```

并发与恢复规则：

- 多个主播同时完成分段时，只有原子 rename 成功者获得 claim。
- 生成成功：临时样片原子替换 `sample.m4a`，再删除 claim。
- 生成失败：claim 改回 `capture-next`，保留旧样片。
- 启动时发现超过合理时间的 `capture-in-progress-*`，恢复成 `capture-next`。
- 所有接口都只操作固定目录和固定文件名，不接受前端传入路径。

完成标准：重启、并发分段和生成失败都不会丢掉“等待更新样片”的意图。

### 步骤 7：从下一段录像生成基准样片

在 `segment_processor` 完成之后、正式标准化上传之前调用：

```rust
maybe_capture_reference_sample(source_path, sample_store, runner)
```

流程：

1. 尝试认领 `capture-next`；没有标记时零额外 FFmpeg 开销。
2. 探测时长和音轨。
3. 时长超过 30 秒时，从中部截取 30 秒；不足 30 秒时使用完整有效时长。
4. 对截取音频固定按基准目标 `-16 LUFS` 做标准化，不叠加当前用户 offset。
5. 输出为 AAC 96k 的 `m4a`，不保留视频。
6. 重新探测样片，确认存在音轨、时长大于 0、文件非空。
7. 原子提交为 `sample.m4a`。
8. 失败时恢复待截取标记，让下一段继续尝试。

样片固定使用基准 `-16 LUFS` 的原因：前端推子只需要实时施加 `offset_db`，试听结果就近似于后续录像的最终目标；更换保存的 offset 不需要重新生成样片。

样片生成失败不能改变该分段的标准化和上传结果。

完成标准：点击更新后，下一段有效录像能生成可播放样片；失败不影响上传且下一段会重试。

### 步骤 8：增加样片 HTTP API

新增以下接口：

```text
GET    /v1/audio-normalization/sample/status
GET    /v1/audio-normalization/sample
POST   /v1/audio-normalization/sample/capture
DELETE /v1/audio-normalization/sample/capture
DELETE /v1/audio-normalization/sample
```

状态响应：

```json
{
  "sample_ready": true,
  "capture_pending": false,
  "updated_at": "2026-08-25T12:00:00Z",
  "size_bytes": 384000
}
```

接口规则：

- 样片响应为 `audio/mp4`。
- 添加 `Cache-Control: no-store`，避免更新后仍播放浏览器缓存的旧样片。
- 前端播放 URL 附加 `?v=<updated_at>` 作为额外 cache buster。
- 没有样片时 `GET sample` 返回 404。
- 重复 arm 是幂等成功。
- 重复 cancel/delete 是幂等成功。
- 不接受文件路径、FFmpeg 参数或目标 LUFS 等输入。
- 复用现有 API 的错误响应格式。

集成测试使用只挂载这些路由的子 Router，避免拖入不需要的完整服务状态。

完成标准：接口能完整表达“空、等待、就绪、等待更新但旧样片仍可播放”四种状态。

### 步骤 9：实现前端播放器与纵向推子

新增 `AudioNormalizationControl.tsx`，职责限定为：

- 展示开关、样片状态、播放器、推子和样片操作按钮。
- 不自行保存全局配置，由父级现有表单统一保存。
- 轮询样片状态仅在 `capture_pending=true` 时启用，建议 5 秒一次；状态就绪后立即停止。
- 页面卸载时停止轮询并释放 Web Audio 节点。

Web Audio 实现：

1. 使用原生 `<audio>` 承载播放、暂停和进度控制。
2. 用户第一次主动点击播放时创建 `AudioContext`，满足浏览器自动播放策略。
3. 通过 `MediaElementAudioSourceNode → GainNode → destination` 播放。
4. 将 dB 转换为线性增益：`gain = 10^(offset_db / 20)`。
5. 拖动时用 `setTargetAtTime` 或短 ramp 平滑更新，避免音量突变产生点击声。
6. 组件销毁时断开节点并关闭 `AudioContext`。

推子交互：

- 使用可访问的 slider 组件或原生 `input[type=range]` 旋转成纵向。
- 设置 `aria-orientation="vertical"`、最小值 `-6`、最大值 `4`、步长 `1`。
- 支持上下方向键，每次移动 1 dB。
- 中点显示吸附感，但不强制吸附；`0` 档加明显刻度和「推荐」标签。
- 颜色只表达区间：`-6..2` 绿色、`3` 黄色、`4` 橙色；不使用恐吓性的错误红色。

状态文案：

- 无样片：「还没有样片，可从下一段录像自动截取。」
- 等待且无旧样片：「等待下一段完整录像……」
- 就绪：「播放样片并上下拖动试听。」
- 等待更新且有旧样片：「正在等待新样片；当前仍播放旧样片。」
- 生成失败不会在 UI 固定成失败态，因为后端会自动等待下一段；错误细节留日志。

完成标准：拖动时试听立即变化，保存页面后刷新仍保持同一档位。

### 步骤 10：日志、清理与运行保护

每个标准化结果记录一条结构化日志：

```text
audio_normalization=completed|skipped|fallback
file=<local path>
target_lufs=-14
input_lufs=-27.4
offset_db=2
elapsed_ms=...
output_size_bytes=...
reason=...
```

安全与资源限制：

- 标准化全局并发固定为 1，避免多个长分段同时占满 CPU 和磁盘 IO。
- 不把网络上传 semaphore 占用在 FFmpeg 处理期间。
- FFmpeg stderr 截断后写日志。
- 临时输出必须与原片位于同一文件系统，便于空间规划和清理。
- 启动时清理超过 24 小时的 `*.audio-normalized-*.part.*` 孤儿文件；扫描范围只能是录像输出目录，不能递归扫工作区或挂载根。
- 清理前验证文件名模式与文件年龄，不删除原始录像。
- 样片目录创建失败时只禁用样片功能，不禁用正常标准化。

首版不新增 webhook：同一环境缺少 FFmpeg 时，每段都推送会形成通知风暴。日志中保留明确原因即可。

完成标准：模拟每个错误出口后，没有本次创建的临时件遗留，原片仍存在。

### 步骤 11：配置示例和用户说明

配置示例只写两个公开字段：

```yaml
# 自动统一录音响度；默认关闭。开启后视频不重编码，只重编码音频。
#audio_normalization_enabled: true
# 相对推荐音量的偏移，网页音量推子会自动维护；范围 -6 到 +4。
#audio_normalization_offset_db: 0
```

网页说明必须包含：

- 视频不重编码。
- 音频会重新编码。
- 处理时需要一份临时文件，磁盘峰值会增加。
- 处理失败自动上传原片。
- 样片只是试听参考，删除样片不会关闭增强功能。

不在普通文档列出所有隐藏 FFmpeg 参数，避免重新把复杂度转嫁给用户。

## 6. 测试计划

### 6.1 Rust 单元测试

- 配置默认值、边界及 `ConfigPatch`。
- loudnorm stderr JSON 提取与非法数值。
- 无音轨、测量失败、转换失败、输出为空、输出探测失败。
- 成功时返回临时路径，失败时返回原片。
- artifact 在所有退出路径清理。
- 文件名含空格、中文、引号时参数传递正确。
- sample store 的 arm/cancel/delete 幂等。
- 两个并发 claim 只有一个成功。
- stale in-progress 标记能恢复。
- 新样片失败时旧样片不被覆盖。

### 6.2 FFmpeg 集成测试

用 FFmpeg 生成一段低音量测试视频，标记为 `#[ignore]` 或按仓库现有本机 FFmpeg 测试约定执行：

- 输入约 `-30 LUFS`，输出达到目标值 `±1 LU`。
- 输出真峰值不明显超过目标；AAC 编码允许设置合理容差。
- 视频 codec 与输入一致，证明 `-c:v copy` 生效。
- 输出时长与输入误差在允许范围内，音画同步不漂移。
- FLV、MP4、TS 至少各有一个冒烟用例；生产主要格式 FLV 为必测。
- 30 秒样片是可播放的 `m4a`，且不包含视频流。

### 6.3 上传链路测试

- 功能关闭时不启动响度 FFmpeg。
- 正常上传使用标准化临时件，durable 后清理临时件并按原路径执行 postprocessor。
- 上传失败时删除临时件，缺失队列记录原片。
- 自动补传重新从原片生成标准化临时件。
- 手动补传与自动补传使用相同的有效配置。
- 标准化失败时三条路径都上传原片。
- 标准化输出仍会经过时间戳检测。

### 6.4 HTTP 与前端验证

- 五个样片接口覆盖正常、404 和幂等行为。
- 样片响应 MIME 与缓存头正确。
- 开关关闭/打开后表单值正确保存。
- 推子键盘操作、上下边界、恢复推荐档位。
- 拖动时 GainNode 值按 dB 正确换算。
- 只有等待截取时才轮询状态。
- 更新样片后播放器 URL 变化并播放新内容。
- `npm`/`pnpm` 对应的 `tsc --noEmit`、lint、build 通过。

## 7. 实施顺序与提交边界

建议按以下顺序提交，每一步都保持可编译：

1. **配置骨架**：两个配置字段、校验、示例和单测。
2. **标准化核心**：runner、probe、JSON 解析、双遍 loudnorm、Fake runner 测试。
3. **上传接入**：统一 `PreparedMedia`，覆盖正常上传和两条补传路径。
4. **样片存储**：固定目录、marker 认领、崩溃恢复和样片生成。
5. **样片 API**：状态、读取、更新等待、取消和删除。
6. **前端控件**：开关、播放器、纵向推子、即时试听和状态轮询。
7. **集成验证**：真实 FFmpeg 冒烟、三条上传路径、文档与构建检查。

不要在同一个提交里顺带修复当前 `opt_args` 未接通的问题；本功能不依赖它，混入会扩大回归范围。

## 8. 验收标准

- 默认关闭，升级后已有录制和上传行为不变。
- 用户界面只需要操作开关、播放按钮和纵向推子。
- 推子保存后，未来分段的目标响度按 `-16 LUFS + offset_db` 计算。
- 不重编码视频，只重编码主音轨。
- 输入声音大小不同的两段录像，处理后综合响度都落在目标值 `±1 LU` 内。
- 标准化失败不会阻断录制、上传、自动补传或手动补传。
- 上传失败时持久队列只保存原片路径，不保存临时路径。
- 样片更新失败时旧样片仍可播放，并会等待下一段重试。
- 并发完成多个分段时只生成一次样片。
- 服务重启后仍能继续等待样片更新。
- 所有成功与失败路径都不会长期遗留标准化临时文件。

## 9. 明确不做

- 首版不提供 LUFS、LRA、真峰值、码率、采样率的高级配置页面。
- 首版不按主播保存多份样片。
- 首版不从浏览器上传整段录像作为样片。
- 首版不在录制过程中实时转码。
- 首版不修改下载器 `opt_args` 行为。
- 首版不对背景噪声做降噪，不做压缩器、门限器、EQ 或人声增强。
- 首版不保证样片试听与完整双遍 loudnorm 在瞬态细节上逐采样一致；样片用于确定总体听感档位。
- 首版不新增响度失败 webhook 或独立监控页面。

## 10. 上线观察与回滚

上线后先只对一个低风险主播通过 override 开启，观察至少一场完整直播：

1. 标准化耗时相对分段时长是否可接受。
2. 上传队列是否因双遍分析产生明显积压。
3. 临时磁盘峰值是否可接受。
4. B 站成片的响度、真峰值和音画同步是否正常。
5. FLV/MP4 时间戳修复率是否出现异常变化。

确认后再开启全局开关。回滚只需关闭 `audio_normalization_enabled`；样片文件可以保留，不影响旧流程。

如果生产观察发现双遍分析造成持续上传积压，再单独评估单遍动态 loudnorm；不要在首版同时实现两套模式和选择器。

## 11. 实施记录

- 已加入两个公开配置字段、`-6..=4` 保存校验及主播 override 支持。
- 已实现 FFprobe 探测、双遍 `loudnorm`、视频流复制、AAC 音频编码、单并发和失败上传原片。
- 已接入正常上传、自动补传和手动补传；缺失队列继续持久化原片路径。
- 已实现样片一次性认领、失败重试、陈旧认领恢复、原子替换及五个 HTTP 接口。
- 已实现全局设置中的样片播放器、纵向推子、Web Audio 即时试听和等待状态轮询。
- 已通过 Rust 全量库测试（178 passed，1 ignored）、样片 HTTP 集成测试、TypeScript 检查与 Next.js 生产构建。
- 已安装 Homebrew FFmpeg 9.0.1；低音量 FLV 冒烟输入 `-51.55 LUFS`、输出 `-15.97 LUFS`，视频仍为 H.264、音频为 AAC。MP4/TS 仍建议随部署灰度继续观察。
