# 01 — 在共享删除边界发原生分段丢弃事件

Status: resolved

## 目标

让每次实际删除都能通过 R/S/DA 关联回原录制分段，并保持现有过滤与删除行为不变。

## 实现

1. 为 `InvalidMediaReason` 在 `download.rs` 内做稳定 `reason_code` 映射；短有效分段使用
   `below_filtering_threshold`。
2. 给 `observe.rs` 增加 `recording.segment_discarded` emitter，WARN、`outcome=executed`，携带
   身份、basename、`size_bytes` 和 `threshold_bytes`。
3. 给 `Fields` 允许列表增加非负数值字段 `threshold_bytes`。
4. `SegmentEventProcessor` 保存构造时算出的 `filtering_threshold_bytes`；
   `remove_invalid_segment` 接收 `SegmentInfo` 所需快照并改为 async，await 删除成功后立刻发原生
   事件。删除失败保留原文件和既有错误行为。
5. legacy 成功行使用 `original_file` / `reason_code`，不修改桥接白名单。
6. 更新结构化日志契约与覆盖表。

## 最小回归

- 原因映射单测覆盖所有 `InvalidMediaReason` 变体。
- 一条原生采集测试断言事件名、WARN、身份、basename、大小、阈值、原因以及 `rejected=0`。
- 一条删除失败测试断言文件保留且没有成功丢弃事件。
- 运行 `cargo test -p biliup-cli --lib`、`cargo test -p biliup-observability`。

## Comments

- 2026-09-05：分析确认 legacy 字段拒收来自有意的 allowlist，不在桥接层放宽。
- 2026-09-05：拒绝场次计数等式；现有下载器与短分段恢复语义无法满足该不变量。

## Answer

已在共享删除边界完成实现：`SegmentEventProcessor` 保存本次阈值，两个淘汰分支都等待删除；
只有删除成功才发 WARN `recording.segment_discarded`，携带 R/S/DA、basename、大小、阈值和稳定
原因。删除失败保留原路径且不发成功事件。legacy 成功行改用 `original_file` / `reason_code`，
allowlist 只增加数值字段 `threshold_bytes`，没有放宽未知字段。

验证：

- `cargo test -p biliup-cli segment_discard_tests --lib`：3 passed。
- `cargo test -p biliup-cli --lib`：365 passed，8 ignored。
- `cargo test -p biliup-observability`：30 passed。
- `python3 scripts/check_code_index.py` 与 `git diff --check` 通过。

未添加场次汇总、计数等式、配置项或依赖；这些都不属于本 issue 的最小正确修复。
