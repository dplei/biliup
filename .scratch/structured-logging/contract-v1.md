# 事件契约 v1（P0 冻结）

适用范围：独立组件及后续采集；不代表业务原生覆盖已实现。身份不可从文本/时间推断。
schema_version=1；capture_kind=native|legacy_bridge。旧输出保持原调用点、级别、文案、
总过滤器、文件命名/轮转/flush 和旧页面；P1 不改任何应用入口。

## 字段与身份

- 每个组件实例显式接受持久的 instance_id；process_run_id 为每次初始化随机 UUID。
  event_uid 为每次创建随机 UUID；投递同一不可变 Event 保留 UID/occurred_at_ms/sequence。
  durable 投影可用业务 outbox 持久 UUID 创建事件；由调用者保留原事件，日志库不担保跨库事务。
- occurred_at_ms/ingested_at_ms 为 UTC 毫秒；sequence 是组件运行内原子序号，不是因果证明。
  id 是 SQLite 自增提交游标。来源 target 最长 128 字节；schema/来源字段不能由 span 覆盖。
- level=TRACE|DEBUG|INFO|WARN|ERROR；category=system|recording|processing|upload|submission|auth|audit。
  event_name 为 category 开头的 ASCII 小写点分名称（≤96 字节）；桥接固定 system.legacy。
  message 为 ≤512 字节中文摘要，桥接可保留有界旧文案，不把桥接统计为原生覆盖。
- outcome=executed|skipped|fallback|failed|waiting|succeeded|unknown|recovered|cancelled；
  reason_code 为 ≤64 字节 ASCII 小写下划线代码，域枚举见覆盖清单。未知不得写成功。
- Context 是显式可克隆字段载体；live_streamer_id=房间配置，streamer_info_id=录制场次，
  upload_session_id=投稿会话，segment_id=文件创建时分配的稳定身份，missing_id=登记账本，
  download_attempt_id/upload_attempt_id=各自尝试，task_id=独立 CLI/嵌入调用或页面上传请求。
  ID ≤128 字节，仅 ASCII 字母/数字/下划线/短横/点/冒号；ID 可未知，绝不互相代用。
  business ID 查询同时传 instance_id。P1 不为业务分配 segment_id，不新增业务字段。
  P3 起 segment_id 在文件创建时分配（`LifecycleFile::create`），随关闭回调与登记账本持久化；
  空字符串 ID 表示「调用方没有这个身份」，既不入库也不计 rejected，与格式非法的 ID 区分。
- 独立上传既有文件时没有录制账本，不按路径补造 segment_id / upload_session_id。
  P3/14 的上传事件采用另一种完整形态：task_id + original_file + segment_order +
  upload_attempt_id；segment_order 是本次输入列表从 1 起的序号，用于区分脱敏后同 basename
  的不同输入，不能跨 task 当作持久身份。排队/断点复用时 attempt 可空；每次预上传重试
  分配新 attempt。checkpoint 只证明既有记录被复用，不重新宣称本次完成了远端传输。
- 页面 `POST /v1/uploads` 返回新增 `task_id`，仅用于事件关联；HTTP 成功表示接受请求，
  不表示上传/投稿成功，也不是可恢复的持久任务凭证。首文件的主播反查只供模板渲染，
  不给同批文件推断录制身份。后台显式传 task 与 subscriber；被强杀没有结束事件时保持未知。
  空视频结果记 `submission.decided skipped/no_videos`，模板构建返回错误记
  `failed/studio_build_failed`；仅真正发起投稿才记 started/completed。
- streamer_name 为脱敏显示快照（≤256 字节）；original_file/artifact_file 只保留脱敏 basename
  （≤256 字节），转码不覆盖 segment_id。file/line 仅辅助来源，不作身份。
- fields 是受控扁平标量集合，未知键、嵌套 JSON、负的非负量丢弃并计数；数值以
  _ms/_secs/_bytes 为单位，不能不带单位地添加时长/大小。最终允许键由 Fields 单一入口维护。
  事件覆盖子 span，子覆盖父；on_record 更新未来事件，入队后快照不再变。

### P3/14 HLS 增量

