# 03 · dev 实跑与阈值校准

step 01、02 的阈值（`SLOW_RATIO = 4.0`、`SLOW_COOLDOWN = 30min`、`SLOW_WINDOW = 90s`、
前半段保护线 50%）都是从一次实测反推的初值，必须在真实链路上确认「不误伤」比「抓得准」更优先。

## dev 环境（本地 `biliup server` + `pnpm dev`，本地 sqlite，仅自己可见的投稿模板）

本机上行只有 30 Mbps，AUTO 探测那套 4×10MB/4s 的门槛必然全失败——**必须显式指定上传线路**，
不要用 AUTO 复现。

1. 正常线路跑通一次真实上传，确认 `avg_mbps` 落库、`record_success` 的 `None` 路径未回归。
2. 人为压低带宽（`pf` / 限速代理）到基线的 1/10，确认：
   - step 02 在 90 秒窗口内中止，日志出现 `watchdog=slow_transfer`；
   - 重试换线成功；
   - 已传过半时不再中止。
3. 三条 recoverable 线路人为全冷却，确认安全阀只 warn 不冷却，选路没有掉进「放开限制重探」。

## 生产观察（上线后一周）

只看日志链与结论，不抄行数/会话 id/时间戳：

- 有没有 `slow_throughput` 冷却发生在**健康**时段（误伤信号）。若有，调高 `SLOW_RATIO` 分母
  （更保守）或拉长 `SLOW_WINDOW`。
- 有没有明显劣化的上传**没有**被判到（漏抓信号，如 8.55 MB/s 这一档）。若漏抓且无误伤，
  再把 `SLOW_RATIO` 往 3.0 收。
- `avg_mbps` 在各线路上的分布是否稳定，基线（全库 MAX）会不会被某次异常高值污染。

## 收尾

阈值定稿后：`.scratch/upload-line-degradation/` → `.archive/`，补 `.archive/README.md` 一行，
更新 `CODE_INDEX.md` 里 `upload_line_health.rs` / `upload.rs` 的职责摘要，跑
`python3 scripts/check_code_index.py`。

## 落地（dev 部分已完成）

结果见 [`../verification.md`](../verification.md)。五项全过：冷启动只写 EWMA、判慢按 30 分钟
冷却、950 MB 在第 90 秒 33% 处中止、497 MB 同速率过半后不中止、三条可取回线路只剩一条时
安全阀只 warn。

两处与本文计划的偏差：

- **没用 `pf` 限速，改为抬高基线。** 判据是纯比值，抬基线 ≡ 压带宽，且不需要 sudo。
- **「重试换线」没能在 dev 复现。** dev 配置把线路钉死（AUTO 在 30 Mbps 上行必然探测失败），
  而显式线路不受冷却影响，换线无从触发。它走的是 selection 既有的 `active_cooldowns` →
  `excluded` 通道，本次改动没有碰。

阈值维持初值。**生产观察一周那一节仍未开始**，所以本 effort 继续留在 `.scratch/`。
