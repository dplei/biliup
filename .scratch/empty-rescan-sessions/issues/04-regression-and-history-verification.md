# 04 — 事故回归与历史验证

Status: resolved
Blocked by: 01, 02, 03

## 背景

Issue #3 的现场文件已经被后处理删除，无法直接复用。事故的关键状态全部在 SQLite：孤儿
`streamerinfo`、零 lifecycle 会话、`blocked_missing_segments`、migration 19 回填的投稿意图以及
周期扫描。因此可以用临时数据库完整重放，不需要生产录像。

## 改动范围

1. 增加事故级集成测试，构造：
   - `livestreamers` + 孤儿 `streamerinfo`；
   - 空目录或指向不存在文件的 `filelist`；
   - 调用补扫；
   - 推进假时钟超过恢复窗口并执行投稿协调扫描。
2. 断言修复后补扫阶段根本不创建会话，因此后续 stale/投稿扫描无候选。
3. 为升级前已经存在的空壳构造 fixture：状态为 uploading、零 lifecycle、已有投稿意图或历史
   blocked；执行协调后一次性进入 `discarded_empty`，之后扫描稳定为空。
4. 覆盖人工丢弃后再次补扫的防复活路径。
5. 补充维护说明：已有明确空壳优先使用新接口；若必须直接 SQL，谓词必须与 02 的安全条件一致。
6. 运行后端全量测试、前端检查和代码索引校验；若本轮读懂了未收录的导航关系，同步更新
   `CODE_INDEX.md`。

## 验收矩阵

| 场景 | 预期 |
| --- | --- |
| 孤儿场次 + 空目录 | 不创建会话 |
| 孤儿场次 + 消失的 filelist 文件 | 不创建会话 |
| 孤儿场次 + 无效媒体 | 不创建会话，只累计 invalid |
| 孤儿场次 + 有效媒体 | 创建一个会话并登记 lifecycle |
| 历史关闭态空壳 | 单次终结，不进入 blocked 重试 |
| 活动空会话、无投稿意图 | 保持活动，不误终结 |
| 空壳人工丢弃后再次补扫 | 命中 finalized，不复活 |
| 非空会话人工丢弃 | 409，不改变账本 |

## 验证命令

```bash
cargo test -p biliup-cli
pnpm lint
python3 scripts/check_code_index.py
```

若仓库实际前端检查命令不同，以 `package.json` 中现有脚本为准，不新增只为本 ticket 服务的脚本。

## 完成记录要求

实现完成后在本文件 `## Answer` 下写入：

- 新增回归测试名与覆盖的事故步骤；
- 全量测试结果；
- 是否需要对现存数据库执行人工清理；
- 任何无法用合成夹具覆盖、仍需真实环境验收的项目。

全部 ticket resolved、改动落到 `dev` 且不再需要真实环境观察后，整个目录按
`docs/agents/issue-tracker.md` 移至 `.archive/`。

## Comments

- 2026-08-30：当前没有现场本地分段；这不影响零产出、状态收敛和 UI/API 的实现与回归。真实媒体
  只用于确认正常“有产出”路径，可由测试生成。

## Answer

- 新增事故级集成测试 `target_09_empty_rescan_shell_never_blocks_or_revives`：重放孤儿场次空目录补扫、
  migration-19 风格投稿意图、历史 blocked 空壳启动扫描、单次逻辑终结、后续扫描稳定为空，以及再次
  补扫命中 finalized 边界。
- 相关单元/API 回归：
  `local_rescan_without_valid_candidate_has_no_session_side_effect`、
  `local_rescan_creates_session_with_first_valid_candidate_and_is_idempotent`、
  `closed_zero_baseline_session_is_discarded_without_blocking`、
  `concurrent_empty_discard_is_idempotent`、
  `empty_session_discard_endpoint_is_logical_and_idempotent`、
  `empty_session_discard_endpoint_rejects_nonempty_or_uncertain_sessions`、
  `startup_scan_terminalizes_historical_empty_shell_once`。
- 验证结果：`cargo test -p biliup-cli` 主单测为 309 passed、0 failed、3 ignored，事故集成测试为
  25 passed、0 failed，其余集成测试全绿；`npm run lint` 通过（仅仓库既有登录页 `<img>` warning）；
  `npx tsc --noEmit` 通过；`python3 scripts/check_code_index.py` 通过。
- 已同步 `CODE_INDEX.md` 的空会话终结 API、页面入口与补扫 → enrollment 关系。
- 对满足严格条件的现存空壳不要求直接 SQL：启动/周期协调会自动收敛，也可从页面显式终结。带 claim、
  aid/bvid 或远端不确定状态的行不会自动处理，仍须人工核对远端后决定。
- 合成夹具已覆盖全部本地状态转换；无需真实遗留分段。部署后只需常规确认页面按钮与待投稿列表刷新，
  不作为合并前阻塞项。