- 复用 R 或 T、DA 与文件层 S。独立 Rust/wheel/Python 下载在每次调用分配 task 和 DA；
  `recording.stopped` 仍是 task 终态，不要求重复 DA。服务端从 DownloadConfig 传入 R/DA。
- `recording.hls_gap`：WARN、executed/media_sequence_gap，带当前文件 S、`media_sequence`、
  `previous_media_sequence`、`missing_segments`。三者为非负整数，missing 必须大于 0 且
  等于 current−previous−1；表示列表序列中漏过的媒体片段数量，不换算成 gap_ms。
- `recording.hls_discontinuity`：WARN、executed/hls_discontinuity，带不连续边界后新建文件的
  S 与 media_sequence；旧文件关闭仍是 unknown，新边界事件补充原因，不把它冒充网络断连。
- `recording.disconnected` 的 HLS 错误边界带 stage=hls、R/T/DA、failed；原因由类型映射为
  invalid_playlist/http_error/read_timeout/source_io/transport_error，不解析自由错误文本。
  当时不可可靠取得的 S、时长保持未知。它说明本次下载未完成，不能据此推断直播下播。
- 服务端已知 HLS 后缀直接解析列表；未知后缀保留原有 FLV 探测回落。HLS 只有完整收到至少
  一个非空媒体片段才触发已有 reconnect 上下文，不因读到列表头就恢复；不制造 FLV 静默测量。
  配置切片及取消继续复用文件层；取消原因须在下载 future 被释放前写入 close handle。
- 本批未添加 HLS 解密、fMP4 初始化段支持、分段媒体内容校验或新的网络恢复状态机。列表
  解析成功不等于媒体可播放；已收字节与无损播放分别验收，不承诺跨进程去重。

### P3/14 外部下载器（FFmpeg）增量

- 外部分段的目标文件由本进程选定，`recording.segment_created` 与 `recording.segment_closed`
  都是真实观测，复用 R/T + DA 和文件层的 S；关闭原因取本次进程的结束方式：取消优先记
  `user_cancel`，退出码 0 且配置了时长/大小上限记 `split_limit`，0/255 记 `stream_end`，
  其余非零码记 `transport_error`，被信号结束且未取消保持 `unknown`。**退出码 0 区分不出
  「切到上限」与「刚好同时下播」**，这是 ffmpeg 的口径，不额外推断。
- 内部分段由 ffmpeg 自己创建文件，进程外看不到创建时刻，因此**只发 `segment_closed`，
  不补造 `segment_created`**；消费方不得要求两者成对。分段列表行写的是相对列表文件的
  名字，管道输出时只有 basename，按配置的输出目录还原。
- 内部分段的关闭原因按配置取值（配了 `-segment_time` 记 `split_limit`，否则 `unknown`）。
  **最后一段实际是流结束时关闭的，拿到列表行时无法与切片区分**，整场结束原因由
  `DownloadStatus` 与上层 `recording.stopped` 说明，不在分段事件里改写。
- `-strftime 1` 的文件名只有秒级精度：同一秒关闭的两段会拿到同一个名字并被 ffmpeg 覆盖，
  分段列表出现重复行。重复行与改名失败一律记 `recording.segment_closed` `failed`/`unknown`
  并继续下载，不结束整场录制，也不冒充一个已关闭的分段。
- `processing.command_failed`：WARN、failed/`process_failed`，带 `stage`
  （`ffmpeg_external`/`ffmpeg_internal`）、`exit_code`（信号退出时缺省）、`total_bytes`，
  有界 stderr 尾部作为**附件**按 event_uid 关联，事件字段不复制第三方输出。退出码 0/255
  和主动取消都不是外部命令失败。含 URL/凭据线索的行整值脱敏，因此尾部常见 `[REDACTED]`。
- 该事件经本次调用的采集器直接写出（附件无法走 tracing 字段），只取当前 dispatch 上的
  采集器，不搜索全局运行，也不从业务回调里初始化存储。

### P3/14 入口生命周期与凭据健康增量

