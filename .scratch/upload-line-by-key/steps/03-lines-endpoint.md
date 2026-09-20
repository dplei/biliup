# 03 · `/v1/upload-lines` 与前端

- 后端：GET `preupload?r=probe`，取每条 `query` 的 `upcdn` 值，`AUTO` 置顶；内存缓存 5 min；索引失败返回 `IMPLICIT_FALLBACKS` 并 `degraded: true`。
- `app/ui/plugins/global.tsx:443`、`app/(app)/uploads/page.tsx:205`：选项改为请求该端点；描述文字里删掉 `qn`、`bda`。
- 集成测试见 issue 清单。

## 完成（2026-09-20）

- `Probe::index_keys`（`line.rs`）：抽出 `fetch_index`，AUTO 探测与索引端点共用一次 GET；单测用当日真实响应形状。
- `GET /v1/upload-lines` → `{lines, degraded}`。响应**不含 auto**（由页面各自置顶，两页的 auto 语义不同：
  全局是 `AUTO`，上传页是 `''`=跟随配置 / `auto`）；索引失败或为空 → `IMPLICIT_FALLBACKS` + `degraded: true`。
  单测 `upload_lines_fall_back_to_implicit_candidates_and_say_so`。
- 没做 5 min 缓存（`ponytail:` 注释）：SWR 挂载才请求且 `revalidateOnFocus: false`，一次索引 GET 很便宜。
- `app/lib/use-upload-lines.ts`：共用 hook，客户端按字母排序（B 站每次返回顺序不同），已保存却不在索引里的
  key 保留为选项；`lineLabel` 给已知 key 附说明。
- 全局配置页去掉 `qn`，上传页去掉 `bda`；本地 dev 实际打开两页核对：下拉为 `AUTO/akbd/bda2/bldsa/estx/tx`。
  中途试过 `filter + allowCreate` 让用户手输 key，Semi 会按当前值过滤把其他选项藏起来，已撤掉。
- 验收清单里「`AUTO` 在首位」按上面调整为「页面置顶」，issue 已同步改写。
