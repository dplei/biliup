# 01 孤立短片段当场作为独立分 P 入队

文件：`crates/biliup-cli/src/server/common/download.rs` `flush_pending_short_segments`

## 改动

1. `let pending = std::mem::take(...)` 之后，若 `pending.len() == 1`：
   取出事件，读 `file_bytes`，`info!` 一条 `"enqueueing lone recoverable short segment as its own part"`
   （带 file / file_bytes / close_reason），然后 `self.enqueue_validated(&mut event, bytes).await` 并返回。
   `stats` 里加一个计数（如 `lone_short_segments_uploaded`），并入现有 session summary 输出。
2. 剩余路径中 `group.len() == 1` 的 defer 文案改为如实描述，带 pending 总数。
3. `docs/short-segment-recovery.md`：说明 `Deferred` 不会自动重试、只能人工处理；说明孤立短片
   会作为独立分 P 上传。

## 测试

`flush_pending_short_segments` 依赖 `Context`/uploader，不好直接单测。把「pending → 动作」的判定
抽成一个纯函数（例如 `plan_short_segment_flush(pending, max_files) -> Vec<FlushPlan>`，
`FlushPlan::{UploadLone, Merge, Defer{reason}}`），只对它写两个测试：

- 1 个事件 → `UploadLone`
- 2 个不同 `flv_fixture` → 两个 `Defer`，reason 含 `cannot form a merge group`

现有 `deferred_batch_manifest_is_durable_and_lists_all_originals`、
`incompatible_codec_parameters_remain_independent` 不动。

## 验证

- `cargo test -p biliup-cli download`
- dev 环境按 spec 验收第 2 条实跑一次。