- `system.started` / `system.stopped` 描述**一次运行**：两个 CLI 是一个进程，Python 绑定是一次
  嵌入调用。运行自带 task_id，**与其中的录制/上传业务 task 是两个身份，互不代用**；同一进程内
  两者只能靠 process_run_id 关联，重叠的嵌入调用可能共享 run，运行 id 才是区分它们的依据。
- 字段：`stage` 为入口（`rust_cli`/`wheel_cli`/`python_download`/`python_upload`），`command`
  为**解析后的子命令固定词**（新增允许键，text），不是原始参数文本；进程标识与版本由
  `process_run_id`/`app_version` 承载，不重复写入 fields。参数值、cookie 路径、账号一律不带。
- 结果三态：正常返回 executed/`shutdown`；返回错误 failed/`entry_failed`；**被取消或 panic 展开
  记 unknown/`entry_interrupted`**，不推断成功也不算作失败。**进程被强杀不执行任何析构，
  因此没有结束事件——缺失就是缺失，不补造。**
- `auth.health_changed` 只由 `cookie_health` 的状态机在两次跃迁时发出：连续鉴权失败达阈值记
  WARN failed/`authentication_failed`，恢复记 INFO recovered/`authentication_recovered`，带
  `platform` 与 `count`（当次连续失败数）。**单次操作失败不改变健康状态，也不得声称凭据失效。**
- `auth.operation_failed`：WARN、failed，带 `platform` 与 `stage`。`stage=live_check` 来自直播间
  检查（监控轮询与录制断流复查同一口径），登录类入口的 stage 是各自的操作名。原因由
  `classify_error` 定型：`authentication_failed`/`transport_error`/`server_error`/`invalid_response`。
  **错误文本只用于分类，随即丢弃**，不进字段也不进摘要。
- 去抖窗口内的重复鉴权失败只更新记录、不累加计数，因此**也不发事件**：事件条数等于被计数的
  失败数，而不是重试风暴的次数。非鉴权类失败不进入去抖，与旧的每次告警行 1:1。
- 包装 CLI 结果时按 `Debug` 而非 `Display` 渲染错误再分类：`error_stack::Report` 的 Display
  只有顶层 context，按它分类会把所有被包裹的失败都定型成 `invalid_response`。
- 登录成功不发原生事件：`auth.health_changed` 的 recovered 属于状态机，一次成功的登录不等于
  平台健康跃迁；「这次登录跑完了且正常结束」由该入口的 `system.stopped` 回答。

### P3/14 持久审计投影与辅助诊断增量

- `upload_recovery_audit` 仍是恢复拒绝事实的权威来源；加法字段 `event_uid` 保存 UUID，仅用于
  向独立事件库投影。新审计先落业务表再尽力投影；采集关闭、队列拒绝或日志库故障都不能改变
  恢复判定。迁移前的空 UID 由回放先写回业务表，再发事件，不按时间或路径生成身份。
- `audit.operation_projected` 使用业务表中的 UID 和原 `created_at`，WARN；stage 保留稳定的业务
  审计原因，结果映射为 failed/`source_missing`、skipped/`session_finalized`，未知新增原因保守记
  unknown/`audit_reason_unknown`。启动时分页回放一次，运行中有界重放最近行；事件库按 UID 幂等，
  业务审计不会随普通事件保留策略删除，必要时可重新生成视图。它不是跨库原子提交承诺。
- 预处理外部进程统一复用 `processing.command_failed`：stage 为 `loudnorm_measure`、
  `loudnorm_transcode`、`timestamp_detect`、`timestamp_remux`、`timestamp_reencode`、`hook_remux`
  或 `custom_hook`。非零退出带真实 `exit_code`；spawn/read/wait 失败没有退出码，不补造。
  `ffmpeg_scan` 同时保留旧解析所需的 64KiB 尾部和独立附件的 8KiB 脱敏尾部；原先继承 stderr
  的 remux/reencode/hook 路径继续 tee 到原输出，原先静默捕获的扫描/转码路径仍不新增旧行。
