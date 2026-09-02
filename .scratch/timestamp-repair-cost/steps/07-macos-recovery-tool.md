# 07 · macOS 侧的取回—修复—回推工具

Status: ready-for-agent
Blocked by: 06（已完成，凭证已落库、通道已实测）
优先级：P2——不阻塞发布

## 前提（06 已验证，别重新摸索）

- 取回：`GET https:{endpoint}/{upos_uri 去 upos:// 前缀}` + header `X-Upos-Auth: {auth}`。
- 三个字段存在 `upload_missing_segment.upos_recovery_json`（migration 24），TTL 7 天。
- **取回按线路存在**：tx / bda2 / alia 的 GET 逐字节一致；**bldsa 只有 HEAD 200，GET 403**。
- 匿名访问一律 403，凭证与对象绑定，事后重新 preupload 拿到的新 auth 无效。
- 回推用 `crates/biliup` 已有的 `edit` / `edit_by_web` / `edit_by_app`；CLI 侧的
  `LegacyFinalizedEdit` 是现成的「往已定稿稿件里动分 P」先例。

## 交付形态

一条 `biliup` 子命令，形态对齐 `cover-preview` 那种本地工具。不要写成 `scripts/` 脚本——
取回要复用现有的鉴权与 client，重新拼一套鉴权是白干。

分成可以单独跑的几个子步骤，别做成一个不可中断的大动作：取回、修复、验收、回推各自可独立
执行并把中间产物留在磁盘上。修完的片子在回推前一定要能让人自己看一眼。

## 四步

1. **取回**
   - 输入是 `missing_id`（告警里带得出来）。
   - `endpoint` 是 bldsa 时**直接报「这条线路没有取回通道」并退出**，不要静默下载半截。
   - 落地后用 `Content-Length` 与 `upload_missing_segment.total_bytes` 对一次。
   - 描述符过期/被清成 NULL 时给出明确提示，而不是空指针式的失败。

2. **修复**
   - **不要把服务端删掉的那条 x264 搬过来。** 05 删它是对的，本机性能好不等于该做蠢事。
   - 优先试**按检测到的偏移量做 `TS+offset` 平移**：`max()` clamp 会把重叠/回退那段压扁，
     平移能把内容保住。偏移量从 `ffmpeg_scan::parse_backward_ms` 已经解得出来。
   - 平移方案在时间戳重置形态下是否成立要实测——03 的两个构造样本可以直接复用
     （`splice(..., keep_timestamps = false)` 那个就是重置形态）。
   - 平移不成再谈重编码，且要说清为什么。

3. **验收**（照搬 03 的核对表，别另发明一套）
   - 全片扫描命中数为 0；
   - packet 数与源一致（不丢帧）；
   - 时间轴首尾、末尾 A/V 偏差与源一致；
   - 内容跨度符合预期。

4. **回推**
   - 上传修复件拿到新 `filename` → 用 `aid` 走 `edit` **替换该分 P**，不是重新投稿。
   - 回推是不可逆的对外动作：**执行前必须让人确认**，把「要改哪个 aid 的第几个分 P、
     用哪个文件」打出来等一次明确的 yes。

## 顺带要答的

- **凭证有效期**：06 只证明了刚上传完能取回，没测 T+1 / T+7。测出来才知道 TTL 7 天是不是
  合适，以及告警里要不要写「请在 N 天内处理」。这个只能靠时间，先留一个已上传的对象定期探。
- ~~**线路策略**~~：已由 [08](./08-prefer-recoverable-lines.md) 解决——AUTO 探测现在优先只在
  `RECOVERABLE_LINES` 里挑，其余线路兜底。**但历史分段不受影响**：08 之前上传的行，
  endpoint 可能仍是 bldsa，工具遇到时照样要报「无取回通道」。

## 注意

- 这一步全程碰账号凭证与稿件 id。按 `CLAUDE.md` 脱敏：spec / commit / PR 里不写 BV 号、aid、
  cookie 文件名、UID、房间地址；证据链保留 `trigger=` / `outcome=` 这类不带标识符的字段。
- 跑真实上传的测试会触发 601「上传过快」的账号级冷却（06 那轮触发过一次）。挑没有录制的
  时段跑，别在直播时间做。
