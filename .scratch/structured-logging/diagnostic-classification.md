# P3/14 未迁移诊断分类清单（v1）

本清单回答两个不同问题：关键业务事实是否已有原生事件/指定权威源，以及剩余旧 tracing
诊断在迁移期和终态应如何处理。**保留桥接不等于原生覆盖，旧文本也不能补造身份或结果。**

机器可核对的文件级清单在 [diagnostic-classification-v1.json](diagnostic-classification-v1.json)，
校验命令为：

```bash
python3 scripts/structured_logging/check_diagnostic_classification.py
```

当前保守扫描覆盖 4 个运行时源码根、62 个含 tracing 级别宏的 Rust 文件和 555 个宏位置；
计数包含 `src/` 文件中的测试模块，所以只作为「是否漏掉文件」的上界，不冒充生产调用次数。
校验器要求每个含宏文件恰好落入一个默认处置组；新增/删除/移动到新文件会使校验失败。
它不能替代语义复核：同一文件可能同时有已迁移业务边界和应保留的内部诊断，文件级决定描述
的是**剩余旧诊断的默认处置**。

## 决定词表

| 决定 | 含义 | 能否计入 C01–C14 原生覆盖 |
| --- | --- | --- |
| `native_covered` | 该调用点本身就是冻结契约的原生事件发射器，或同一业务边界已有类型化原生事件 | 可以，但仍须对应场景证据 |
| `retain_bridge` | 旧输出在双写期仍有排障价值；以 `legacy_bridge` 显式查询，待 P5/P6 决定停写/删除 | 不可以 |
| `no_persistence` | 请求内进度、启动提示、轮询/分块/协议 chatter，或已有更合适的状态/HTTP 返回；终态不应追加进事件库 | 不可以，也不算缺口 |
| `explicitly_unsupported` | 因隐私、容量或语义边界明确不提供的能力；必须写出理由，不能伪装成以后会有 | 不适用，但不能借此移除受支持业务路径 |
| `coverage_gap` | 受支持路径的关键事实仍只有自由文本或根本未向上返回；保持在分母并阻止任务 14 完成 | 不可以 |

## 支持入口与下载运行时核对

| 入口 / 运行时 | 当前原生状态 | 运行证据边界 | 分类结论 |
| --- | --- | --- | --- |
| Rust CLI | 入口生命周期及下载/上传业务 task 已接 | 入口成功/失败进程实跑；上传/HLS 有受控矩阵，远端样本只覆盖既有记录的部分 | 已迁移；剩余旧文案 `retain_bridge` |
| wheel CLI | 与 Rust CLI 共用 `run()` / `Invocation` 和业务实现 | 上传/HLS 受控矩阵有记录；第五批后未另起 Python 进程验证入口生命周期 | 代码已接，入口运行证据仍 partial |
| Python download / upload | 局部 subscriber 内显式传 task | 既有上传/HLS 矩阵；第五批后未重新起 Python 进程核对 lifecycle | 代码已接，入口运行证据仍 partial |
| server StreamGears（FLV/HLS） | R/DA/S、断连/恢复、HLS gap/discontinuity 已接 | 受控断流/HLS 与一场真实录制；真实断连样本仍缺 | 已迁移，真实矩阵 partial |
| server FFmpeg（内/外分段） | 分段身份、关闭原因与有界失败附件已接 | 本地合成媒体与失败注入 | 已迁移，真实平台矩阵 partial |
| server Streamlink | 第九批接入：目标文件由 `--output` 选定，创建/关闭是真实观测，带 S/DA、关闭原因与有界退出诊断 | 只有与外部进程无关的判定口径单元测试；本机未安装 streamlink，无实跑样本 | 代码已接，运行证据 pending |
| server YtDlp / YtArchive | 第九批接入：产物解析后发 `segment_closed`（不补造 created），失败带 stage/exit_code 与有界脱敏附件，自由错误不再整段回灌 | 只有隐私/容量边界单元测试；本机未安装 yt-dlp/ytarchive，无实跑样本 | 代码已接，运行证据 pending |
| danmaku 异步 recorder | 第十一批接入：后台任务终止经宿主注入的观察回调发 `recording.auxiliary_failed`/`danmaku_runtime`，panic 与取消记 `danmaku_aborted` | 真实「启动即死」（输出不可写）在测试中实跑到 `danmaku_output_failed`；真实断连/协议失败样本待自然采集 | 代码已接，运行证据 partial；逐条 write/decode 失败改归 `no_persistence` |
| cover / hook | 下载、渲染、读取、上传、非命令 hook 及命令失败已接 | 编译/全量回归；真实失败逐项触发仍待补 | 代码已接，运行证据 partial |
| durable recovery audit | 业务表稳定 UID，事件库幂等投影 | 临时业务库 + 真实事件 SQLite 重放 | 已迁移；真实恢复/outbox 矩阵 partial |

这次源码核对修正了第四至第六批回执沿用的历史措辞：Streamlink/YtDlp 不是“当前配置永远
不会选中的死分支”。Niconico、General、Twitch、YouTube 等插件会给出对应 hint/runtime
options，服务端也允许显式 downloader 配置；因此它们必须继续留在任务 14 分母。

## 剩余诊断逐类处置

