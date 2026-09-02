# 07 · macOS 侧的取回—修复—回推工具

Status: resolved
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

## Answer

交付形态改了：**不是一条大子命令，是一个 Skill**。主人的流程是「B 站审核发现问题 → 在生产
或日志里找到对应信息 → 交给本机的 codex/claude → 调用 skill 跑完后续」，所以这条流程的
主体是**判断力**，不是可执行文件。判断力放进 skill，可执行的部分只补真正缺的那一件。

### 交付物

- `.claude/skills/segment-recover/SKILL.md` 与 `.agents/skills/segment-recover/SKILL.md`
  （两份只差描述里的 Claude/Codex，对齐 `cover-background` 的既有做法）。
- `scripts/timestamp_shift.py`：算平移参数并打印现成的 ffmpeg 命令。
- `biliup append --replace <分P序号> [--execute]`：**替换**分P而不是追加。

### 大部分环节零新代码

原计划想做一条 `biliup segment-recover` 子命令，做之前重新数了一遍每一环真正缺什么：

| 环节 | 用什么 | 要写代码吗 |
| --- | --- | --- |
| 取回 | `sqlite3` 读描述符 + `curl -H "X-Upos-Auth: ..."` | 不用 |
| 修复 | `ffmpeg` + `scripts/timestamp_shift.py` | 一个小脚本 |
| 复检 | `ffprobe` + `awk` | 不用 |
| 回推 | `biliup append` **只能追加，不能替换** | **缺这一件** |

所以只补了替换。它是 `append` 的同一条路径（login → studio_data → 改 videos → edit），
差别只有一行 `videos[i] = 新的` 对 `videos.append(...)`，做成 `--replace` 标志而不是新
子命令，省掉一整套重复的登录/上传/edit 管线。

两处安全设计：**稿件先取回来再上传**（序号写错时不该已经白传一遍），以及 `--execute`
预演开关（对齐仓库里 `Reply` 的既有做法，默认只打印将要做的改动）。分P标题跟着老分P走
——换的是坏掉的那个文件，不是这一P的身份。

### 修复方法：平移，不是夹取（已实测）

服务端用 `setts=...max(TS, PREV_OUT+1)` 夹取，回退量一大就把后面的内容压成帧风暴，
所以那边有 10 秒闸门（05）。本机没这个限制，改用**按回退量整体平移**。在 03 的重置样本
（回退 25 秒）上实测：

| | 扫描命中 | 包数 | 被压扁的包 | 时间轴 |
| --- | --- | --- | --- | --- |
| 夹取 | 0 | 不变 | 350 个，后半段全毁 | 12.34s（错） |
| 平移 | 0 | 不变 | **0 个** | 38.32s = 25.0 + 13.3，正确 |

`timestamp_shift.py` 逐流找回退点、按典型帧间隔补偏移、多次回退累加，输出的参数与手工
推导逐位一致；干净文件会直说「不需要平移」。

### 单测

- `replace_index` 的两条：分P序号从 1 起、越界时要说清稿件到底有几个分P（写错序号是这条
  流程最容易犯的错，而它作用在真实稿件上）。
- `cargo test -p biliup -p biliup-cli` 全绿。

### 遗留

- **凭证有效期**仍未测 T+1 / T+7，TTL 7 天是估的。这只能靠时间。
- **历史分段**：08 之前上传的行 endpoint 可能是 bldsa，skill 第 1 步会直接判「无取回通道」。
- skill 本身还没在真实故障上跑过——下次真出问题时是第一次实战，届时按实际卡壳的地方回来
  补 SKILL.md。
