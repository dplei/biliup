# 06 · 修不了的片子改到本机 macOS 上重修

Status: resolved
Blocked by: 05（已完成）
优先级：P2——不阻塞发布，05 落地后生产已经是安全的

## 背景

05 之后，回退量超限的分段会**原片直传 + 告警 + 本地清理**。稿件里那一段画面完整、
只是时间轴异常；本地不再留档。

主人的决策是：**不在录制服务器上重修**。那台机器只有 2 vCPU、1.6 GiB 内存，issue #25
整件事就是它扛不住整段像素重编码。要重修就凭上传凭证从 B 站 OS 库把原片取回来，在本机
macOS 上做——它性能好得多，而且不占录制/上传链路。

> 主人已实测过「凭 UPOS 上传时的凭证从 B 站 OS 库下载原片」这条路可行。可能有有效期，
> 但问题一般两天内会处理完。

## 待定（开工前先答，别默认）

- **凭证从哪来**：UPOS 上传时的凭证现在有没有落库？`upload_missing_segment` /
  `upload_attempt` 里存了什么？不够的话要补哪些字段、补在哪一步。
- **有效期**：凭证/对象的可下载窗口有多久？超期之后是什么表现——报错还是静默 404？
  这决定告警里要不要写「请在 N 天内处理」。
- **交付形态**：一条 `biliup` 子命令（对齐 `cover-preview` 那种本地工具的形态），
  还是一个 `scripts/` 下的脚本？倾向子命令：下载要复用现有的鉴权与 client。
- **重修用什么**：本机可以负担整段重编码，但未必需要——先试 setts 的其它表达式
  （例如按检测到的偏移量做 `TS+offset` 平移而不是 `max()` clamp），平移能保住被压扁的
  那段内容。这才是「性能更好的机器」真正该做的事：**试更贵但更正确的修法**，而不是
  把服务器上那条 x264 原样搬过来。
- **修完怎么回去**：B 站「修改视频」替换分 P，还是重新投一次？

## 不要做的

- 不要把重修塞回服务端的预处理链路。05 刚把它拆掉。
- 不要为此在服务器上重新保留本地原片——「本地不留档」是这一步成立的前提。

## 注意

这一步会碰到账号凭证与稿件 id。按 `CLAUDE.md` 脱敏：spec、commit、PR 里都不写 BV 号/aid、
cookie 文件名、UID、房间地址；证据链保留 `trigger=` / `outcome=` 这类不带标识符的字段。

## Answer

本轮把 issue #13 的 UPOS 补充读完、在本地跑通了真实端到端验证、把凭证落库做掉，并据实测
结论定下 macOS 侧的流程。**实现留给 [07](./07-macos-recovery-tool.md)。**

### 凭证链路（已落地）

取回一个对象需要三个字段，全部来自 preupload 的响应体，也就是
[`upos::Bucket`](../../../crates/biliup/src/uploader/line/upos.rs)：

```text
GET https:{endpoint}/{upos_uri 去掉 upos:// 前缀}
Header: X-Upos-Auth: {auth}
```

新增 `UposRecovery` + `Bucket::recovery()` + `Parcel::recovery()`（必须在 `upload()`
消耗 parcel **之前**调用）。落库走 `UploadActivity::UposRecovery` 这条已有的 activity
通道——拿到描述符的 `upload_single_file` 手上没有 `missing_id`，而 watchdog 循环两样都有，
`NormalizedInPlace` 就是现成的先例。存进 `upload_missing_segment.upos_recovery_json`
（migration 24），明文，写新描述符时顺带把超 TTL（7 天）的清成 NULL，没有独立定时任务。

明文的理由写在 migration 里：同一个 `data/` 目录已经放着 cookie 文件，而 cookie 的权限远
大于「下载自己某一个对象」的短期令牌，只加密这一列是安全剧场。约束改为**这一列不得进入
日志、事件或告警**。

### 端到端实测（2026-09-02，真实账号，未投稿）

`upos_recovery_round_trip_by_line`，每条线路传一个几字节的临时对象再取回：

| 线路 | 匿名 HEAD | 带 auth 的 HEAD | 带 auth 的 GET |
| --- | --- | --- | --- |
| `bldsa`（B 站自建） | 403 | 200 | **403** |
| `tx` | 403 | 200 | 200，逐字节一致 |
| `bda`（bda2 端点） | 403 | 200 | 200，逐字节一致 |
| `alia` | 403 | 200 | 200，逐字节一致 |

**这是 issue #13 那张表没覆盖到的一层：取回通道是按线路存在的。** 同一个 bucket、同一套
auth 机制，差别只在 endpoint——bldsa 上的对象，凭证能证明它还在（HEAD 200），但拉不回内容。
GET/HEAD/Range/query-param 各种带法都试过，bldsa 一律 403。

对生产的影响：本地库里出现过的线路是 `bda2`，`IMPLICIT_FALLBACKS` 是 `["bda2", "tx"]`，
两条都可取回。但配置默认是 `AUTO`，而 AUTO 探测的是 B 站当场返回的线路表，**可能落到
bldsa**——那些分段没有取回通道。这是 07 要在文档里写清的前提，也可能值得把线路策略从
AUTO 收成显式白名单。

> 跑这轮验证时触发了一次 601「上传过快」。是账号级的短时冷却，几分钟自愈，但当时如果有
> 真实分段在传会被 rate gate 挡一下。后续再跑这条测试请挑没有录制的时段。

### macOS 侧流程（设计，07 实现）

四步，全部在本机跑，服务器只出凭证：

1. **取回**：从生产库读 `upos_recovery_json` → `GET object_url()` + `X-Upos-Auth`。
   落地校验用 `Content-Length` 对 `upload_missing_segment.total_bytes`。
   bldsa 的行直接报「无取回通道」，不要静默半截下载。
2. **修复**：本机可以负担更贵但更正确的修法。**不要把服务器上那条 x264 搬过来**——
   05 删掉它是对的。优先试按检测到的偏移量做 `TS+offset` 平移（保住被 clamp 压扁的那段
   内容），平移不成再谈重编码。判据沿用 05 的回退量口径。
3. **验收**：修完的文件必须过 `normalize_timestamps` 的复检，且 packet 数与源一致、
   A/V 末尾偏差与源一致——03 的核对表可以直接搬。
4. **推送回原稿件**：`crates/biliup` 已经有 `edit` / `edit_by_web` / `edit_by_app`，
   CLI 侧也已有 `LegacyFinalizedEdit` 这条既有的「往已定稿的稿件里补分 P」路径。
   所以是「上传修复件拿到新 filename → 用 aid 走 edit 替换该分 P」，不是重新投稿。

### 遗留给 07 的待定

- 凭证有效期实测：本轮只证明了刚上传完可取回，没有测 T+1 / T+7。这决定 TTL 该不该调，
  以及告警里要不要写「请在 N 天内处理」。
- 交付形态：倾向 `biliup` 子命令（复用现有鉴权与 client），而不是 `scripts/` 脚本。
- 线路策略是否从 AUTO 收成「只用可取回的线路」。
