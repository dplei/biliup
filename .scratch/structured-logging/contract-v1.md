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
  download_attempt_id/upload_attempt_id=各自尝试，task_id=独立 CLI/嵌入调用。
  ID ≤128 字节，仅 ASCII 字母/数字/下划线/短横/点/冒号；ID 可未知，绝不互相代用。
  business ID 查询同时传 instance_id。P1 不为业务分配 segment_id，不新增业务字段。
  P3 起 segment_id 在文件创建时分配（`LifecycleFile::create`），随关闭回调与登记账本持久化；
  空字符串 ID 表示「调用方没有这个身份」，既不入库也不计 rejected，与格式非法的 ID 区分。
- streamer_name 为脱敏显示快照（≤256 字节）；original_file/artifact_file 只保留脱敏 basename
  （≤256 字节），转码不覆盖 segment_id。file/line 仅辅助来源，不作身份。
- fields 是受控扁平标量集合，未知键、嵌套 JSON、负的非负量丢弃并计数；数值以
  _ms/_secs/_bytes 为单位，不能不带单位地添加时长/大小。最终允许键由 Fields 单一入口维护。
  事件覆盖子 span，子覆盖父；on_record 更新未来事件，入队后快照不再变。

## 安全和边界

字段先允许列表再格式化：未知 Debug 不调用；允许值有界格式化（超限停止），不 stringify
任意请求/响应。消息/错误/诊断遇到 cookie、authorization、token、secret、password、
credential、签名/URL 等敏感线索时整值替换；控制字符变空格。没有标签的任意秘密无法自动识别，
调用方仍禁止把原始请求/响应或凭据当摘要。字段丢弃、脱敏、截断分别可见。
错误字段≤1024 字节；诊断流按≤1024 字节行检查，过长整行省略，避免分块边界泄露；
保留首个 error/fatal 行和≤8192 字节脱敏尾部、原始总字节、exit_code、truncated。
列表不加载诊断正文。JSONL 通过 JSON serializer 生成，换行无歧义。

默认关闭；独立 INFO 最低级别，桥接显式开启。Layer 不安装全局 subscriber，不用 enabled
全局挡掉其他 sink；以 per-layer Filter 做路由，旧 sink 排除 biliup::event 原生 target。
SQLx/observability 内部 target 永不回流。spawn 用 instrument + dispatcher，阻塞/回调/Actor
传 Context 和 Emitter；绝不跨 await 持有 span.enter guard。关闭先拒绝新事件，限时排空。

普通事件最多尽力保存。队列溢出/写入故障/关闭超时按级计损，health 不依赖日志库。
强杀未提交窗口只能给上界或未知；已提交高水位只能在 COMMIT 后发布。审计事务仍在业务库。
