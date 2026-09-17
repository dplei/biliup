# 01 兄弟槽只在自己也偏离时采纳新 offset

Status: open

根因步骤。做完之后，任何一个槽的「幻影确认」都不会再把其它槽拖到错误时间轴上。

## 改哪里

只改 [`httpflv.rs`](../../../crates/biliup/src/downloader/httpflv.rs) 的 `StreamBase` /
`TimestampRebase`，`parse_flv` 的调用处与 `flv_writer.rs` 不动。

```rust
struct StreamBase {
    last_src: Option<i64>,
    last_emit: Option<i64>,
    pending: Option<i64>,
    /// 本流当前的修正量。同一基准内各流相同；某流确认换基准后，其它流通过 `adopt` 跟上。
    offset: i64,
    /// 兄弟流刚确认的新 offset，等本流自己也偏离旧基准时采纳；本流仍在旧基准上则作废。
    adopt: Option<i64>,
}

struct TimestampRebase {
    /// 最近一次确认的 offset，只给某流的第一个 tag（还没有判据）用。
    offset: i64,
    high_water: i64,
    streams: [StreamBase; 3],
}
```

`map` 的四条分支改成：

| 分支 | 现状 | 改后 |
|---|---|---|
| `last_src == None` | `emit = src + self.offset` | 同，另 `stream.offset = self.offset; adopt = None` |
| `follows(last, src)` | `emit = src + self.offset` | `emit = src + stream.offset; adopt = None` |
| 偏离，`adopt = Some(o)` | （无此分支，被清空后走 `None`） | `stream.offset = o; last_src = src; pending = None; emit = src + o`；**`deviation` 保持 `None`** |
| 偏离，`follows(pending, src)` | 更新 `self.offset`，**清空兄弟的 `last_src`/`pending`** | `self.offset = stream.offset = high_water + GAP − src`；兄弟 **`adopt = Some(self.offset)`**，`last_src`/`pending` 不动 |
| 偏离，首次 | `pending = src; emit = high_water + GAP` | 不变 |

`emit.max(last_emit + 1)`、`high_water` 维护不变。

`adopt` 在 `follows` 分支**无条件清掉**（包括 `src == last` 的原地重发）：本流拿到一个旧基准
的时间戳就足以说明它没跟着换，最坏情况是之后它自己再走一遍 `pending` 二次确认，代价是两个
占位 tag，绝不会是跳变。

## 必须留的测试

先加复现测试，确认它在现状下失败（分析时实测 `video 前跳 3794020ms`），再改：

```rust
/// issue #62：Script 槽的 last_src 停在切段 restamp 的 K，424s 后 CDN 连发两个
/// timestamp=0 的 script tag。第二个被当成「新基准确认」，A/V 槽判据被清空，
/// A/V 下一个 tag（仍在旧基准）被无条件套上新 offset，落盘时间戳前跳 ≈ K。
#[test]
fn a_phantom_confirmation_does_not_drag_the_other_streams() {
    const K: u32 = 3_790_667;
    let mut rebase = TimestampRebase::default();
    let mut emit = |t: TagType, src: u32| rebase.map(t, src).emit;
    emit(TagType::Script, K);
    emit(TagType::Audio, K);
    emit(TagType::Video, K);
    let mut last_v = 0;
    for i in 1..=100u32 {
        emit(TagType::Audio, K + i * 33 - 10);
        last_v = emit(TagType::Video, K + i * 33);
    }
    emit(TagType::Script, 0);
    emit(TagType::Script, 0);
    let next_v = emit(TagType::Video, K + 101 * 33);
    assert_eq!(next_v - last_v, 33, "video 必须按真实增量继续");
}
```

再补两条（spec 的推演表）：

| 测试 | 输入 | 断言 |
|---|---|---|
| video 槽成对重发 `[…, B+n, 0, 0, B+n+33, …]`，audio 正常 | R2 形态 | audio 恒等映射；video 整体单调、每步 ≤ 一帧量级（占位 tag 用 `REBASE_NOMINAL_GAP_MS`），三个占位 tag 之后与 audio 回到同一时间轴（`emit_v − emit_a` 与跳变前相差 ≤ 一帧） |
| 真换基准，audio 领先 video 20 ms（复用 `interleaved_*` 的形态）：`[a(B−20), v(B), a(980), v(1000), a(1980), v(2000), …]` | 先确认的槽把 offset 挂给兄弟 | 换基准之后 `emit_v − emit_a` 仍是 20 ms（逐 ms 相同，不是「大致」） |

现有 6 条 `rebase` 单测与 `flv_with_a_mid_stream_base_change` 端到端测试必须原样通过。

## 观测

- `Mapped.deviation` 的语义不变：只有走 `pending` 路径的 tag 报 deviation，采纳分支不报，
  `recording.dts_backward` 事件与 rollup 计数与现在一致。
- 幻影确认的日志签名从「两条 `current: 0` + 一段错误时间轴」变成「两条 `current: 0` + 该槽
  两三条 `timestamp_jump_forward`」，之后无事。验收清单按这个写。

## 收尾

- `CODE_INDEX.md` 里 `httpflv.rs` 的「修正量全局唯一」改成「修正量按槽维护、确认后由兄弟槽在
  自己偏离时采纳」，跑 `python3 scripts/check_code_index.py`。
- PR 正文：根因（R1 机制，不要照抄 issue 的两条推断）、方案、`cargo test -p biliup --lib rebase`
  输出。**不写 `Closes #62`**。
