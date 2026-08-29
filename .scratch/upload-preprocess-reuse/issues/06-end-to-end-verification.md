# 06 — 回归验证与收尾

Status: ready-for-agent
Blocked by: 01

## 目的

证明瞬时预上传故障在同一个 attempt 内收敛，并确认 #14 已有的默认产物复用语义没有回退。

## 自动验证

1. 瞬时 `Transport` 两次后成功：外层 attempt 成功，预上传与 gate 准入各发生 3 次。
2. 601、HTTP、证书错误均不进入普通重试。
3. `Transport` 重试耗尽：attempt 失败，gate 不停在 `Probing`。
4. 所有预上传失败都不新增或递增 `upload_line_health`；实际分块上传失败仍会更新它。
5. 默认原地替换的分段在失败后恢复时，`audio_normalization_needed` 继续返回 false；不增加第二次
   loudnorm。
6. 页面整场上传和 `upload_single_file` 都调用同一个 helper，避免两套策略漂移。

运行：

```bash
cargo test -p biliup-cli
python3 scripts/check_code_index.py
```

## dev 验收

自动测试可以完整覆盖本次状态转换，不依赖真实账号或生产数据。若开发环境恰好可注入一次预上传网络
错误，只需补一条观察：同一 attempt 内先出现 retry、随后成功，且中间没有第二次
`audio normalization started`。没有遇到真实网络抖动不阻塞合并。

不再要求检查长期缓存占用或复用后音质：本方案没有缓存，上传的就是 #14 已校验并原地替换的文件。

## 收尾

Step 01 实现、上述自动验证通过并落到 `dev` 后：

1. 在本文件 `## Answer` 写入测试结果；
2. 把 01、06 标为 `resolved`；
3. 将整个 effort 移入 `.archive/` 并登记 `.archive/README.md`；
4. 在公开 PR/issue 文案中只保留脱敏后的故障链和验证结论。

## Comments

- 2026-09-08：删除对生产三场观察、缓存磁盘峰值和复用稿件音质抽查的要求；新方案无长寿命缓存，
  合成故障足以覆盖本次行为。
