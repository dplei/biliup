# 01 — 服务端 `pre_upload` 瞬时网络失败有界重试

Status: resolved

## 背景

`upload_single_file` 与页面整场上传各自裸调用 `Line::pre_upload`。一次没有远端结论的网络错误会立即
结束外层 attempt；调用点随后还把统一预上传端点的错误记到所选 UPOS 线路头上。

默认响度标准化产物已由 PR #14 原地替换并留 `audio_normalized_at` 标记，因此本 step 不再处理产物
缓存，只缩短瞬时故障的恢复路径。

## 改动范围

1. 在 `crates/biliup-cli/src/server/common/upload.rs` 增加一个私有 helper，供以下两处共用：
   - `upload_single_file`；
   - 页面整场上传的逐文件循环。
2. helper 复用 `biliup::retry_with_config`，固定最多重试 3 次；不增加配置。
3. 每轮都重新 `VideoFile::new(path)`，并在真正请求前执行一次
   `upload_rate_gate::before_pre_upload`。
4. 重试 predicate 只接受 `ConnectTimeout`、`RequestTimeout`、`Transport`；601、HTTP 和证书分类立即
   返回。
5. 每次返回后立即收口 gate：成功调用 `record_success`，601 调用 `record_rate_limited`，其他失败调用
   `record_non_rate_limit_failure`。后者必须发生在决定重试之前，避免冷却 probe 卡死在 `Probing`。
6. 两处 `pre_upload` 最终失败都不再调用 `record_line_kind_failure`；真正分块传输失败的现有记录保持
   不变。
7. gate 自身的数据库错误直接返回；只有 `Line::pre_upload` 返回的原始 `Kind` 进入网络重试 predicate，
   不得先丢失类型信息再靠格式化文本猜回去。

helper 可以返回 uploader 及调用方后续日志所需的文件大小/文件名；不要为一个 helper 新建模块、trait
或配置结构。

## 边界

- `Line::pre_upload` 本身不加重试：它不知道服务端 gate，无法保证每个真实请求都计数。
- 独立 CLI 不改：它不走自动响度标准化链，并有独立的 601 进程锁语义。
- 不区分 DNS 与其他 transport 文本；它们都可重试，也都不应归因到实际 UPOS 线路。
- 不重试 HTTP 状态。当前分类丢失了具体状态码，本轮不扩错误模型。

## 最小验证

用可控的私有 helper 边界留下一个小回归，不引入 mock 框架：

1. 两次 `Transport` 后成功：请求 3 次、gate 准入 3 次，外层只得到一次成功；
2. 601：请求 1 次并进入既有冷却；
3. HTTP/证书错误：请求 1 次；
4. `Transport` 耗尽：返回最终错误，gate 可再次准入；
5. 上述所有预上传失败都不产生 `upload_line_health` 失败行；实际分块上传失败的既有测试继续通过。

运行：

```bash
cargo test -p biliup-cli
```

## Answer

- 在 `upload.rs` 增加私有 `pre_upload_with_retry`，两个服务端入口共用；固定最多重试 3 次，每轮重新
  打开文件并重新经过 rate gate。
- predicate 只接受 typed `ConnectTimeout`、`RequestTimeout`、`Transport`；601、HTTP、证书、文件 IO
  与 gate 数据库错误不重试。每次请求先收口 gate，再决定是否继续。
- 删除两处预上传错误对 `upload_line_health` 的写入；实际分块上传的 breaker 记录保持不变。
- 新增策略回归 `pre_upload_retry_policy_excludes_local_and_final_errors`；`cargo test -p biliup-cli` 全部
  通过。请求次数、probe 释放与 breaker 不变性的故障注入留在 Step 06 集中验证。

## Comments

- 2026-09-08：原设计只覆盖录制/补传入口，漏了页面整场上传；本次改为两个服务端调用点共用同一
  helper，并修正 `Probing` 状态必须逐次释放的死等风险。
