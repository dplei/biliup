# 04 · 真实上传验收

按 issue #67「真实上传」段逐项执行并把日志证据（脱敏后的 `line_source` / `line` / `endpoint` 域名 / `outcome`）贴回本文件。全部勾选前不 `gh pr create`。

- 本机上行约 30 Mbps，AUTO 必失败，显式 `bda2` 是主验证路径。
- `akbd` / `estx` 若不可达，记录原因，PR 里如实标注未勾选。

## 实跑记录（2026-09-20，本机 dev）

环境：`data/` 复制到临时目录起后端（`c4009bf` 构建），前端仓库根 `pnpm dev`；
`segment_time=00:02:00`、`delay=60`；主播为主人给的抖音直播链接；模板 `is_only_self=1`。

| 分段 | 配置线路 | 日志 | 落库 endpoint 域名 | 结果 |
|---|---|---|---|---|
| order 0 | `bda2` | `upload attempt started … line="bda2" line_source="configured"` → `upload attempt completed` | `upos-cs-upcdnbda2.bilivideo.com` | 成功 |
| order 1 | `bda2` | 同上（切换配置前已开始） | `upos-cs-upcdnbda2.bilivideo.com` | 成功 |
| order 2 | `bda2` | 同上 | `upos-cs-upcdnbda2.bilivideo.com` | 成功 |

会话 5 暂停后（`delay=60`）由 `periodic_scan` 一次性投稿：`n_videos=3` → `APP接口投稿成功` →
`submit_ok_with_aid`，稿件 aid/bvid 已写回本地库（标识符按脱敏规则不入库）。

**发现**：线路在 `session_init` 决定一次并整场沿用（`UploadContext.line`），中途改 `lines` 不影响
正在录的会话，所以 `tx` / `akbd` 各开一场新会话验证。这是既有行为，不属于本次改动。

### 会话 6（`tx`）

| 分段 | 配置线路 | 日志 | 落库 endpoint 域名 | 结果 |
|---|---|---|---|---|
| order 0 | `tx` | `upload line selected context="session_init" configured="tx" chosen=tx source="configured"` → `upload attempt started … line="tx"` → `completed` | `upos-cs-upcdntx.bilivideo.com` | 成功 |

暂停后 `n_videos=2`（含暂停时的尾段）→ `APP接口投稿成功` → `submit_ok_with_aid`。

### 会话 7（`akbd`，旧表里从未登记过的线路）

| 分段 | 配置线路 | 日志 | 落库 endpoint 域名 | 结果 |
|---|---|---|---|---|
| order 0 | `akbd` | `upload line selected context="session_init" configured="akbd" chosen=akbd source="configured" candidates=akbd,bda2,tx,auto` → `upload attempt started … line="akbd"` → `completed` | `bb27c891csbd.aikobo.cn` | 成功 |

暂停后 `n_videos=2` → `APP接口投稿成功` → `submit_ok_with_aid`。**「不登记也能用」成立。**

### 收尾状态

- 三个会话 `upload_session` 均 `finalized / ok_with_aid`，`upload_line_health` 三条线路无失败、无冷却。
- 前端两页下拉（`c4009bf` 已在 step 03 实跑核对）：`AUTO / akbd / bda2 / bldsa / estx / tx`。
- 代码层：`grep fn bldsa|fn bda2|fn alia|enum UploadLine` 只剩两个测试函数名；`cargo test --workspace`
  626 通过 0 失败；`tsc` 通过；`next lint` 0 error；`next build` 成功。

### 与清单的偏离（如实）

- 「在上传页对同一分段选择 `tx` 重传」没有按字面做：分段一登记就立即上传成功，页面对 `succeeded` 行
  不提供换线重传入口，也没有失败行可用。改为**新开一场会话以 `tx` 为配置线路**走完整上传+投稿，
  覆盖的是同一个 `explicit_upload_line` → `Line::explicit` → `pre_upload` 路径；页面 `forced` 路径由
  单测 `a_manual_line_overrides_configuration_but_still_yields_to_real_cooldown` 覆盖。
- 三场稿件都是 `is_only_self=1` 模板，稿件标识符按脱敏规则不写入库内文档。
- 临时目录里的录像已删，仓库 `data/` 未被触碰。
