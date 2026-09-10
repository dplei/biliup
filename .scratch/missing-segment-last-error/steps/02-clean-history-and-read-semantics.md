# 02 · 清理历史成功行并统一读取语义

Status: ready-for-agent

Blocked by: 01

## 目标

让升级后的已有数据库立即满足成功行不带错误的约束，并移除读取侧对“正常记录也放在
`last_error`”的旧认知。

## 修改范围

### migration 26

新增下一号 migration：

```sql
UPDATE upload_missing_segment
SET last_error = NULL
WHERE status = 'succeeded' AND last_error IS NOT NULL;
```

只依赖状态做确定性清理。不得按错误文案做 `LIKE`/前缀分类，也不得改动 pending、uploading、
failed、source_missing 或 deleting 行。

### 读取侧

- 更新 `app/ui/logevents/ProgressView.tsx` 中声称正常流程也写 `last_error` 的注释和“最近记录”
  分支，统一按真实错误诊断展示。
- 补传页已有“最后错误”列，不新增字段或兼容分支；migration 与运行时契约保证 succeeded 行为空。
- API/健康接口继续使用 `status`，不添加 `last_error IS NOT NULL` 判断。

## 测试

- 在临时 SQLite 中准备一条带 `last_error` 的 succeeded 行和一条 failed 行，执行 migration：
  succeeded 被清空，failed 原样保留。
- 复用现有 migration/test pool，不新增测试框架。

## 验收

- migration 对已清洁数据库幂等。
- 前端不再把 `last_error` 描述成正常进度说明。
- `pnpm lint` 通过。

## Comments

