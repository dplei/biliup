# 11 · 按真实生产措辞修复回退量解析器

Status: needs-info
Blocked by: 09 + 下一次 `timestamp_anomaly_unparsed` 诊断样本
优先级：P0——拿到证据后立即处理；当前禁止猜实现

## 为什么

目前只能证明 `stderr_indicates_anomaly` 命中，而 `parse_backward_ms` 对所有命中行返回 `None`。
没有原始 stderr，就不知道缺的是一种数值格式、单位、大小写，还是本来不携带回退量的异常形态。
录制侧的 `Non-monotonous DTS ... previous/current` 不是解析器输入，不能拿来写回归测试。

## 信息入口

09 上线后，从 `reason_code=timestamp_anomaly_unparsed` 的诊断附件取得脱敏后的原始命中行，连同
生产 ffmpeg 版本一起回写本文件。不要收集账号、稿件、房间或文件路径。

## 拿到样本后的做法

- 先把真实行逐字写成 `ffmpeg_scan.rs` 的最小回归测试，确认当前实现失败。
- 在共享的 `parse_backward_ms` / 异常扫描处做最小修复，覆盖所有调用者；优先解析稳定的数值结构，
  不堆版本专用的宽泛关键词。
- 保持未知格式仍返回 `None`，保持超过 10 秒拒绝，避免把观测修复变成安全闸门放宽。
- 跑解析器单测、`timestamp_repair` 测试和 `cargo test -p biliup-cli`。

## 出口

- 能从真实样本得到可信的最大单次回退量，并由回归测试锁住：改为 `ready-for-agent` 后实施。
- 真实行根本不含可推导的 previous/current 数对：记录结论，保留 `Unfixable`；不要虚构回退量，
  另评估是否需要数值型 ffprobe 判据。

## Comments

当前阻塞是缺真实输入，不是实现难度。09 解决“下次仍拿不到”的系统性原因。
