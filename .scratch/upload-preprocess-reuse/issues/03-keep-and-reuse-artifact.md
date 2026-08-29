# 03 — 上传失败后保留并复用标准化产物

Status: wontfix

## 结论

不实现独立缓存。

默认配置下，标准化成功时产物已经替换到原路径，并立即发送 `NormalizedInPlace` 活动让 watchdog 写入
`audio_normalized_at`。自动补传、人工补传和换线重投都用现有 `audio_normalization_needed` 判据跳过
loudnorm，已经得到原 ticket 想要的“失败后复用”，且不需要额外文件生命周期。

`audio_normalization_keep_original=true` 刻意退回临时产物形态，用于必须保留原片的用户。为这个非默认
模式保留失败缓存会重新引入双份磁盘占用、跨进程命中和回收协议，代价高于收益。

## 保留的回归

实现 Step 01 时保持现有 `recovery_skips_normalization_for_an_already_replaced_recording` 一类回归通过；
不要改变 `audio_normalized_at` 的含义或 `keep_original` 的清理语义。

## Comments

- 2026-09-08：被 PR #14 的原地替换与持久标记取代。
