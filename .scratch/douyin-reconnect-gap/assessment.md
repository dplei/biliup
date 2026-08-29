# 抖音断连缺口压缩：总览与任务拆分

Status: ready-for-agent

来源：[`dplei/biliup#5`](https://github.com/dplei/biliup/issues/5)（2026-08-28/29 观测）。
同一场的三个大空洞根因不同，已拆到 [`dplei/biliup#6`](https://github.com/dplei/biliup/issues/6)，不在本轮范围。

本文只拆分实现步骤，不含代码改动。

---

## 1. 现象一句话

某抖音高码率房间（origin 画质、约 14 Mbps、30 分钟一段约 3.1 GB）整场被切成 15 个分 P，
**11/11 个定时分段边界全部有缺口**，单次 22~40 秒，一场累计约 4 分 56 秒，观感上每个分 P 交界处"跳针"。
低码率房间的分段是 0 秒无缝的，问题只出现在会被上游掐断的连接上。

## 2. 已确认根因

### 2.1 上游（不可控）

抖音 CDN 对这条高码率拉流连接有 **~30 分钟（实测 1817~1848s）的强制寿命上限**，到点由远端主动断开。
证据横跨 4 个日期、3 台 CDN 主机：

| connected_ms | received | host 前缀 | error |
|---|---|---|---|
| 1819453 | 3.14 GB | pull-flv-t11 | `Reset(StreamId(1), INTERNAL_ERROR, Remote)` |
| 1818189 | 3.14 GB | pull-flv-t11 | 同上 |
| 1817775 | 3.13 GB | pull-flv-t11 | 同上 |
| 1833566 | 3.26 GB | pull-q5 | Reset |
| 1848270 | **1.65 GB** | pull-x0-t5 | Reset |
| 1820725 | 3.12 GB | pull-t5 | `Io(InvalidData, "cannot decrypt peer's message")` |

要点：**是时间维度不是字节维度**（1.65 GB 也断，3.26 GB 也断）；低码率连接可以活 3~4 小时，
说明不是全局连接问题。

biliup 配置的定时分段也是 1800s，于是每次断连都恰好落在分 P 边界后 ~20 秒。

### 2.2 biliup 侧：22~40 秒里绝大部分是可省的

#### (a) 停顿检测阈值设成了 30 秒 —— 原 issue 的定性需要修正

原 issue 说"唯一的保护是 30 秒超时"，对；但把它定性成了兜底。实际上
[`httpflv.rs:361`](../../crates/biliup/src/downloader/httpflv.rs) 的
`timeout(Duration::from_secs(30), self.resp.chunk())` 包的是**单次 chunk 等待**，每收到一个 chunk 就重置，
语义精确等于"连续 N 秒一个字节都没收到"。

**它就是原 issue 建议里要新增的那个"码流停顿看门狗"，只是阈值设错了。**
本次事故里它没生效，仅仅因为上游自己在 ~20 秒时先报了错。不需要新写机制，把 30 改小并可配置即可。

#### (b) 分段后新建的文件必然停在 13 字节

缓存只在**下一个关键帧**到来时刷盘（[`httpflv.rs:196-212`](../../crates/biliup/src/downloader/httpflv.rs)）；
分段时压进缓存的是 `onMetaData + AAC seq header + H264 seq header + 当前关键帧`
（[`httpflv.rs:214-258`](../../crates/biliup/src/downloader/httpflv.rs)），这批要等下一个关键帧才落地。
连接死在下一个关键帧之前 → 新文件停在 9 + 4 = 13 字节 → 判 `HeaderOnly` 删除
（[`download.rs:190-201`](../../crates/biliup-cli/src/server/common/download.rs)）。

关键在于：**缓存里那一段是自带 metadata 和序列头的完整可播前缀**，收尾时刷出去就是合法短分段。

#### (c) 重连路径约 3 秒

断开后重新 `check_stream` 解析候选 + 固定 `RETRY_BASE_DELAY = 2s` 退避
（[`download.rs:34/64/1055-1063`](../../crates/biliup-cli/src/server/common/download.rs)），实测约 3 秒。

#### (d) 缺口统计口径漏掉了大头 —— 原 issue 的定性也需要修正

`estimated_missing` **已经存在**（[`download.rs:921/1118/1153`](../../crates/biliup-cli/src/server/common/download.rs)），
但它只累加 `check_elapsed + backoff`，即**从报错之后**开始算，结构上看不见报错之前那 ~20 秒静默。
所以整场丢近 5 分钟，日志里的 `estimated_missing_ms` 只有 30 秒量级。
不是"没有统计"，是"统计口径漏掉了大头"。

## 3. 收益测算

单次边界缺口的构成与压缩后：

| 组成 | 现状 | 01+02+04 之后 |
|---|---|---|
| 分段后到上游停发 | ~1.5s（数据在缓存里被丢弃） | ~1.5s（`02` 刷盘保住） |
| 静默到报错 | ~20s | 停顿阈值（建议 6~8s） |
| 报错到新文件写入 | ~3s | <1s |
| **实际丢失** | **22~40s** | **~7~9s** |

再往下要靠 `06` 的 make-before-break。

## 4. 任务拆分

| # | 标题 | 依赖 | 独立可发布 |
|---|---|---|---|
| 01 | 码流停顿超时可配置并调低 | — | 是 |
| 02 | 连接收尾时刷出缓存，保住分段后那一小段 | — | 是（但见其前置条件） |
| 03 | 缺口统计口径修正与可观测 | — | 是 |
| 04 | 重连快路径 | — | 是 |
| 05 | 上游连接寿命诊断实验 | — | 研究型 |
| 06 | make-before-break | 05 | 否 |

**最小止血 = 01**，单条就能把 22~40s 压到 ~11s。

## 5. 关于原 issue 建议第 5 条（诊断实验）

值得做，而且应该在投入 `06` 之前做。倾向性判断：证据里连接寿命散布在 1817~1848s，抖动 31 秒；
如果是分段动作触发的死亡，应该锁死在 1800+ε 而不是散在 30 秒宽的窗口里，所以"上游寿命上限"这个
结论大概率成立。但 `06` 成本足够高，花一晚上确认划算。见 `05`。
