# 04 — 标准化成品缓存回收

Status: wontfix

## 结论

不实现。

Step 02、03 已撤销，系统不会新增长寿命标准化缓存。默认原地替换的峰值由 PR #14 的单并发转码槽和
两级磁盘水位约束；转码中的随机名 `.part` 仍由 `TempArtifact` 与
`cleanup_orphaned_normalization_artifacts` 清理。

没有缓存就不需要账本查询、24 小时 TTL、会话结束扫描或周期 reaper。删除整套回收设计也避免误删
待上传媒体的新风险。

## 重新考虑的条件

仅当未来真的引入持久标准化缓存时，与缓存本身在同一个 effort 里一起设计回收，不能提前落一个没有
生产者的清扫器。

## Answer

- 默认标准化成品通过原子替换进入原路径，不产生需要 TTL 管理的独立缓存文件；Step 02、03 也没有
  引入缓存生产者或账本。
- 现有 `TempArtifact` 负责正常与失败路径的短命文件清理；下一次标准化前，
  `cleanup_orphaned_normalization_artifacts` 只删除未登记的遗留 `.part`，不会碰活动产物。
- 磁盘峰值继续由单并发转码和准入/运行中水位限制。新增周期 reaper 没有目标且会扩大误删面，因此
  本 step 无代码改动，维持 `wontfix`。

## Comments

- 2026-09-08：随持久缓存方案撤销。
