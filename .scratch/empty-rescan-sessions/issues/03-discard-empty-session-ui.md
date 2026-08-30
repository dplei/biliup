# 03 — 空会话丢弃 API 与 UI

Status: resolved
Blocked by: 02

## 背景

待投稿会话卡片目前只有“恢复会话”。零基线会话恢复后仍是 0/0，操作员无法从页面安全终结；直接
删除数据库行又会留下孤儿 `streamerinfo`，使下一次补扫重新创建空会话。

## 改动范围

1. 新增会话级丢弃路由，例如 `DELETE /v1/uploads/sessions/{id}`：
   - 语义是逻辑终结，不执行 SQL `DELETE`；
   - 复用 02 的事务原语；
   - 已 finalized 时幂等返回当前状态；
   - 非空、已有远端标识或 claim 时返回 `409` 和可执行的中文原因；
   - 不存在时返回 `404`。
2. 返回结构至少包含 session id、终结前后状态、`submit_state` 和 `discarded=true`。
3. 待投稿页面只在以下条件显示“丢弃空会话”：
   - `total_expected == 0`；
   - 没有 aid/bvid；
   - 没有 submit claim；
   - 非 `manual_inspection`。
4. 点击后弹出二次确认，明确“保留历史身份、不会删除录像文件、终结后不再自动投稿”。
5. 成功后刷新待投稿与缺失分段数据；失败使用现有 `sendRequest` 错误边界展示后端原因。
6. 对零基线会话隐藏或弱化“恢复会话”，避免继续制造无意义 blocked 尝试。

## 不变量

- API 不删除 `upload_session`、`streamerinfo` 或文件。
- API 不接受任意会话删除；严格限定为空会话。
- UI 条件只是便利提示，权限与一致性最终由后端事务检查保证。
- `manual_inspection` 与持有 claim 的远端不确定状态绝不能出现丢弃按钮。

## 验收与测试

1. 合法空会话调用接口后变为 `finalized/discarded_empty`，从待投稿 API 消失。
2. 对同一会话重复调用得到幂等成功。
3. 非空、远端标识、claim、manual-inspection 四类均被后端拒绝。
4. 页面只为合法候选显示按钮；确认取消不发送请求。
5. 丢弃后对同一 `streamer_info_id` 补扫，命中 finalized 身份且不创建替代会话。
6. 前端 lint/typecheck 与后端路由测试通过。

## Comments

- 若最终选择 `POST .../discard-empty` 而不是 `DELETE`，验收语义不变；关键是逻辑终结而非硬删除。

## Answer

- 新增 `DELETE /v1/uploads/sessions/{id}`；它调用统一事务原语，只逻辑终结，不删除会话、场次或文件。
- 不存在返回 404；非空、远端标识、claim、远端不确定状态返回 409；已 finalized 幂等返回当前终态。
- 待投稿页仅为严格零基线候选显示“丢弃空会话”与二次确认；零基线不再显示无意义的普通恢复按钮。
- 新增 API 合法/幂等与非空/不确定状态拒绝测试，成功后待投稿列表不再返回该会话。
