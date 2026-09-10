# 03 · 验证、回写与提交 PR

Status: ready-for-agent

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
- 分阶段提交实现与验证，创建以 `dev` 为 base 的 PR，正文关联并关闭 issue #7。
- PR/commit/公开文档只写代码契约与脱敏验证结论，不写部署或生产数据细节。

## 完成标准

- 所有检查通过，工作区干净。
- PR 的 head 为 `fix/issue7-260910-103945`、base 为 `dev`。
- issue #7 由 PR 关联，合并后再按仓库流程归档 `.scratch/missing-segment-last-error/`。

## Comments

