# 投稿后自动入合集在审核期必然失败

## 现象

1.3.46 上线后，自动入合集一次都没成功过。所有失败的报错都一样：

```
加入合集失败，将重试: get archive view failed: code -404 啥都木有   (attempt 1..5)
加入合集多次失败，已放弃（可稍后在创作中心手动添加）
```

## 根因

`BiliBili::add_archive_to_season` 加入合集前，先调用公开接口
`api.bilibili.com/x/web-interface/view` 取标题和 cid。刚投出的稿件处在审核期，公开接口对它返回
`-404`。调用方只重试 5 次、每次间隔 30 s，总共 2.5 分钟就放弃了，而审核远不止这么久。所以
只要稿件需要过审，这条路径必然失败，与网络无关。

已只读核对：失败的稿件过审后，公开接口能正常返回，但这些稿件都不在对应合集里。

## 官方前端怎么做（读 `member.bilibili.com/platform/upload/video/frame` 的 JS 确认）

- 投稿请求 `add/v3` **不带**合集字段，只有付费合集才走 `pay_season_id` / `section_id`。
- 投稿成功后立即调用 `POST /x2/creative/web/season/section/episodes/add`，参数为
  `{sectionId, episodes:[{title: 表单标题, cid: 首个视频上传阶段的 cid, aid: 投稿返回值, charging_pay}]}`。
  全程不查 view，说明 B 站接受把审核中的稿件加入合集。

## 方案候选

- A：照搬官方做法，用上传阶段的 cid（UPOS preupload 的 `biz_id`）。`Video` 结构不带 cid，
  增量会话的 `videos_json` 也没存 cid，要把 cid 贯穿所有上传路径，改动面大。
- B：保留「投稿后加入合集」的流程，只把取 cid 的接口换成创作中心的
  `member.bilibili.com/x/vupre/web/archive/view`。这是 UP 主自己的视角，审核期也能拿到标题和
  cid。

## 选定方案：B

B 站侧的语义两条方案相同：审核中的稿件可以带着视频 cid 调用 `episodes/add` 加入合集，B 方案已实测通过。
两者的区别只在于 cid 从哪来。B 只需改一个函数，A 要改上传数据结构，因此选 B。

「在投稿请求里直接带合集 ID」不可行：官方前端只对付费合集这样传，普通合集一律在投稿后再调用
`episodes/add`。不去猜未公开的字段。

上传页提示「分P稿件不支持加入合集」只是前端限制：线上合集里抽查 10 个，其中 9 个是多 P 稿件，
都经接口加入成功。

`add_archive_to_season` 也被补录（`/bili/seasons/backfill`）复用，修复后补录同样能把审核中的稿件加进去。

## 验证记录（dev 环境实测）

1. 在创作中心建了一个测试合集，只放仅自己可见的稿件，所以外部看不到。
2. 本地 dev 用仅自己可见模板手动上传了一个 10 s 测试视频，投稿成功后立即查询：
   - 公开 `web-interface/view` → `-404 啥都木有`（复现线上报错）
   - 创作中心 `x/vupre/web/archive/view` → `code 0`，`state -30 审核中`，有标题和 `videos[0].cid`
   - 用这个 cid 调用 `episodes/add` → `code 0`，分区里能查到该稿件
3. 真实录制端到端（修复后的二进制，本地 dev + 本地 sqlite）：
   - 新增本地测试主播：挂仅自己可见模板，override 设为
     `{season_section_id: <测试分区>, segment_time: "00:01:30", delay: 30}`；
     录制一个正在开播的直播间约 3.5 分钟后暂停。
   - 日志链：`submit_attempt n_videos=3 trigger="periodic_scan"` →（1 s 后）`已加入合集` →
     `稿件已加入合集`，**第 1 次尝试即成功**，没有重试。
   - B 站侧核对：稿件 `state -1 复核中`、公开 view 仍是 `-404`，3 个 P，已在测试分区里。

## 收尾

- 合并发版后，生产再出现「加入合集失败」或「多次失败已放弃」的日志，就说明还有遗漏。
- 修复前失败的稿件已经过审，用 `POST /bili/seasons/backfill` 补进合集即可。

## 进度

- 已合并（[dplei/biliup#100](https://github.com/dplei/biliup/pull/100)），随 1.3.48 发版。
- 待生产验收（未通过前不归档）：
  - [ ] 1.3.48 上线后，下一次投稿的日志依次出现 `submit_ok_with_aid` → `已加入合集` → `稿件已加入合集`，没有「加入合集失败」。
  - [ ] 修复前失败的稿件经 `POST /bili/seasons/backfill` 补进合集，再跑一次时 `to_add` 为空。

## 附：投稿免「合集更新推送」（`no_disturbance`）

- 不用改代码：在投稿模板的「自定义提交参数」（`extra_fields`）里填 `{"no_disturbance": 1}`，会原样合并进投稿请求。
  它关的是合集订阅者收到的「合集更新」推送，粉丝动态照常出现。
- Web 接口：已实测生效，读回 `data.no_disturbance = 1`。
- App 接口：请求里带上了这个字段，但两次测试都被 21566 频控拒绝，B 站是否认这个字段还没验证。
