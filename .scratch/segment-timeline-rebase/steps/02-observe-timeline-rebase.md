# 02 · 跨基准跳变的结构化事件

issue #35 建议 3 里「输出结构化告警」的那一半。判定逻辑不放这里——step 01 已经让跳变
不再影响切片，本 step 只解决**事后从日志能不能看见它发生过**。

## 现状的缺口

回退侧已经有 `recording.dts_backward`（`httpflv.rs` 的 `DtsBackwardRollup`，按分段汇总），
issue 证据里那三条 `previous_ms=32891256 current_ms=0` 就是它打的。
**前向跨基准跳回没有任何事件**：0 → 32891256 这一步在日志上不存在，只能靠回退侧倒推。

step 01 之后这类跳变会被静默丢弃（不计入 elapsed），如果 MAX_STEP 定得不合适，
症状是「分段比配置的长」，而没有任何线索指向原因。这是本 step 存在的理由。

## 做什么

`set_time_position` 判定为不连续（回退或超 MAX_STEP）时发一条事件：

```
event_name = "recording.timeline_rebased"
outcome    = "executed"
reason_code = "backward" | "forward_jump"
previous_ms, current_ms, elapsed_ms
```

`Segmentable` 手上没有录制身份（`RecordingOwner` 在 `LifecycleFile` 上），两者是并列结构。
**不要为了发这条事件把 owner 塞进 `Segmentable`**——那是给日志改数据结构。可选两条路，
实现时二选一并在此记录理由：

- **A**：`set_time_position` 返回一个 `Option<TimelineRebase>`，由 `httpflv.rs` 在持有
  `out.file` 的地方发事件（身份现成）。签名变了，但调用点只有一处。
- **B**：像 `DtsBackwardRollup` 那样在 `httpflv.rs` 侧自己比对时间戳并汇总，
  `util.rs` 完全不动。重复一点判定逻辑，换零耦合。

倾向 A：判据只有一处定义，不会和 MAX_STEP 漂移。

事件同样要**按分段汇总**，不能一帧一条——CDN 重发是成批的，逐条会淹掉日志。
汇总形状照抄 `DtsBackwardRollup`（首条 1:1，其余计数 + 首末 + 极值）。

## 验收

- 单测：喂一串带跳变的位置，断言事件条数是汇总后的条数而不是跳变次数。
- 事件字段进 `docs/` 的契约清单（照 v1 契约的既有做法补一行）。

## 可后置

step 01 上线就闭环了，这条只是让下一次调 MAX_STEP 时有据可依。
如果生产日志里 step 01 之后再没出现过异常分段时长，这个 step 可以一直不做。
