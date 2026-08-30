# 03 — 样片截取窗口与空产物检测

Status: resolved（截取窗口改用跨度，并补空产物拦截）
Blocked by: 01

## 背景

[`spec.md` 第 3 节](../spec.md)。`create_reference_sample` 用同一个错误口径算 `-ss`，
而 ffmpeg 的输入 `-ss` 还会自动叠加 `ic->start_time`，于是 seek 到文件尾之外。
本机实测：**退出码 0，产出 257 字节空 m4a**，代码只看 `status.success()` 所以放行。

这一处早于 1.3.3，不是本次回归，但根因相同。

## 做法

1. 截取窗口改用 ticket 01 的内容跨度算：`start = ((span - length) / 2).max(0.0)`。
   `-ss` 的语义（相对文件起点，ffmpeg 自行叠加 `start_time`）与跨度口径正好匹配，
   不要自己再加 `start_time`。
2. 截取完成后校验产物：`status.success()` **不够**，还要确认字节数与探得的音频时长都 > 0。
   失败时走既有的 `retry_later` 路径。

## 验收标准

1. 真机（`#[ignore]`）：对 `-output_ts_offset 3600 -flvflags no_duration_filesize` 造出的
   6 秒 FLV 取样，产物字节数与现有零偏移素材同量级（不是几百字节）。
2. 单测：截取返回退出码 0 但产物为空时，`create_reference_sample` 返回 `Err`。
3. 跨度探不到（`None`）时沿用现有的 30 秒兜底，行为不变。