- 弹幕、封面与非命令 hook 失败使用 `recording.auxiliary_failed` 或
  `processing.auxiliary_failed`，WARN/failed，stage 区分 start/stop/roll、下载/读取/渲染/上传和
  pre/downloaded hook；原因只用 `danmaku_failed`、`cover_failed`、`hook_failed`、`source_io`。
  原始错误仍只在原有旧调用点输出，新事件不复制 URL、请求响应、命令文本或自由错误文本。

### P3/14 外部下载器（Streamlink / yt-dlp / ytarchive）增量

- Streamlink 用 `--output` 写本进程选定的 `.part`，因此 `recording.segment_created` 与
  `recording.segment_closed` 都是真实观测，复用 R/T + DA 和文件层 S。关闭原因：取消优先记
  `user_cancel`；退出码 0 且配了 `--hls-duration` 记 `split_limit`，0/130/143/255 记
  `stream_end`，其余非零码记 `transport_error`，被信号结束且未取消保持 `unknown`。
  **和 ffmpeg 一样，退出码 0 区分不出「切到上限」与「刚好同时下播」。**
- 进程退出后没有 `.part`：如实记一次 `recording.segment_closed` `failed`/`unknown`，
  不冒充一个已关闭的分段，也不改写旧的 `DownloadStatus`。改名失败同样记 failed 后原样报错。
- yt-dlp / ytarchive 由外部工具自己创建、命名并搬运文件，进程外看不到创建时刻，因此
  **只发 `segment_closed`，不补造 `segment_created`**；一次调用对应一个分段，关闭原因固定
  `stream_end`。**`stop()` 只置标记、不杀进程**，被停止的调用不发分段事件，这是既有语义。
- `processing.command_failed` 的 stage 新增 `streamlink`、`ytdlp`、`ytarchive`。命令起不来时
  记 `spawn_failed` 且**没有** `exit_code`；非零退出记 `process_failed` 带真实退出码。
  被 `stop()` 请求过的下载是预期结束，不记命令失败。
- 第三方输出只以有界脱敏附件出现：单行超限省略、尾部封顶 8 KiB、含 URL/凭据线索的行整值
  脱敏。**yt-dlp/ytarchive 原先把完整 combined output 塞进自由错误，本批改为有界脱敏摘要**
  （首个致命行，没有则尾部，并注明原始字节数）；错误类型、返回值与既有分类分支不变。
- yt-dlp 内置的直播封面下载失败改发 `recording.auxiliary_failed`，stage
  `live_cover_download`、原因 `cover_failed`；旧告警行保留，事件不带 URL 或错误文本。

## 安全和边界

字段先允许列表再格式化：未知 Debug 不调用；允许值有界格式化（超限停止），不 stringify
任意请求/响应。消息/错误/诊断遇到 cookie、authorization、token、secret、password、
credential、签名/URL 等敏感线索时整值替换；控制字符变空格。没有标签的任意秘密无法自动识别，
调用方仍禁止把原始请求/响应或凭据当摘要。字段丢弃、脱敏、截断分别可见。
旧上传链的 `ResponseData` 调试文本按原始响应整值脱敏，不能因其中 `aid` 是
`Number(...)` 而绕过账号清理；旧 sink 不改，桥接采集与离线导出均执行此规则。
错误字段≤1024 字节；诊断流按≤1024 字节行检查，过长整行省略，避免分块边界泄露；
保留首个 error/fatal 行和≤8192 字节脱敏尾部、原始总字节、exit_code、truncated。
列表不加载诊断正文。JSONL 通过 JSON serializer 生成，换行无歧义。

默认关闭；独立 INFO 最低级别，桥接显式开启。Layer 不安装全局 subscriber，不用 enabled
全局挡掉其他 sink；以 per-layer Filter 做路由，旧 sink 排除 biliup::event 原生 target。
SQLx/observability 内部 target 永不回流。spawn 用 instrument + dispatcher，阻塞/回调/Actor
传 Context 和 Emitter；绝不跨 await 持有 span.enter guard。关闭先拒绝新事件，限时排空。

普通事件最多尽力保存。队列溢出/写入故障/关闭超时按级计损，health 不依赖日志库。
强杀未提交窗口只能给上界或未知；已提交高水位只能在 COMMIT 后发布。审计事务仍在业务库。
