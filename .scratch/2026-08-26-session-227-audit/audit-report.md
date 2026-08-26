# 会话 #227 事故恢复审计报告

Status: needs-info（等待 [server-data-collection.md](server-data-collection.md) 的采集结果填空）

对应任务：[07-migration-backfill-and-incident-recovery.md](../../.doc/2026-08-26-upload-reliability/07-migration-backfill-and-incident-recovery.md) 第 5、6 节。

## 约束重申

- 本报告只做只读审计，**不建议也不执行**任何 B 站 edit 接口调用、分 P 删除或数据库写操作。
- 「必须人工确认」类结论不代表批准执行，只代表已排除自动化能安全处理的可能性。
- 所有结论必须能追溯到 [server-data-collection.md](server-data-collection.md) 的具体采集项，禁止凭经验/常见模式代替证据。

## 一、会话 #227 概况

| 字段 | 值 |
| --- | --- |
| aid |  |
| bvid |  |
| status |  |
| submit_state |  |
| submit_attempts |  |
| submit_claim_token |  |
| submit_claimed_at |  |
| blocked_signature / blocked_count |  |
| last_submit_error |  |
| videos_json 是否可解析 |  |
| created_at / updated_at |  |

`videos_json` 原文（用于逐条比对 order/filename/title）：

```json
[]
```

若 `videos_json` 非空但解析失败：在此明确标注「按 07 号任务文档结论，不得当作尚未投稿处理，
本报告不建议任何重新上传，只做闸门状态说明」，并跳过依赖 `videos_json` 解析的比对项。

## 二、本地生命周期账本（upload_missing_segment）

| id | segment_order | status | attempts | line_index | lifecycle_version | normalized_file_path | last_error |
| --- | --- | --- | --- | --- | --- | --- | --- |
|  |  |  |  |  |  |  |  |

同场未绑定行（采集清单 3.2/3.3 命中但 `upload_session_id` 不是 227 的行）：

| id | 推测所属 order | 依据 | 是否已在其他会话终结 |
| --- | --- | --- | --- |
|  |  |  |  |

## 三、三个切片文件的落盘状态

| 时间戳 | 文件路径 | 存在 | 大小 | ffprobe 时长 | ffprobe 是否报错 |
| --- | --- | --- | --- | --- | --- |
| 22:29:32 |  |  |  |  |  |
| 22:59:56 |  |  |  |  |  |
| 23:30:19 |  |  |  |  |  |

## 四、判读规则

比对维度固定为以下四项，**任意结论都必须写明依据来自哪一项，不能只凭其中一项就下结论**：

1. **segment_order**：本地账本记录的顺序号 vs B 站分 P 顺序（`pages[].page`）。
2. **源路径**（`normalized_file_path`）：同一源路径只应该产生一条成功记录；出现两条不同
   `id` 但 `normalized_file_path` 相同的 `succeeded`/`conflict` 行，是重复的强证据。
3. **Video.filename**（远端）：`video_json`/`videos_json` 里的 B 站返回文件名标识，
   与本地文件是一一对应关系；两条本地行若指向同一个远端 `filename`，即为同一次上传的重复记录。
4. **title**：仅用于辅助定位，**不能单独作为重复或缺失的判定依据**——同一场直播的分段标题
   经常是同一模板生成，标题相同大概率只是巧合。

判读矩阵：

| 情况 | segment_order | 源路径/远端 filename | 结论 |
| --- | --- | --- | --- |
| 本地 succeeded 且远端存在对应 order/filename | 一致 | 一致 | 无需处理 |
| 本地缺失（无 succeeded 行）但文件存在且远端无对应分 P | — | 本地文件可 ffprobe 通过 | 可安全补齐（需人工触发，不自动执行） |
| 两条本地行指向同一源路径或同一远端 filename | 可能不同 | 相同 | 判定为重复，按标题相同与否分别处理，标题不构成独立证据 |
| 本地状态为 `conflict`（两个不同远端 Video 声称同一源） | — | 冲突 | 必须人工确认，禁止自动合并 |
| 文件缺失且本地无 succeeded 记录，远端也无对应分 P | — | — | 必须人工确认（可能是录制失败，也可能是历史误删） |

## 五、`04:54:30` 重复项判定

依据采集清单第五节的结果填写：

```text
候选行：
- id=?  normalized_file_path=?  远端 filename=?
- id=?  normalized_file_path=?  远端 filename=?

identity 证据（二选一，缺一不可仅凭标题下结论）：
- 源路径是否一致：
- 远端 Video.filename 是否一致：

结论：
```

## 六、B 站实际分 P 列表

```json
{}
```

（若采集阶段无法访问，标注「待人工执行第六节公开接口命令后补全」）

## 七、逐 order 比对表

| order | 本地 succeeded 记录 | 本地文件是否存在 | 远端是否存在对应分 P | 结论类别 | 依据 |
| --- | --- | --- | --- | --- | --- |
|  |  |  |  |  |  |

结论类别只能是以下三种之一：

- **无需处理**：本地与远端一致，或差异在预期范围内（如 synthetic legacy 基线本就不参与本地恢复）。
- **可安全补齐**：本地文件完整、ffprobe 通过、远端确认缺失该分 P，且不存在冲突记录；
  仍需人工触发补传，本报告不自动执行。
- **必须人工确认**：存在冲突记录、文件缺失、`videos_json` 解析失败、或 identity 证据不足以判断是否重复。

## 八、最终结论与建议动作

```text
（一句话摘要：#227 的核心问题是……）

建议动作（均需人工在确认后手动执行，本报告不触发任何操作）：
1.
2.
```

## 九、回滚策略（对应 07 号任务文档第 6 节）

本节只是策略说明，不涉及代码改动：

- **新代码回滚**：`upload_missing_segment` 新增的 nullable 字段（`normalized_file_path`、
  `lifecycle_version` 等）和新表（`upload_line_health`、`upload_recovery_audit`、
  `upload_lifecycle_backfill` 及其事件表）对旧版本代码不可见即可安全忽略，旧版本按
  `lifecycle_version=1` 语义运行不受影响。
- **不通过 down migration 删除生命周期记录**：`upload_missing_segment`/`upload_session` 的历史
  行是唯一的恢复依据，任何回滚路径都不得删除或清空这些行，包括手工执行 `DELETE`。
- **迁移失败处理**：立即恢复停写前对 `data.sqlite3`/`-wal`/`-shm` 的备份，不在生产库上反复重试
  迁移；确认失败原因后先在生产副本重放。
- **backfill 失败处理**：`biliup backfill-lifecycle` 的断点记录在 `upload_lifecycle_backfill`
  同一事务内，失败后应从断点继续而不是重跑全量；任何情况下都不覆盖原始 `videos_json`——
  backfill 只新增/更新 `upload_missing_segment` 的 v2 字段，不改写会话表的历史字段。
- **审计留痕**：迁移和 backfill 的结构化摘要（受影响会话数、synthetic 基线数、conflict 数）
  需要保留下来供后续审计，不随日志滚动丢失。

## Comments

-
