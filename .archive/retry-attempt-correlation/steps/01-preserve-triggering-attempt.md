# 01 · 保留触发退避的下载 attempt

Status: resolved

来源：[spec](../spec.md)、[dplei/biliup#28](https://github.com/dplei/biliup/issues/28)

## 目标

服务端每次实际下载结束后发出的 `recording.retry_scheduled`，携带刚结束且触发该次退避的
`download_attempt_id`；连续失败时不串到下一轮 attempt。

## 改动

- `crates/biliup-cli/src/server/common/download.rs`
  - 在进入本轮 `self.download(...)` 前复制当前 `stream.attempt_id`，并与“本轮确实执行过下载”绑定。
  - `check_stream` 刷新并覆盖 `stream` 后，发 retry 时仍传这份旧快照。
  - `can_download == false` 的纯检查/冷却轮次不制造新 ID，也不把下一轮候选 ID 当成已执行 attempt。
- `.scratch/structured-logging/contract-v1.md`
  - 明确 C05：disconnected/retry 共享触发失败的 attempt；reconnected 使用新连接 attempt。
- `.scratch/structured-logging/coverage-ledger.md`
  - 记录回归入口与关联断言，移除“只能按时间推测 retry 所属 attempt”的缺口。

保持 `observe::retry_scheduled`、`DownloadTask::download` 和 downloader trait 签名不变；不要新增字段、
迁移、配置或 attempt 状态机。

## 测试

在 `crates/biliup-cli/src/server/common/download.rs` 的相邻测试模块补一个最小、确定性事件捕获用例：

1. 准备顺序为 `attempt-a`、`attempt-b` 的两轮下载身份，并让每轮进入非零退避。
2. 在直播状态刷新为下一轮身份后再采集 `recording.retry_scheduled`，复刻实际覆盖顺序。
3. 断言两条 retry 的 DA 依次为 `attempt-a`、`attempt-b`；第一条不得提前变成 `attempt-b`。
4. 同时断言事件的 `outcome=waiting`、`reason_code=transport_error`，避免只测辅助变量而没测最终事件。

优先复用现有 tracing 捕获写法；若完整 `DownloadTask::execute` fixture 需要搭建数据库、Monitor 和真实
等待，则只抽取保存/发射这条局部路径供测试复用，不为一行传参引入通用测试框架。

## 验收命令

```bash
cargo test -p biliup-cli retry_scheduled --lib
cargo test -p biliup-cli --lib
cargo fmt --all -- --check
python3 scripts/check_code_index.py
```

## 回执

- `DownloadTask::execute` 在每轮实际下载前保存当前 `stream.attempt_id`；直播复查刷新候选后，
  `recording.retry_scheduled` 仍使用这份触发退避的旧快照。未执行下载的冷却轮次保持未知。
- 原有退避条件和原因映射收进局部 `emit_retry_after_attempt`，事件捕获回归连续发出
  `attempt-a` / `attempt-b`，逐条断言 DA、`waiting`、`transport_error` 与零 rejected 字段。
- C05 契约、覆盖清单和 `CODE_INDEX.md` 已同步。未新增字段、迁移、配置或跨平台 ID 分配。
- `cargo test -p biliup-cli retries_keep_the_attempt_that_triggered_each_backoff --lib`：1 passed。
- `cargo test -p biliup-cli --lib`：382 passed，9 ignored，0 failed。
- `rustfmt --edition 2024 --check crates/biliup-cli/src/server/common/download.rs`、
  `git diff --check`、`python3 scripts/check_code_index.py`：通过。
- `cargo fmt --all -- --check` 仍会报告本分支开工前已有的其他文件格式差异；本轮没有顺手改动。

## Comments

未搭建完整数据库、Monitor 和真实等待 fixture；按 spec 的后备方案复用了现有内存事件采集器，
只抽出退避发射边界。attempt 快照本身保留在实际循环中，与下载是否执行使用同一个 `can_download`
判据。
