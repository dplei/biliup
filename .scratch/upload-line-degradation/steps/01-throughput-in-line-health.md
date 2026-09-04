# 01 · 吞吐入库，慢即短冷却

补 issue #17 的缺口 2 和 3：让「传完了但很慢」沉淀成信号，并在下一次选路时自动规避。

## 改什么

### migration 25

```sql
alter table upload_line_health add column avg_mbps REAL;
```

一列。EWMA 就地更新，不留样本历史——要看趋势时日志里每次上传都打了 `MB/s`。

### `upload_line_health.rs`

- `record_success(pool, line_key, mbps: Option<f64>)`
  - `mbps` 为 `None`（探测成功等非传输路径）时行为与今天完全一致：清零、清冷却。
  - 有值时先算基线 `SELECT MAX(avg_mbps) FROM upload_line_health`：
    - 基线为 `NULL` → 只更新 EWMA，不判慢（冷启动）。
    - `mbps < baseline / SLOW_RATIO`（`SLOW_RATIO = 4.0`）→ 判慢。
  - EWMA：`new = old.map_or(mbps, |old| old * 0.7 + mbps * 0.3)`。判慢用的是**本次实测值**不是
    EWMA，否则一次劣化会被历史稀释掉。
  - 判慢时写 `cooldown_until = now + SLOW_COOLDOWN`（`Duration::minutes(30)`）、
    `last_failure_kind = "slow_throughput"`、`last_error` 写成
    `"throughput 2.29 MB/s < baseline 25.78/4 MB/s"` 这种可读句子，**`consecutive_failures` 保持 0**。
    慢不是失败，不该污染 `ordinary_cooldown` 的失败梯度。
  - 安全阀：判慢后、写库前查一次 `active_cooldowns`，若加上本条会让 `RECOVERABLE_LINES` 三条
    全部处于冷却，则放弃冷却，只 `warn!` + 照常更新 EWMA。理由见 spec。
- `UploadFailureKind` **不加变体**。`slow_throughput` 只作为 `last_failure_kind` 的字符串值存在：
  它不参与错误分类，加进枚举会让 `classify_*` 的穷尽匹配凭空多一个永不命中的分支。

### `upload.rs`

调用点 `upload.rs:2041` 正好在吞吐计算旁边——`instant` 在 2003 定义（`pre_upload` 之后、
`TransferStarted` 之前），`total_size` 在手上，`t` 在 2046 已经算过。把同一个值提到 `record_success`
之前算好传进去即可，不新增计时。

**计时口径正确性**：`instant` 起点在全局 permit 获取（1830）之后、预处理之后，所以它度量的是纯
网络传输。这正是 `upload.rs:2266` 那条注释的教训——只有网络阶段的慢才能归咎线路。

其余 `record_success` 调用点：全仓只有这一处（`grep` 确认，其余同名函数属 `cookie_health` /
`upload_rate_gate`，无关）。

## 测试

`upload_line_health.rs` 的 `mod tests` 已有 `migrated_pool()`，跟着写：

- 冷启动：空库 + 一次极低吞吐 → 只写 EWMA，`cooldown_until` 为 `NULL`。
- 判慢：先灌一条 26 MB/s 的健康线路建立基线，再让另一条报 2.3 MB/s → 冷却 30 分钟、
  `last_failure_kind = "slow_throughput"`、`consecutive_failures` 仍为 0。
- 不误伤：8.55 MB/s vs 基线 26 → 26/4 = 6.5，**不判慢**。这是故意的：issue 里 8.55 那次确实慢，
  但一刀切到中位数的 1/3 会在正常抖动上误伤。先按 1/4 上线，靠 step 03 校准。
- 安全阀：三条 recoverable 已冷却两条，第三条报慢 → 不写冷却，EWMA 照常更新。
- `mbps = None` → 行为与改动前逐字段一致（防止回归清零语义）。

## 验收

- `cargo test -p biliup-cli upload_line_health` 通过。
- 一次慢上传之后 `active_cooldowns` 里出现该线路，`plan_upload_line` 的 `skipped` 能渲染出
  `slow_throughput`（页面「下一条线路」列复用同一份数据，无需另改）。

## 落地（已完成）

`migrations/25_add_upload_line_throughput.sql` + `upload_line_health.rs` + 两处调用点。
`cargo test -p biliup-cli` 全绿，新增 5 个测试按上面的清单一一对应。

两处与原计划的偏差：

- **签名是 `mbps: f64` 不是 `Option<f64>`**：全仓只有那两个调用点，都在传输后、都有实测值，
  没有第三方会传 `None`。「没测到」的语义由 `mbps` 非有限或非正数表达（`t = 0` 会算出 `inf`，
  这条路径必须留），行为与旧版逐字段一致，回归测试照写。
- **多了 `now: DateTime<Utc>` 参数**：与 `record_failure` 对齐，冷却断言才能是确定值。

安全阀落在 `strands_recoverable_lines`，只在 `line_key` 属于 `RECOVERABLE_LINES` 时生效；
为此把 `RECOVERABLE_LINES` 从私有改成 `pub(crate)`。
