# `.archive/`：已完结工作的归档

`.scratch/` 是流程中的工作区，只放**还在推进**的 effort。一项工作彻底完结后，整个
`.scratch/<feature-slug>/` 目录原样移动到这里，`.scratch/` 保持只剩活跃项。

归档只挪位置，不改内容：spec、issue、验证记录一并保留，链接与引用继续有效。

## 什么时候算「完结」

同时满足才归档：

- 目录里每个 issue 的 `Status:` 都是 `resolved` 或 `wontfix`；
- spec / assessment 一类的总览文件不再留有待办（`ready-for-human`、`needs-info`
  都表示还有人要做的事，不归档）；
- 代码已落到 `dev`，验证结论已写进目录内的文件。

只要还有一条待真实环境验收、待补数据、待部署观察，就留在 `.scratch/`。

## 已归档

| 目录 | 内容 | 归档日期 | 完结依据 |
| --- | --- | --- | --- |
| [`2026-08-27-upload-incident-round2/`](2026-08-27-upload-incident-round2/) | 上传事故第二轮：尝试阶段与租约收敛、异步恢复接口、线路选择、会话连续性等 9 个 ticket | 2026-08-29 | assessment `implemented`，01–09 全部 `resolved` |
| [`audio-normalization/`](audio-normalization/) | 自动响度标准化与样片音量推子 | 2026-08-29 | spec 记「已实现并完成本机 FFmpeg 冒烟验收」，无待办 |
| [`2026-08-28-session-submit-liveness/`](2026-08-28-session-submit-liveness/) | 投稿会话活性修复：投稿意图状态、协调器、唤醒、补提交扫描与回归 | 2026-08-29 | 01–07 全部 `resolved`；生产只读核对确认迁移已应用、新路径投出过多分段稿、无卡住会话 |
| [`douyin-config-override-investigation/`](douyin-config-override-investigation/) | 抖音三个覆写开关「显示开启却不生效」的排查 | 2026-08-29 | `wontfix`：采集 + 代码核对证明覆写链路正常，前提是误判，观感问题实为 [#6](https://github.com/dplei/biliup/issues/6) |
| [`2026-08-26-session-227-audit/`](2026-08-26-session-227-audit/) | 一次投稿会话的分P重复/缺失只读审计 | 2026-08-29 | `resolved`：全库对照证明是孤立事故，源文件已删无法补传，决定不编辑稿件；审计逻辑已固化为 `scripts/consistency-audit.sh` |
| [`normalization-duration-metric/`](normalization-duration-metric/) | 响度标准化的时长口径：FLV 非零起始时间戳导致的全量误判，连带样片截取、失败证据与熔断 | 2026-08-31 | 01–05 全部 `resolved`；随 [#18](https://github.com/dplei/biliup/pull/18) 合入 `dev`，dev 环境真实录制 6/6 `completed`、0 次 `duration_drift` |
| [`empty-rescan-sessions/`](empty-rescan-sessions/) | 补扫不得复活零分段空会话 | 2026-08-31 | 01–04 全部 `resolved`；随 `2c1b871` 合入 `dev`，spec 唯一的保留理由（等待落 `dev`）已解除 |
