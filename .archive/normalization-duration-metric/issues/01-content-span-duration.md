# 01 — 时长口径改为内容跨度

Status: resolved（`content_span` + `parse_probe_output`，单测覆盖三项边界）
Blocked by: —

## 背景

[`spec.md` 第 2、4 节](../spec.md)。`format.duration` 在直录 FLV 上是末尾时间戳，
必须减掉 `format.start_time` 才是时长。

## 做法

`crates/biliup-cli/src/server/common/audio_normalization.rs`：

1. `ProbeFormat` 增加 `start_time: Option<String>`（`-show_format` 已经返回，命令行不用改）。
2. `AudioProbe` 三个字段各司其职：
   - `duration_seconds`：**内容跨度**，判据只看它；
   - `container_duration` / `start_seconds`：原值，只供日志（ticket 02 要用）。
3. 跨度按 [`spec.md` 4.2](../spec.md) 的边界表计算：`start_time` 缺失/非有限/为负按 0，
   `span ≤ 0` 时 `duration_seconds = None`（判据自然跳过，不当失败）。
4. 计算集中在一个函数里，原片与产物共用，不要在调用点各算各的。

## 验收标准

1. 单测：`format.duration = 3605.9` + `start_time = 3599.977` 的 JSON 解析出
   `duration_seconds ≈ 5.923`、`start_seconds ≈ 3599.977`。
2. 单测：`start_time` 为 `"N/A"` / 缺失 / 负数三种输入都退化成「按 0」，不 panic。
3. 单测：`duration < start_time`（跨度为负）时 `duration_seconds` 为 `None`，
   `output_is_faithful` 跳过时长判据而不是返回 `duration_drift`。
4. 判据侧无需改动即可通过——口径统一后两侧自然对齐。
