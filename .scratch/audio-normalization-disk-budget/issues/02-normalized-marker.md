# 02 — `audio_normalized_at` 标记与补传跳过

Status: resolved（随 [#14](https://github.com/dplei/biliup/pull/14) 合入 `dev`；真实环境验收集中在 [`06`](./06-concurrency-acceptance.md)）
Blocked by: 01

## 背景

见 [`spec.md` 4.1](../spec.md)。[`01`](./01-replace-original-in-place.md) 之后，
`file_path` 指向的已经是标准化产物。补传若不知情会重新 measure + transcode：增益几乎为零
（`input_i ≈ -16`、`offset ≈ 0`），但 AAC→AAC 的有损重编码照做，还要多一整遍全片 IO。
补传本身会重试多次，损失会叠加。

> ⚠️ **命名陷阱**：`upload_missing_segment` 已有列 `normalized_file_path`，那是**路径规范化**
> 的结果（`normalize_segment_path`，用于唯一索引），与响度无关。见
> [`upload_session.rs:214`](../../../crates/biliup-cli/src/server/common/upload_session.rs)、
> [`recovery_eligibility.rs:82`](../../../crates/biliup-cli/src/server/common/recovery_eligibility.rs)。
> **不要复用、不要改写这一列。**

## 改动范围

### 1. Migration `21_add_audio_normalized_marker.sql`

```sql
alter table upload_missing_segment add column audio_normalized_at DATETIME;
```

可空，无默认值。`NULL` = 未标准化或不确定，语义上等价于「按原片处理」。不建索引：只按主键
读写，没有范围查询。

v2 下每个通过校验的分段在 enrollment 时就建行（见
[`segment_enrollment.rs`](../../../crates/biliup-cli/src/server/common/segment_enrollment.rs)
的 `enroll_in_database`，`lifecycle_version = 2`），所以标记有落点。
`lifecycle_version = 1` 的历史行一律留 `NULL`，行为同现在，不回填。

### 2. 写入时机：rename 成功之后

顺序是**先 rename、后落标记**，理由见 [`spec.md` 4.2](../spec.md)：两步之间崩溃时，
「多一次有损编码」比「静默漏掉标准化」更可接受，也更容易从日志发现。

落库失败不回滚 rename、不失败整个上传：`warn!` 一条后继续。这与整条链路「标准化是可选增强，
不阻断主链路」的原则一致。

写入点在上传编排层而非 `audio_normalization.rs`——后者不持有 pool，也不该知道账本存在。
`normalize_for_upload` 只在 `NormalizationOutcome::Normalized` 上多带一个
「已就地替换」的布尔，由 `upload_single_file_with_repair` 的调用方落库。

### 3. 读取时机：补传前

三条补传路径都要先读该列，非 `NULL` 就跳过 `normalize_for_upload`（等价于当前
`normalization_enabled = false` 的分支）：

- 自动补传：[`upload.rs:2554`](../../../crates/biliup-cli/src/server/common/upload.rs) 一带
- 人工恢复：[`upload.rs:3486`](../../../crates/biliup-cli/src/server/common/upload.rs) 一带
- 到期扫描：[`recovery_scheduler.rs`](../../../crates/biliup-cli/src/server/common/recovery_scheduler.rs)
  最终也汇到上面两条，确认无第三处独立调用即可

跳过时记一条 `audio_normalization = "skipped"`，`reason = "already_normalized"`，
让日志能区分「跳过因为已标准化」与「跳过因为没开」。

## 验收标准

1. Migration 单测：新列存在、可空、既有行读出 `NULL`。
2. 单测：标准化并替换成功后，对应行的 `audio_normalized_at` 非空。
3. 单测：`audio_normalized_at` 非空的分段走补传路径时，`AudioFfmpegRunner` 的 `measure`
   与 `transcode` **一次都没有被调用**（用现有的 fake runner 断言调用计数）。
4. 单测：`audio_normalized_at` 为 `NULL` 且开关打开时，补传照常标准化（回归）。
5. 单测：标记落库失败不影响上传成功返回。
6. `cargo test -p biliup-cli` 全绿。

## 风险

rename 与落标记之间崩溃 → 该段补传时多一次有损编码。窗口是一次 `rename` 加一条 UPDATE，
代价有界且不是数据丢失，接受，不引入两阶段提交。

## 实现记录（2026-08-30）

标记走既有的 activity 通道（新增 `UploadActivity::NormalizedInPlace`）在替换完成的那一刻
发出，由 `upload_enrolled_with_watchdog` 落库——比等 `upload_single_file_with_repair` 返回
早了整个上传时长。legacy v1 路径不传 activity 通道，天然不落标记，与「v1 一律留 NULL」
一致，不需要额外分支。

`mark_audio_normalized` 只更新 `audio_normalized_at` 一列：`updated_at` 参与到期扫描排序，
不该被一次预处理事件推着走。

验收 3 用 `audio_normalization_needed` 的纯函数单测覆盖——它就是「读取跳过」的全部逻辑，
比拉起一次真实上传更直接。
