# Passport 错误 URL 脱敏

来源：https://github.com/dplei/biliup/issues/51

## 根因与范围

`Credential::validate_tokens` 将 access_key 和 sign 放入请求 query，发送及 JSON 解码错误直接转换为 `Kind::Reqwest`。登录重试与 API 错误链的 Debug 输出会带出该 URL；CLI 和 Python 入口也共用此方法。

## 方案

在该方法的发送请求和 JSON 解码边界调用 `reqwest::Error::without_url()`。保持错误类型、底层原因、重试和成功返回值，仅移除错误携带的 URL。无需改调用方、配置、导出或依赖。

## 验证与交付

- 用本地无响应代理与虚构凭据验证真实 token 校验失败：仍为超时且保留 source，URL 与 Debug/Display 中的敏感 query 消失。
- 运行 biliup 库测试、服务端编译检查、格式与代码索引检查。
- PR 目标为 dev，使用 `Refs #51`；合并与真实运行验收前不归档。

## 边界

本任务不清理历史日志、不修改凭据、不扩展全局 tracing 脱敏。字面量核对另发现稿件查看接口也构造含 access_key 的 URL，属于独立入口，后续单独核对。

## 进度

实现与本地验证已完成，等待 PR 合并与真实运行验收。

本地结果：`cargo test -p biliup --lib` 88 通过、1 个需真实账号的既有测试忽略；`cargo check -p biliup-cli`、修改文件 rustfmt 检查、代码索引检查均通过。新增隔离测试覆盖发送阶段超时与完整 source 链；JSON 解码错误使用同一标准库映射，尚未通过真实响应注入验证。

真实运行验收：token 校验发生网络失败后，登录重试及 API 错误日志不含 access_key/sign query，仍保留网络原因；正常登录与上传继续成功。仅记录脱敏结论，不公开原始凭据或账号标识。
