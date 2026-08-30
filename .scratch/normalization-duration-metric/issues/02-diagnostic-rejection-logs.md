# 02 — 判据失败日志带上实测值

Status: resolved（`OutputRejection` 携带实测值，`reason()` 取值集合不变）
Blocked by: 01

## 背景

[`spec.md` 第 5 节](../spec.md)。现在的 WARN 只有 `reason="duration_drift"`，
判断「差多少、差在哪一侧」必须回到现场 ffprobe 原片；而分段可能已经上传完被清掉。
这次事故里这一条直接决定了排查成本。

## 做法

`output_is_faithful` 的返回类型从 `Result<(), &'static str>` 换成携带证据的结构
（reason 仍是稳定的 `&'static str`，日志字段名不变，便于既有日志检索继续可用），
让每条判据都能把自己用到的数附上：

| reason | 需要附带 |
| --- | --- |
| `duration_drift` | `source_span`、`output_span`、`tolerance`、`source_start_time`、`source_container_duration` |
| `output_too_small` | `input_bytes`、`output_bytes` |
| `video_streams_differ` | 两侧的 codec 列表 |
| `output_has_no_audio` / `output_has_no_duration` / `output_unreadable` | 产物字节数 |

`source_start_time` 与 `source_container_duration` 必须在 `duration_drift` 里出现——
本次事故只要有这两个数就能一眼定位。

## 验收标准

1. 单测断言结构体字段值正确（不断言日志文本）。
2. `reason` 的取值集合与现在完全一致，不新增、不改名。
3. 人工看一眼：把本机复现的 offset FLV 跑一遍，WARN 一行内能读出「差的正好是 start_time」。
