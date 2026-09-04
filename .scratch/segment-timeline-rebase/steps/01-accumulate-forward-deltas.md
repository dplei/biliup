# 01 · 计时改为累加前向增量

改 `crates/biliup/src/downloader/util.rs` 一个文件，`Segmentable` 的公开方法签名全部不变。

## 改什么

`struct Time` 的字段从 `{ expected, start, current }` 换成 `{ expected, elapsed, last }`：

| 方法 | 新语义 |
|---|---|
| `elapsed_time()` | 直接返回 `self.time.elapsed`（不再做减法） |
| `set_time_position(n)` | 见 spec：`last` 存在且 `delta = n - last` 落在 `(0, MAX_STEP]` 才累加；总是更新 `last` |
| `set_start_time(n)` | `elapsed = ZERO; last = Some(n)` |
| `increase_time(d)` | `elapsed += d`（HLS 唯一入口，行为不变） |
| `reset()` | `elapsed = ZERO`，`last` 保留（下一段接着当前基准算增量） |
| `full_reset()` | `elapsed = ZERO; last = None` |
| `Default` / `new()` | `elapsed = ZERO; last = None` |

`MAX_STEP` 定成模块级 `const`，注释写清它的依据是停顿看门狗的 30s（`httpflv.rs` 的
`DEFAULT_STALL_TIMEOUT`）——段内不可能有比它更长的合法空档，因为更长就断连了。
两个值没有编译期耦合，注释里点名关系即可，不为此建跨模块常量。

`set_time_position` 里那段「时间戳回绕后 elapsed 恒为 0」的注释和整个 `number < start`
分支一起删掉——新模型下回绕不再是特例。

`Debug` 输出会变（`splitting.{segment:?}` 打在 info 里）。这是诊断行不是契约字段，
`elapsed`/`last` 比 `start`/`current` 更好读，不做兼容。

## 测试

### `util.rs` 单测

- **保留** `a_backward_timeline_rearms_the_segment_start`（真回绕仍要能切）：
  start 1000s → 1005s → 0 → 10s，最后一步应满足 10s 阈值（elapsed = 5s + 10s）。
- 新增 `a_single_out_of_base_tag_does_not_inflate_elapsed`：
  start 32_891_256ms，位置 +1s、+2s，插一个 0，再回到 +3s，断言 `elapsed ≈ 2s`、`!time_needed()`。
- 新增 `a_gap_longer_than_max_step_is_not_counted`：跨 MAX_STEP 的前向跳变不计入。

### `httpflv.rs` 端到端回归

仿 `flv_with_repeated_metadata` 加一个 fixture `flv_with_absolute_base_and_zero_keyframes`：
FLV 头 + onMetaData + 两个序列头，然后关键帧从 `32_891_256` 起每帧 +1000ms，
**每两个关键帧之间插一个 timestamp=0 的关键帧**（不是 script tag——#32 那条已经覆盖 script）。

用 `Segmentable::new(Some(Duration::from_secs(3)), None)` 跑 `parse_flv`，断言：

- `progress.splits` 等于按 3s 步进应有的刀数（不是每帧一刀）；
- 每个 `recording.segment_closed` 的 `size_bytes` 都远大于单个 CDN 初始化 tag 的体量
  （即不出现「几百字节的碎片」这个现象本身）。

第二条断言用 `Captured` 订阅器取事件，写法照 `a_size_split_closes_the_segment_with_split_limit`。

注意 fixture 里 timestamp=0 的关键帧会被写进文件并触发 `dts_backward`——这是真实行为，
不要为了让测试干净而回避，本 step 不改 DTS 汇总。

## 验收

- `cargo test -p biliup` 通过，`cargo clippy -p biliup` 无新增告警。
- 新增的端到端用例在**改动前**必须失败（先跑一次确认它真的抓得住这个 bug），改动后通过。
