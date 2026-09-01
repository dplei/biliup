# P3/14 未迁移诊断分类清单（v1）

本清单回答两个不同问题：关键业务事实是否已有原生事件/指定权威源，以及剩余旧 tracing
诊断在迁移期和终态应如何处理。**保留桥接不等于原生覆盖，旧文本也不能补造身份或结果。**

机器可核对的文件级清单在 [diagnostic-classification-v1.json](diagnostic-classification-v1.json)，
校验命令为：

```bash
python3 scripts/structured_logging/check_diagnostic_classification.py
```

当前保守扫描覆盖 4 个运行时源码根、62 个含 tracing 级别宏的 Rust 文件和 554 个宏位置；
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
| server Streamlink | `core/live.rs` 可由平台 hint 或配置选择；分段仍用无稳定 S 的旧 `SegmentInfo::new`，命令行/逐行 stderr 只有旧输出 | 没有本批次原生证据 | **`coverage_gap`，不能写成“不支持”** |
| server YtDlp / YtArchive | `core/live.rs` 可选择；命令输出整体读入并把失败正文塞回自由错误，产物回调仍缺稳定 S/有界附件 | 没有本批次原生证据 | **`coverage_gap`，不能写成“不支持”** |
| danmaku 异步 recorder | start/stop/roll 的外层调用已发 auxiliary 事件 | 内部 spawned recorder/write/decode 失败仍只在 `danmaku::client` 打旧行，外层 start 成功后无法观察这些失败 | **`coverage_gap`；第六批只覆盖外层失败，不覆盖运行中失败** |
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
| Streamlink 命令/分段 | `streamlink.rs` | `coverage_gap` | 支持的运行时仍缺 S、明确关闭原因和有界命令失败；在补原生前保留桥接且不能通过任务 14 |
| YtDlp/YtArchive 命令/产物 | `ytdlp.rs` | `coverage_gap` | 支持的运行时仍缺 S/DA/退出附件，且当前把完整 combined output 放进自由错误；须先收敛隐私/容量边界 |
| danmaku 运行中 recorder/write 失败 | `danmaku/src/client.rs` | `coverage_gap` | 外层 start 返回成功后，spawned task 的错误不会触发第六批 auxiliary 事件；需要显式回调/健康状态，不能解析旧行补造 |
| 第三方完整 payload / 任意命令行 | 无 | `explicitly_unsupported` | 原始请求响应、cookie/token、签名 URL、完整命令行和协议 payload 不属于允许字段；只保留稳定枚举和脱敏摘要 |
| 通过文本/时间推断身份与结果 | 无 | `explicitly_unsupported` | 不从旧文本生成 task/R/U/S/attempt，也不因相近时间宣布成功或失败；未知保持 unknown |

## 文件级核对结果与阶段门槛

`diagnostic-classification-v1.json` 当前分组为：

- `native_covered`: 3 个原生发射模块；
- `retain_bridge`: 38 个混合业务/运维模块；
- `no_persistence`: 18 个请求、启动、响应或协议诊断模块；
- `coverage_gap`: 3 个受支持但仍缺关键原生事实的模块；
- `explicitly_unsupported`: 4 项能力边界，不以文件数量计。

结论：**分类清单本身完成，但任务 14 不能标 complete、不能开始首轮长观察。** 当前至少要先
补 Streamlink、YtDlp/YtArchive 和 danmaku 异步运行失败的原生边界，再跑 wheel/Python 生命周期、
真实/受控外部下载器及第六批辅助失败矩阵。任务 12 的真实断连、录制期 601 类型差异和 FFmpeg
秒级文件名碰撞等既有差异同样没有被本清单消除。
