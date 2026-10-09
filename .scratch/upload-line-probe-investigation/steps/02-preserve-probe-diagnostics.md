# 02 保留探测失败诊断并更正降级告警

Type: task
Status: resolved
Blocked by: —

## 目标

当全部候选失败或首轮白名单探测失败时，也能追查具体错误；
选线前告警仅说明将尝试放宽范围，不预测上传和取回结局。

## 范围

- `crates/biliup/src/uploader/line.rs`：保留失败结果及候选为空的区别；
  复用已有脱敏边界，避免把底层自由错误不加限制写进结构化字段。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs`：
  传递两轮失败结果；降级探测前只描述行为，选出线路后再描述是否属已验证取回白名单。
- `crates/biliup-cli/src/server/common/upload.rs`：成功/失败决策都按已选方案持久处理诊断。
- 检查独立 CLI、Python 上传等 Probe API 调用方，保持原有结果与错误契约。
- 更新测试与 `CODE_INDEX.md`。

## 最小验收

- 首轮失败、次轮成功时仍能定位首轮失败的线路和原因。
- 全部失败、没有可探测候选时可区分，不能仅留顶层 reqwest 文本。
- 告警不能把「开始降级探测」写成「已经上传且取回失败」。
- 领取前选线失败不删文件、不增加 attempt、不遗留 uploading claim。
- TLS 校验保留；未重新验证的线路不加入取回白名单。

## 实现回执

已完成核心库 typed 失败、两轮诊断合并、成功/失败健康记录、准确告警及 TLS 分类。
Probe 公共函数签名保留，独立 CLI 和 Python 上传调用不需改参。

健康记录每条线路只计一次，证书错误优先于普通超时；本次已成功选中的线路不再因首轮失败冷却。
普通 Probe 冷却仍可被显式线路绕过，TLS 冷却不可绕过。

验证：

- `cargo test -p biliup --lib`：86 passed，1 ignored。
- `cargo test -p biliup-cli --lib`：429 passed，12 ignored。
- 新增请求 URL 脱敏与底层原因链回归另行运行：1 passed。
- `SQLX_OFFLINE=true cargo check --workspace`：通过，覆盖 Rust CLI 与 Python 绑定。
- `python3 scripts/check_code_index.py`、`git diff --check` 与修改文件的 rustfmt 检查：通过。

## Comments

本地验证使用脚本化探测结果、临时 SQLite 与本地连接，不需账号或真实上传。
[PR #98](https://github.com/dplei/biliup/pull/98) 已合入 `dev`，实现步骤完成。
真实运行验收见 spec，本目录暂不归档，issue 保持开放。
