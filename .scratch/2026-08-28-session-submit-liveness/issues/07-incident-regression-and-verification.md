# 07 — 事故回归与端到端验证

Status: ready-for-agent
Blocked by: 03, 04, 05, 06

## 背景

现有可靠性测试重点验证“不完整会话绝不能投稿”，没有验证相反的活性性质：“已经要求投稿的会话在
账本后来完整时必须最终投稿”。本任务把两次生产事故固化为可重复的自动化回归，并完成上线前验证。

## 自动化场景

1. **正常竞态**：四段会话，首次协调时最后一段 uploading，断言 blocked 且远端调用为 0；最后一段
   `persist_segment` 后，断言最终 exactly-once 投稿、四个 filename 顺序正确、状态 finalized。
2. **重启竞态**：尾段已 durable enrollment，写入投稿意图后模拟进程退出；尾段由恢复任务成功，新的
   启动扫描完成投稿。
3. **历史 NULL 会话**：全部 succeeded、无投稿意图、`submit_state=NULL`；自动扫描不得提交，调用人工
   recover 后必须提交。
4. **活跃直播反例**：当前所有分段 succeeded 但没有投稿意图；多轮扫描后远端调用仍为 0，后续 enrollment
   仍能写入同一 session。
5. **并发唤醒**：下播、最后分段、人工 recover、周期扫描同时触发，断言只有一个 submit claim、一个远端
   请求和一个 BV。
6. **失败退避**：明确远端失败释放 claim 并设置 next time；到期前不重试，到期后仅重试一次。
7. **不确定结果**：`ok_no_aid`、远端成功后写回失败或预置 claim；自动协调器不重复投稿。
8. **阻塞可见性**：source_missing/unknown 等不能完成的会话在 API 与页面显示明确原因。

## 测试设施

- 优先扩充 `crates/biliup-cli/tests/upload_reliability_incident.rs`，复用既有 incident DB fixture。
- 使用可注入 submit spy/fake，禁止自动化测试触碰真实 B 站。
- 使用 fake clock 控制 `next_submit_at`，避免 sleep。
- 对远端副作用以调用次数和 payload 中的有序 filename 双重断言。

## 验证步骤

1. `SQLX_OFFLINE=true cargo check --workspace`。
2. `cargo test -p biliup-cli`，重点报告上述新增 incident cases。
3. `cargo test --workspace`。
4. `tsc --noEmit`、`next build`。
5. 本机 dev 环境用不触碰真实投稿的 fake/测试模式复演 11 秒竞态并查看页面状态流转。
6. 若进入部署阶段，先备份生产 SQLite；migration 后只读检查历史 blocked 回填范围，不批量改变 NULL 会话。
7. 发布后观察至少一场多分段直播：确认正常投稿、无重复稿件、待投稿扫描无异常高频重试。

## 验收

- 所有自动化场景稳定通过，不依赖真实时间和外网。
- migration 的生产只读检查结果与预期一致。
- 一场真实多分段直播正常产生单一稿件；日志能串起
  `submit_requested -> blocked/ready -> claimed -> submitted`。
- 本任务只在验证完成后标记 resolved，并把提交、测试和部署证据写入 `## Answer`。
