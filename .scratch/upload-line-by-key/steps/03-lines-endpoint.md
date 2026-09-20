# 03 · `/v1/upload-lines` 与前端

- 后端：GET `preupload?r=probe`，取每条 `query` 的 `upcdn` 值，`AUTO` 置顶；内存缓存 5 min；索引失败返回 `IMPLICIT_FALLBACKS` 并 `degraded: true`。
- `app/ui/plugins/global.tsx:443`、`app/(app)/uploads/page.tsx:205`：选项改为请求该端点；描述文字里删掉 `qn`、`bda`。
- 集成测试见 issue 清单。