| 诊断族 | 代表源码 / 权威源 | 决定 | 理由与后续 |
| --- | --- | --- | --- |
| C01–C13 的旧业务文案 | `observe*.rs` 对应的录制、上传、投稿、认证、审计事件；业务账本 | `native_covered` + 旧行 `retain_bridge` | 原生事件回答身份/阶段/结果；旧行只用于双源核对，P5 停旧写入后不要求继续保存 |
| 上传进度、分块重试、heartbeat | `UploadActivity`、attempt lease/history、上传健康快照 | `no_persistence` | 这是可覆盖状态或内部重试，不逐块堆普通事件；最终失败/恢复由 C07/C08 权威源回答 |
| 租约、投稿/恢复扫描器内部轮次 | 业务 SQLite 的 lease/intent/attempt/audit 表 | `retain_bridge` | 扫描失败与调度细节仍可排障，但业务是否到期/领取/完成只读业务表，桥接不能改写状态 |
| API/router/service 初始化提示 | HTTP 状态/错误响应、`system.started/stopped` | `no_persistence` | 请求局部失败直接返回调用者；成功注册路由、构造 service 等过程提示不是关键追加事件 |
| 旧日志 WebSocket、文件 tail/轮转错误 | `server/api/ws.rs`、旧 `/logviewer/legacy` | `no_persistence` | 这是 P5/P6 要停用/移除的旧传输自身，不能把“读旧日志失败”再写入新日志形成递归依赖 |
| 日志存储自身故障 | observability health snapshot、受限 stderr | `no_persistence`（普通事件） | queue/storage/commit 故障由独立健康源回答；写入器不能把自身故障无条件回写同一队列 |
| HTTP/平台提取与协议 DEBUG | live extractor、danmaku protocol、代理/header/url 输出 | `no_persistence` | 常含签名 URL、token、header 或高频协议细节；只持久化上层稳定结果，原文不进入事件库 |
| 投稿/登录原始响应 | `biliup::uploader::{bilibili,credential}`，上层 submission/auth 事件 | `no_persistence` | 远端响应可能含账号/稿件标识；成功、失败、不确定由类型化事件回答，不保存原响应全文 |
| FFmpeg/loudnorm/timestamp/custom hook | `DiagnosticCapture` + `processing.command_failed` | `native_covered`；完整输出 `explicitly_unsupported` | 只保存 8 KiB 有界脱敏附件、总字节和截断信息；不承诺完整 stdout/stderr 归档 |
| Streamlink 命令/分段 | `streamlink.rs` + `processing.command_failed` | `native_covered`；逐行 `[streamlink] …` 输出 `retain_bridge` | S/DA、关闭原因与退出诊断已原生；`--hls-duration` 下退出码 0 区分不出「切到上限」与「刚好同时下播」，与 ffmpeg 同口径 |
| YtDlp/YtArchive 命令/产物 | `ytdlp.rs` + `processing.command_failed` | `native_covered`；`运行: …`、清理告警 `retain_bridge` | 只发 `segment_closed`；完整 stdout/stderr 仍 `explicitly_unsupported`，错误正文换成有界脱敏摘要 |
| danmaku 后台任务终止 | `danmaku/src/client.rs` 的退出观察回调 + `recording.auxiliary_failed` | `native_covered`；旧 `Recorder error` 行 `retain_bridge` | 恰好上报一次，含 panic/取消；原因只从错误类型映射，不解析文本，事件不带原文 |
| danmaku 逐条 write/decode 失败 | `danmaku/src/client.rs` 的 `warn!`/`debug!` | `no_persistence` | 丢的是单条弹幕不是整场结果，且频率可达弹幕级；整场是否还在录由上一行的终止事件回答 |
| 第三方完整 payload / 任意命令行 | 无 | `explicitly_unsupported` | 原始请求响应、cookie/token、签名 URL、完整命令行和协议 payload 不属于允许字段；只保留稳定枚举和脱敏摘要 |
| 通过文本/时间推断身份与结果 | 无 | `explicitly_unsupported` | 不从旧文本生成 task/R/U/S/attempt，也不因相近时间宣布成功或失败；未知保持 unknown |

## 文件级核对结果与阶段门槛

`diagnostic-classification-v1.json` 当前分组为：

- `native_covered`: 3 个原生发射模块；
- `retain_bridge`: 41 个混合业务/运维模块；
- `no_persistence`: 18 个请求、启动、响应或协议诊断模块；
- `coverage_gap`: 0 个——第十一批闭合最后一个后归零；
- `explicitly_unsupported`: 4 项能力边界，不以文件数量计。

结论：**源码缺口已归零，但任务 14 仍不能标 complete。** 第十一批闭合了最后一个
`coverage_gap`（danmaku 后台任务终止），门槛条件因此满足，可以开始首轮实际双写观察——而
**观察本身尚未进行**，14 的完成取决于观察结果而不是本清单。
代码接入不等于运行证据：Streamlink 与 YtDlp/YtArchive 在本机没有第三方工具，弹幕只实跑到
「输出不可写」这一种终止，真实断连、协议失败与外部工具样本都要在实际运行中自然积累。
任务 12 的真实断连和录制期 601 类型差异同样没有被本清单消除。
