# biliup-observability

独立、默认关闭的结构化日志底座。P1 **不接入** Rust CLI、wheel、Python、Web 或业务数据库。
源契约与预算见 [contract-v1](../../.scratch/structured-logging/contract-v1.md) 和
[baseline-budget](../../.scratch/structured-logging/baseline-budget.md)。不安装全局 subscriber。

```rust,no_run
use biliup_observability::{*, sqlite::*};
use std::time::Duration;

let storage = StoreOptions::new("data/observability.sqlite3");
// 父目录由宿主显式创建。instance_id 由宿主持久保存，不能每次启动随意换。
let mut runtime = Runtime::start("local-instance", env!("CARGO_PKG_VERSION"),
    Options { enabled: true, ..Options::default() },
    move || SqliteStore::open(storage.clone()))?;
let emitter = runtime.emitter();
emitter.emit_with(Level::Info, || {
    let mut event = Draft::new("system.started", "独立任务开始");
    event.context = Context(Fields::new().with("task_id", "synthetic-task"));
    event
});
let health = runtime.shutdown(Duration::from_secs(2));
assert!(health.closed && !health.shutdown_timed_out);
# Ok::<(), StorageError>(())
```

## 宿主/采集契约

- `Runtime::start` 立即返回；线程创建/配置错误返回 `Err`，数据库打开/迁移错误进入 `Health`。
  调用者持有 Runtime 至任务结束，克隆 Emitter/Context 跨 channel、Actor 和同步回调。
  SQL/磁盘工作只在消费者线程运行。`SqliteStore::open/write/backup` 本身是阻塞接口，禁止
  在业务异步线程上直接调用；应通过 Runtime factory 或独立维护进程使用。
- tracing 用 `registry().with(CaptureLayer::new(emitter).filtered())`；旧 sink 自己使用
  `with_filter`，同时排除 `legacy_output == false` 的原生 target。**不能把旧 EnvFilter
  继续挂为 registry 总过滤器再期望新层穿透它**。P2 才改入口组合。
- 原生 tracing 使用 `target: "biliup::event"` + `event_name`，中文message；桥接显式开启。
  异步任务同时传 span (`Instrument`) 和 dispatcher (`WithSubscriber`)；当前/全局宿主
  的 dispatcher 不能靠线程ID猜。阻塞/回调用 Emitter+Context，不在await两侧持enter guard。
- off 使用 `emit_with` 或 `.filtered()` 不构造昂贵字段。动态关闭后重新开启，只保证未来
  span/显式Context；关闭期间未采的span字段不能回填。事件快照不可修改，submit重试保UID。
- 创建durable投影时 `project(outbox_uuid, original_ms, ...)`；先由业务事务/outbox保存，
  成功投影不是业务提交。普通事件的随机UID不能自动把两个内容相同的事件合并。
- 仅允许扁平、带单位的字段。原始响应、cookie、URL不允许作为日志内容；未知Debug不格式化。
  检测到敏感线索的整值省略，无法识别无标签秘密，调用方仍承担语义允许列表责任。
  诊断用 DiagnosticCapture 持续push，跨chunk拼接再脱敏，长行整行省略，最坏缓存约10KiB。

## 故障、维护与恢复

- 队列≤4096且保守计费≤16MiB，1/4容量保留WARN/ERROR；事件≤32KiB、诊断≤16KiB。
  所有级别最终仍可能丢弃。Health列出各级dropped、in_flight、队列字节峰值、最近提交、
  storage_failures/recoveries。恢复不清零损失计数；这些计数是本次运行内存状态，不是durable审计。
- 单写连接WAL，busy 50ms，每批≤64、最多3次尝试。写入事务失败全部回滚，UID去重。仅COMMIT
  后更新committed_id，不提前广播。日志内部SQL关闭statement日志，也从Layer按target排除。
- SQLite application_id 防止误迁移已有业务/外来库。日志迁移仅本crate migrations。
  同一数据库只由一个Runtime所有者运行；不在网络盘多机共享。独立读取者可有多个。
- 30/90/7天清理，每轮各类最多256条；每批及空闲约1秒维护。空间到75%提前删附件/低级事件，
  达硬逻辑上限拒写；删除不缩小主文件，页可复用。维护可能产生保留缺口；Page.gap是保守标志，
  pruned_through是删除过的最大ID，不表示低于它的记录全不存在。
- 4KiB页、最多192MiB主库，WAL预留8MiB事务余量，超过水位先短checkpoint；读者pin住就拒写。
  独立只读pool最多2连接，每页≤200，等待≤250ms；SQLite VM还在约200ms中断长扫描，
  连接归还时移除旧deadline，避免后续查询误受影响。事件列表不载诊断正文。
- Unix低水位检测用statvfs，默认256MiB，无法测量时保守拒写；其他OS目前未提供空间探针，
  不宣称通过。SQLITE_FULL测试限制隔离库页数，不填满宿主磁盘。
- 默认关闭deadline建议2秒。超时丢弃未取队列、返回in_flight未知并停止等线程；已开始的
  I/O可能稍后完成，不能把返回当作“所有后台工作都已终止”。自定义Consumer必须有界；
  它永久阻塞时线程最多持有一个受限批次，不阻塞调用者无限join。Drop不隐式等待。
- 每次写入器打开记录dirty，正常close清理；强杀/异常重开通过unclean_shutdowns给出未知
  丢失窗口，不伪造结束事件。已提交记录在进程强杀后可查；NORMAL synchronous不承诺掉电
  零丢失。普通队列不是业务账本。
- 备份使用显式 `SqliteStore::backup`（VACUUM INTO），目标必须不存在、预留≥208MiB，
  不复制运行中的主文件。返回成功后备份可单独只读打开；超时/失败的目标属于不完整备份，
  保留排查但不得恢复。恢复时停止该库写入者，选择一个新的路径放已验证备份并重新启动；
  不覆盖在线库，也不碰业务数据库/旧日志。缩容另在离线维护窗口操作，当前不自动VACUUM。

## 验证

```sh
cargo test -p biliup-observability
cargo clippy -p biliup-observability --all-targets -- -D warnings
cargo run -p biliup-observability --release --example baseline -- wheel
cargo run -p biliup-observability --release --example acceptance -- data/observability-evidence/P1.json
```

所有数据是合成的，强杀只终止测试创建的子进程；所有SQLite故障仅作用于TempDir。没有网络、
账号、真实录制、业务库、页面或旧日志写入开关的副作用。完整真实入口双写验收属于P2。
