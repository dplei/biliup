# 历史稿件补录进合集

## 背景

账号开通了 B 站「视频合集」。新稿件已有自动入合集：主播 override 填 `season_section_id`，
投稿成功后 `add_archive_to_season`（`crates/biliup-cli/src/server/common/upload.rs`）。
缺的是**在此之前投的历史稿件**，要求一个主播一个合集。

## 方案

不走 Computer Use，走现成 API：

- 历史稿件来源：`upload_session` 表（`live_streamer_id → aid`），按主播精确归属，不靠标题猜。
- 合集来源：每个主播的生效配置（全局配置 + override）里的 `season_section_id`，与自动入合集同一口径。
- cookie：主播挂的投稿模板的 `user_cookie`，与投稿同一口径。
- 已在合集里的跳过：`GET member.bilibili.com/x2/creative/web/season/section?id=<section_id>`
  取 `data.episodes[].aid`。
- 逐个调用现成的 `add_archive_to_season`，单条失败不影响其余，失败原因原样返回。
- 按 `upload_session.created_at` 从旧到新加入，合集里的顺序就是时间顺序。

接口：

- `GET  /bili/seasons/backfill[?id=<主播id>]`：预演，只列每个主播「库里的 aid / 已在合集 / 待加入」，不写 B 站。
- `POST /bili/seasons/backfill[?id=<主播id>]`：真正执行，返回 `added` / `failed[{aid, error}]`。

## 不做

- 不建合集：主播数量少，在创作中心手动建更快。
- 不补 `upload_session` 之前投的稿件：库里没有归属记录。数量少就在创作中心手动加。
- 不做合集内重排序：先补录、再填 override 开启自动入合集，顺序就天然正确。
- 不做前端按钮：一次性操作，浏览器打开 GET 预演、curl 发 POST 即可。

## 已知限制

- 审核中或被打回的稿件加不进去（view 接口取不到 cid），过审后重跑 POST 即可，已加入的会跳过。
- B 站一个稿件只能属于一个合集，已在别的合集里的会出现在 `failed`。

## 验证

- 单测：待加入列表的计算（去重、跳过已在合集、保持时间顺序）。
- dev 环境：GET 预演核对主播与 aid 对得上，再对一个主播 POST，到创作中心确认合集内容与顺序。

## 进度

- 已合并（[dplei/biliup#95](https://github.com/dplei/biliup/pull/95)），随 1.3.46 发版。
- 待验收（未通过前不归档）：
  - [ ] `GET /bili/seasons/backfill` 预演：每个配了 `season_section_id` 的主播都列出，
        `already_in_section` 与创作中心里合集的实际稿件数一致（验证 `season/section` 响应结构）。
  - [ ] 单个主播 `POST ...?id=<主播id>`：`failed` 只有审核中或已属于其他合集的稿件；
        创作中心里合集顺序从旧到新。
  - [ ] 再跑一次同一 POST：`to_add` 为空（幂等）。
