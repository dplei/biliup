# 05 — 本机 DNS 失败单列线路类型

Status: wontfix

## 结论

不新增 `UploadFailureKind::LocalResolution`。

`pre_upload` 访问统一的 `member.bilibili.com` 端点，成功返回前还没有连接具体 UPOS 线路。问题不在于
当前分类没认出某一种 DNS 文案，而在于调用点把任何预上传错误都写进了线路 breaker。

Step 01 直接停止这条错误归因：预上传的 DNS、连接、超时、HTTP 和 TLS 失败都不更新具体线路健康；
真正分块上传失败才保留现有 breaker 逻辑。这样不需要维护依赖操作系统与错误库版本的字符串列表。

错误仍会通过 attempt 的 `last_error` 与结构化 `upload_failed` 事件可见，只是不再伪装成线路证据。

## 重新考虑的条件

若未来需要独立展示宿主机网络/DNS 健康，应增加全局环境事件或健康状态，不能复用某条上传线路的
breaker。

## Answer

- `pre_upload_with_retry` 只把 typed HTTP 客户端的瞬时网络分类交给 retry predicate；本地 IO、解析与
  gate 数据库错误不会因文本碰巧包含 DNS/timeout 字样而重试。
- 两个预上传调用点均已移除 `record_line_kind_failure`，而实际分块上传错误仍保留该调用；预上传错误
  因而不会再伪装成某条 UPOS 线路的健康证据。
- 独立 DNS 枚举既不影响重试决策，也没有正确的线路归属，因此本 step 无代码改动，维持 `wontfix`。

## Comments

- 2026-09-08：用“预上传错误一律不归因到 UPOS 线路”取代脆弱的 DNS 文本分类。
