# 子任务 01：事故 fixture 与可靠性不变量

Status: ready-for-agent

Blocked by: none

## 目标

在改变生产逻辑前，把本次事故固化为可重复、可脱敏、无需生产环境的测试场景。后续每个子任务必须先让对应测试失败，再实现修复。

## 详细步骤

### 1. 建立测试场景模型

- [ ] 为上传流程提供可注入的假 uploader、假时钟和可控 SQLite 测试库。
- [ ] 假 uploader 能分别停在 probe、pre-upload、首块、任意中间块和完成回调。
- [ ] 假 uploader 能模拟 TransportError、HTTP 错误、证书过期、永久 pending 和延迟成功。
- [ ] 测试时禁止访问真实 B 站接口。

### 2. 固化未登记分段事故

- [ ] 构造三个 `SegmentInfo`，文件名对应 22:29:32、22:59:56、23:30:19。
- [ ] 令媒体验证全部返回 Valid。
- [ ] 模拟 UActor 正在处理另一条长生命周期 receiver。
- [ ] 断言旧实现只产生 validated 日志但没有 session/missing 行。
- [ ] 为目标行为增加断言：验证完成后即使上传 Actor 尚未消费，三段也均已持久登记。
- [ ] 模拟登记后下一次下载以 TransportError 结束，确认前一个分段不受影响。

### 3. 固化不完整投稿事故

- [ ] 构造 session，包含两个 succeeded 和一个 uploading 分段。
- [ ] 触发下播 finalize 流程。
- [ ] 断言旧实现会继续调用 submit。
- [ ] 目标断言为 submit 调用次数 0，session 写入 `blocked_missing_segments`。
- [ ] 分别覆盖 pending、failed、source_missing 和无 `video_json` 的 succeeded 异常。

### 4. 固化重复与乱序事故

- [ ] 对同一个本地路径重复发送 SegmentEvent。
- [ ] 同时触发直播上传管道和人工补扫。
- [ ] 模拟旧 attempt 在新 attempt 成功后才返回成功。
- [ ] 构造两个标题都为 `04:54:30`、但源路径不同的合法分段，确认不能仅凭标题误去重。
- [ ] 构造同一路径、相同 session、相同 logical order 的重复事件，目标结果只能有一个分 P。
- [ ] 覆盖缺失段先失败、后续段先成功、缺失段最后补齐的顺序恢复。

### 5. 固化上传卡死与线路故障

- [ ] 假 UPOS 在上传一个分块后永久不返回下一块。
- [ ] 假时钟推进 5 分钟，断言旧任务被取消并释放全局 semaphore。
- [ ] 持续产生进度但总时间推进到 2 小时，断言触发总时长超时。
- [ ] bda2 首次失败后检查下一 attempt 选择 tx。
- [ ] bldsa 返回 `invalid peer certificate: certificate expired`，其他线路返回成功。
- [ ] 断言 TLS 错误不会导致关闭证书校验或全局停止上传。

### 6. 固化源文件和 finalized 行为

- [ ] 创建 pending 行后删除源文件，触发恢复。
- [ ] 目标断言状态变为 `source_missing`，attempts 不再自动增长。
- [ ] 对 finalized session 执行补扫，目标断言不创建新 session 或 active missing。
- [ ] 对事故前已经绑定 finalized session 的 missing 行执行人工恢复，保留编辑原稿能力。

## 测试数据安全

- [ ] fixture 不保存 Cookie、鉴权头、签名 query、真实用户 UID 或完整拉流 URL。
- [ ] 错误快照最多保留必要的错误分类和脱敏 host。
- [ ] FLV fixture 使用合成媒体或最小合法容器，不复制生产录像内容。

## 验收标准

- 上述六组场景均有独立测试名称，失败信息能指出违反了哪条不变量。
- 测试能够在本地和 CI 重复运行，不依赖墙钟等待。
- 修复前预期失败、对应子任务完成后转为通过。

