# 移除 token 校验错误 URL

Status: ready-for-human
Type: task

## 范围

修改 `Credential::validate_tokens` 两个 reqwest 错误边界；添加本地隔离回归并同步上传领域代码索引。

## 验收

- 虚构凭据不出现在错误 Debug、Display 或 source 链中。
- 超时错误类型与底层原因保留。
- 成功路径、登录重试次数与 API 行为不变。

## Comments

实现与本地验证完成。两个 reqwest 边界均在转换到 `Kind` 前移除 URL，各调用方无需修改。

`cargo test -p biliup --lib`：88 通过，1 个需真实账号的既有测试忽略；`cargo check -p biliup-cli`、修改文件 rustfmt 与代码索引检查通过。已有 unused_mut/dead_code 警告未纳入本任务。

待 PR 合并与真实运行验收；不清理历史日志、不修改凭据。JSON 解码错误映射已实现，但没有真实响应注入测试。
