# 02 · dev 限速实跑 + 生产高峰窗口验收

Status: needs-triage

Blocked by: 01

## dev 实跑（合并前）

本机起 `biliup server` + `pnpm dev`，本地 sqlite，投稿模板只对自己可见。本机上行带宽有限，
**必须显式指定上传线路**，不要走 AUTO 探测。

1. 用 macOS 的 `dnctl` + `pfctl` 把到 `*.bilivideo.com` 的出向限到 2 Mbit/s，模拟整机限速。
2. **无证据路径**：清空 `upload_line_health` 中的冷却，上传一个大于 200 MB 的分段。
   预期：90 秒后出现 `verdict="abort"`，行为和旧版一致。
3. **有证据路径**：往本地库给**另一条**线路写一条 `last_failure_kind='slow_transfer'`、
   `cooldown_until` 在未来的记录，重新上传。预期：出现 `verdict="tolerate" reason="machine_wide"`，
   上传持续推进，最后以成功收尾；如果超过 2 小时，能看到总时长豁免的 `info!`。
4. 解除限速，确认下一次上传不受影响。

每一步都把日志链（去掉标识符）记到本文件的 `## 结果` 下。

## 生产验收清单（合并后贴到 issue #82，标 `awaiting-verification`）

在至少一个完整的高峰限速窗口里，下面几项都成立，才能关闭：

- [ ] 出现 `watchdog="slow_transfer" verdict="tolerate" reason="machine_wide"`，而且 `evidence_lines` 非空。
- [ ] 同一个限速窗口里，`verdict="abort"` 最多出现一次，也就是第一次判慢。
- [ ] 被忍住的 attempt 最后是 `outcome="succeeded"`，不是 `total_upload_timeout`。
- [ ] 窗口结束后，被挡住的会话能正常投稿（`submitted=[...]` 里包含它）。
- [ ] 非限速时段，单线劣化仍然会 `verdict="abort"` 并换线，没有被误判成整机限速。

## 结果

## Comments
