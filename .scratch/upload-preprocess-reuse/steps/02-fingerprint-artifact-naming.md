# 02 — 预处理产物按内容指纹命名

Status: wontfix

## 结论

不实现。

PR #14 已把默认形态改成“校验通过后原子替换原片”，失败补传通过
`upload_missing_segment.audio_normalized_at` 跳过再次编码。正常链路不再存在需要跨 attempt 识别的独立
标准化成品，随机名只属于转码中的 `.part` 半成品，正适合避免冲突并由现有清扫器回收。

内容指纹还需要把源路径、大小、mtime、loudnorm 参数、编码参数和 schema version 固化成缓存协议；
当前没有消费者，增加这些约束只会创造失效与兼容问题。

## 重新考虑的条件

只有未来取消默认原地替换，同时又有实测证明跨进程重复 loudnorm 是主要成本时，再单独设计持久缓存；
不能只因为 `audio_normalization_keep_original=true` 这个逃生门存在就预建。

## Comments

- 2026-09-08：被 PR #14 的原地替换方案取代。
