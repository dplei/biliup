# 03 给 fallback 必经修复加回归测试

最便宜的一步，纯防回退，不改行为。

## 现状

issue #13 的建议 2（`TranscodeFailed` fallback 跳过时间戳修复）在当前代码里**已不成立**：
`upload.rs` 的门是 `repair_enabled && !source_timestamps_clean`，而 `source_timestamps_clean`
只有 `NormalizationOutcome::Normalized { source_timestamps_clean: true, .. }` 才为真；
`TranscodeFailed` 走 `Original { reason }`，clean 恒 false，必进 `normalize_timestamps`。

但这是 `8b78a14` / `80494a5` 顺带的结果，**没有任何测试锁住它**。`source_timestamps_clean`
的取值来自响度测量那一遍，将来只要有人给 `Original` 分支补一个"乐观默认"，这条路径就会
悄悄退回 issue 描述的状态——而它恰好是最需要修复的那批文件走的路径。

## 做什么

在 `upload.rs` 的测试模块里加一条：`NormalizationOutcome::Original { reason: TranscodeFailed }`
进入上传前处理时，`timestamp_repair` 的决定必须是 `executed` / `scan_needed`，不能是
`skipped`。同一断言对 `Original` 的其它 reason 也应成立——**归一化没产出，就没有"源已验干净"
这个结论可用**，这才是这条规则的本质，不是针对某一个 reason 打补丁。

现有 `timestamp_repair_fallbacks_never_claim_no_anomaly` 是同一模块里的近邻，照它的形态写。

## 顺带

`processing_decided` 的 reason 目前在跳过时只有 `source_clean` / `disabled` 两种。执行侧只有
`scan_needed` 一种，看不出是「归一化没给结论」还是「归一化说脏」。真要区分再说，**别为这个
单开一轮**——本步只锁行为，不扩字段。

---

## 落地记录（已完成）

行为一字未改，只是把判据从内联表达式抽成可断言的纯函数并加测试。

### 改动

`upload.rs` 里原来是两处内联：

```rust
let source_timestamps_clean = matches!(normalization, Normalized { source_timestamps_clean: true, .. });
...
let outcome = if repair_enabled && !source_timestamps_clean { /* executed / scan_needed */ }
              else { /* skipped, reason 现算 */ };
```

抽成 `timestamp_repair_decision(repair_enabled, &normalization) -> (outcome, reason_code)`，
调用点只负责用返回值发事件。真值表逐项对照过，与改前一致：

| 输入 | 结果 |
|---|---|
| `repair_enabled == false` | `("skipped", "disabled")` |
| `Normalized { source_timestamps_clean: true }` | `("skipped", "source_clean")` |
| `Normalized { source_timestamps_clean: false }` | `("executed", "scan_needed")` |
| `Original { reason: 任意 }` | `("executed", "scan_needed")` |

判据仍在 `normalization` 被 `match` 消费之前取值，取值时机没变。

### 测试

- `every_normalization_fallback_still_scans_timestamps`：遍历 `OriginalReason` 的**全部**
  变体断言 `("executed", "scan_needed")`。配套的 `every_original_reason()` 里放了一个穷举
  `match`——**新增 reason 时这里会编译不过**，逼着人把它补进清单，而不是让新 reason 悄悄
  落到「跳过扫描」那一侧。这比只钉 `TranscodeFailed` 更贴近规则本身：归一化没产出，
  就没有「源已验干净」这个结论可用。
- `only_a_verified_clean_source_skips_the_scan`：`Normalized` 但诊断说脏（或拿不到诊断）
  照样要扫，只有明确验干净才允许跳过。
- `a_disabled_repair_says_so`：关掉开关时理由必须是 `disabled`，不能借用 `source_clean`——
  那会让「没扫」在事件里看起来像「扫过且干净」。

`cargo test -p biliup-cli --lib` 381 passed（新增 3 条）。

### 按本文要求没做的

`processing_decided` 的 reason 没扩字段：执行侧仍只有 `scan_needed` 一种，看不出是「归一化
没给结论」还是「归一化说脏」。本步只锁行为。
