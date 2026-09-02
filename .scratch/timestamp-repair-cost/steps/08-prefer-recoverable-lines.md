# 08 · auto 探测优先挑有取回通道的线路

Status: resolved
Blocked by: 06（取回按线路而定，实测证据在那边）
优先级：P1——它决定 05「本地不留档」那个前提在多大范围内成立

## 为什么

05 之后，修不好的分段是「原片直传 + 告警 + 本地清理」，而清掉本地文件的前提是**需要时能
从 B 站把原片取回来**。06 实测出这个前提有条件：

| 线路 | 匿名 HEAD | 带 auth 的 HEAD | 带 auth 的 GET |
| --- | --- | --- | --- |
| `bldsa` | 403 | 200 | **403** |
| `tx` / `bda2` / `alia` | 403 | 200 | 200，逐字节一致 |

配置默认是 `AUTO`，而 AUTO 探测的是 B 站当场返回的线路表，**可能落到 `bldsa`**。落上去的
分段就算凭证存好了也拿不回内容——只能证明对象还在。

## 做法

主人给了两条路：配置不用 AUTO，或者干脆把 AUTO 改掉。选**后者**：改 AUTO 是配置无关的，
而且顺带覆盖「显式线路在冷却、回落到探测」这条同样会踩到 bldsa 的路径；只改配置的话，
那条路径依然裸奔。

- `crates/biliup`：`Probe::probe_filtered_with_failures(client, allowed, excluded)`，
  `allowed` 为空表示不限定。原来的 `probe_excluding_with_failures` 变成它的一层包装。
  候选过滤抽成纯函数 `retained_lines`，四条单测（不限定 / 限定 / 冷却优先于白名单 /
  白名单命中为空）。
- `upload_line_selection.rs`：新增 `RECOVERABLE_LINES = ["bda2", "tx", "alia"]`，
  probe 先只在这几条里挑；**它们全不可用时放开限制重探并 `warn!`**，因为传不上去比失去
  取回通道更严重，但要把代价说出来。

## 三个刻意的边界

- **白名单是实测的，不是按厂商推断的。** 只有这三条跑过端到端 GET 比对。`cnbd` / `anbd` /
  `atbd` / `txa` 这些看着也是云厂商 CDN，很可能同样可取回，但没测就不进白名单——猜错的
  代价是「以为能取回、真要用时发现不能」，那是最坏的发现时机。要扩白名单就跑 06 那条
  ignored 测试，挑没有录制的时段（真实上传会触发 601 账号级冷却）。
- **显式配置的线路不受影响。** 主人点名要哪条就用哪条，这是既有语义，也是想要速度不要
  取回通道时的逃生口。
- **`biliup upload` 那条独立 CLI 路径没改**（`uploader.rs` 里的 `Probe::probe`）。
  它传的是用户本来就有的本地文件，不走 `upload_missing_segment`，也不存取回描述符——
  取回通道在那里换不来任何东西。这是判断，不是漏改。

## 验收

- `cargo test -p biliup -p biliup-cli` 全绿（`biliup` 60 passed，`biliup-cli` 351 passed）。
- 新增 `probe_filter_tests` 四条，覆盖过滤语义；`plan_upload_line` 是纯函数、未受影响，
  既有线路决策测试原样通过。

## 遗留

`RECOVERABLE_LINES` 只有三条，AUTO 的可选面因此变窄了。如果实际用起来发现探测经常落空、
频繁走到「放开限制」的分支，那说明该扩白名单——扩之前先测，别直接加。
