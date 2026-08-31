# P0 基线、冻结预算与合成样本 v1

以下门槛在首次测量前冻结，不能按结果调宽。P0/P1 是隔离合成验收；不声称真实录制通过。
受控基线程序：`cargo run -p biliup-observability --release --example baseline -- <rust|wheel|python>`。
每次 20,000 条、双 task 交错、有分段和进度字段；console 使用相同 formatter 后写 io::sink，
避免终端速度污染测量，文件为临时本地真实文件，guard flush。wheel 复用格式参数、测量期间
无跨日轮转，使用 never 文件替代 daily（轮转策略另由源码核对）。Python download/upload
共用同一局部格式，分别运行；不启动 Python/网络，入口宿主集成留给 P2。

## 固定门槛

| 项目 | 验收门槛 |
| --- | --- |
| 发射延迟 | release 单线程 20,000 事件 P99 ≤500µs；双路并发无关联错位 |
| 正常独立写入 | 2,000 事件/秒，持续 10 秒；0 丢弃；尾部≤2秒提交 |
| 无日志合成业务 vs 新采集 | 1ms 周期任务 10秒，P99 唤醒迟到增加≤10ms，无任务丢失 |
| 内存 | 队列≤4096条且保守计费≤16MiB；批次≤64条；release 进程峰值 RSS≤128MiB |
| 优先级 | 1/4条数和字节预留 WARN/ERROR；满时低级先拒绝，重要事件仍为尽力 |
| 单事件/诊断 | event JSON≤32KiB；诊断JSON≤16KiB；诊断尾部≤8192字节 |
| 写入 | 单 writer；batch≤64，空闲刷新≤100ms；busy≤50ms；每批≤3次尝试 |
| 关闭 | 默认≤2秒；超时返回未排空/丢弃 health，不无限 join |
| 查询 | 单页≤200；池≤2连接；单次≤250ms；20,000条查询P99≤250ms |
| 磁盘 | DB逻辑事件≤128MiB、附件≤32MiB、WAL≤16MiB；DB硬页上限192MiB |
| 本地总盘 | DB+WAL+附件（库内不重复计）≤208MiB；低水位256MiB；备份另需≥208MiB |
| 双写总预算 | 新库总208MiB + 旧文件≤256MiB = 464MiB；旧日志无字节硬限，P2须外部采样并超限停试点 |
| 清理 | INFO 30天、WARN/ERROR 90天、诊断7天；每批维护至多256条；删除不保证立即缩容 |

基线记录 emit/flush耗时、P95/P99、旧文件字节和丢弃；P1 用同机 release 合成调用者记录
上述指标，原始输出保存在 gitignore 已核实的 `data/observability-evidence/`，回执仅留脱敏指标。
忙锁/只读/SQLITE_FULL/无效路径/溢出/子进程强杀只能作用于 TempDir 内的新库。
SQLITE_FULL 用 max_page_count 限制隔离库模拟，不填满宿主磁盘。

## 场景目录

| 场景 | 输入/必须验证的事实 | 阶段 |
| --- | --- | --- |
| S01 正常链 | 开始→分段→上传→投稿；不同种类身份不合并 | P0定义；P1合成 |
| S02 双路交错 | 嵌套span、延迟record、切段后旧快照不变、异步/阻塞/回调传递 | P1 |
| S03 切片重连 | segment保持/attempt改变，DTS有单位；真实FLV路径另验 | P1载体；P3业务 |
| S04 预处理 | skipped/fallback/failed保留reason、原文件与artifact分离 | P1载体；P3业务 |
| S05 恢复 | 同segment多attempt、未知结果不改成功、幂等投影 | P1载体；P3业务 |
| S06 投稿 | waiting/unknown/succeeded；日志不改claim/watchdog | P1载体；P3业务 |
| S07 重启 | commit后读取、备份一致性、强杀未commit窗口未知 | P1 |
| S08 过滤 | off不构造字段、旧ERROR不误伤新INFO、原生不污染旧sink | P1合成；P2入口 |
| S09 故障 | 忙锁、只读、满页、启动失败、溢出、清理、附件/WAL上限、健康恢复 | P1 |
| S10 安全 | 未知Debug不执行、超长链、敏感键/URL/控制符、分块诊断脱敏 | P1 |

P3/P4 的长观察门槛仍是同最终版本每轮连续7天且≥10场，与以上10秒合成负载不互相代替。
