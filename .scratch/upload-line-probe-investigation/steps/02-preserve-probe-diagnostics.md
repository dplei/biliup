# 02 保留探测失败诊断并更正降级告警

Type: task
Status: ready-for-agent
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

## 待决策

优先选择不破坏既有 API 的诊断传递方式。
探测 TLS 分类与普通 probe 冷却是否分开作为独立取舍，
不要为了补日志同时放大所有探测失败的冷却时间。

## Comments

本轮仅完成前置排查；尚未实现或执行这些验收项。

