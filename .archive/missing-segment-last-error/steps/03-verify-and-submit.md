# 03 · 验证、回写与提交 PR

Status: complete

Blocked by: 01, 02

## 目标

用本地确定性验证证明字段契约成立，完成 issue → PR 的交付闭环。

## 验证清单

1. 精确检索 `upload_missing_segment.last_error` 全部写入点，逐一确认只有真实失败能写非空值。
2. 运行针对性测试：

   ```bash
   cargo test -p biliup-cli missing_segment
   cargo test -p biliup-cli persist_segment
   ```

3. 运行完整检查：

   ```bash
   cargo test -p biliup-cli
   pnpm lint
   ```

4. 用临时 SQLite 验证 migration 后的状态不变量：
   `status = 'succeeded'` 不存在非空 `last_error`，失败行诊断未丢失。

## 回写与提交

- 把 01、02、03 的 `Status` 和验证结果写回各 step。
- 若本轮理解新增了有导航价值的约束，更新 `CODE_INDEX.md` 并运行
  `python3 scripts/check_code_index.py`。
- 分阶段提交实现与验证，创建以 `dev` 为 base 的 PR，正文关联 issue #7——
  **但不要用 `Closes`/`Fixes`/`Resolves` 关键字**，本仓库生产验完才关，见
  [`docs/agents/issue-tracker.md`](../../../docs/agents/issue-tracker.md)。
- PR/commit/公开文档只写代码契约与脱敏验证结论，不写部署或生产数据细节。

## 完成标准

- 所有检查通过，工作区干净。
- PR 的 head 为 `fix/issue7-260910-103945`、base 为 `dev`。
- issue #7 由 PR 关联；合并**不等于**关闭，要等生产验收通过才关，
  `.scratch/missing-segment-last-error/` 在那之前不归档。

## Comments

- 已审计 `upload_missing_segment.last_error` 的当前写入点：非空写入均为上传初始化、
  segment processor、attempt、源文件缺失或删除失败的真实诊断；健康接口仍只依赖
  `status` 与 stale lease。
- `cargo test -p biliup-cli` 完整复跑通过（386 unit passed、9 ignored，全部 integration/doc
  tests 通过）。首跑一条既有并发 enrollment 用例短暂落入 outbox，单独复跑及第二次
  全量复跑均通过。
- migration 不变量由 `migration_clears_only_succeeded_last_errors` 在临时 SQLite 中验证。
- PR：[#47](https://github.com/dplei/biliup/pull/47)（base `dev`，head
  `fix/issue7-260910-103945`，`Closes #7`）。

- ⚠️ 本轮实际提交时正文写了 `Closes #7`，合并瞬间自动关闭了 issue。已 reopen、补 
  `awaiting-verification` 标签与六条可判定的观察清单；约定同时写进了 `docs/agents/issue-tracker.md`。
