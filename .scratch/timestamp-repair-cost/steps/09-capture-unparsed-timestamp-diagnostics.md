# 09 · 为无法解析的时间戳异常保存诊断附件

Status: ready-for-agent
Blocked by: —
优先级：P0——没有这条证据链，解析器再次误判后仍无法定位真实措辞

## 为什么

生产已经走到 `Detection::Anomalous { max_backward_ms: None }`，说明异常模式命中，但每条命中
行都没被 `parse_backward_ms` 解出数值。现有 `run_scanning_stderr` 只在 spawn/read/wait 失败
或非零退出时把 `DiagnosticCapture` 交给事件库；扫描正常退出时，附件被丢弃。

录制侧 DTS 告警来自另一条路径，不能代替 `detect()` 实际消费的 stderr。原片成功上传后会被
清理，取回凭证也不保证一直有效，因此不能继续依赖事后复现。

## 做法

- 复用 `DiagnosticCapture` 的现有限额与脱敏，只收集“命中异常模式、但
  `parse_backward_ms` 返回 `None`”的行；不要把完整 verbose stderr 塞进事件字段。
- 扫描进程即使 exit 0，也要为这类未知异常发一个带上传 `Context` 的原生诊断事件并落
  `log_diagnostic`。事件语义必须表示“业务异常无法解释”，不能伪装成
  `processing.command_failed`。
- 保留现有非零退出诊断、64 KiB 业务 tail、可解析异常与 clean 路径的行为，不新增数据库表或
  migration。
- `timestamp_repair.rs` 仍然返回 `max_backward_ms: None` 并走保守 `Unfixable`；本步只补证据，
  不猜解析规则。

## 验收

- 一个 exit 0 的假进程输出未知格式的异常行：恰好产生一个原生事件，stage、分段/attempt 身份
  正确，诊断附件非空且敏感值已脱敏。
- 可解析的异常行与 clean 输出不产生这条额外诊断；非零退出仍只走既有命令失败事件。
- `cargo test -p biliup-cli --lib ffmpeg_scan` 通过，再跑 `cargo test -p biliup-cli`。
- 实现后同步更新 `CODE_INDEX.md` 对 `ffmpeg_scan.rs` / 原生诊断事件的职责说明，并运行
  `python3 scripts/check_code_index.py`。

## 明确不做

- 不添加 `Non-monotonous` 等猜测性字符串。
- 不改 10 秒安全闸门，也不把 `None` 当成 0。
- 不为了附件另建一套日志存储。

## Comments

真实措辞到手后转交 11；09 本身不等待生产样本。
