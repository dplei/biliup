# 05 — 主播级背景覆盖

**What to build:** 单个主播可以配置自己的背景图，盖掉所属上传模板的设置。共用同一个模板的其他主播不受影响。

**这是 spec 里标记待定的一票。** 如果实际使用中就是一个主播对应一个模板，这一级是纯冗余，整票砍掉即可——砍掉后 08 也随之消失，其余各票不受影响。

**Blocked by:** 03（需要模板级背景与解析函数先存在）

**Status:** ready-for-agent

- [x] 主播表新增一个可空的背景字段，语义与模板级字段一致（存文件名、NULL 与空白等价于未填写）
- [x] 背景解析函数扩展为三级回退：主播 → 模板 → 纯黑
- [x] 未配置主播级背景时，行为与只有模板级时完全一致
- [x] 解析函数单测：两级都为空 → 纯黑
- [x] 解析函数单测：仅模板有值 → 用模板的
- [x] 解析函数单测：仅主播有值 → 用主播的
- [x] 解析函数单测：两级都有值 → 用主播的
- [x] 解析函数单测：主播级填空白字符串时视作未配置，回退到模板（已知限制：因此无法表达「强制回退到纯黑」，见 spec 的 Further Notes）

## 落点

- 迁移：`8_add_streamer_cover_background.sql`，单条 `ALTER TABLE livestreamers ADD COLUMN cover_background VARCHAR`。
- 字段：`LiveStreamer.cover_background`。
- 解析：`resolve_background(streamer, template)`，内部抽出 `background_path` 把「一个值 → 可用路径」的判断收在一处，三级回退变成 `主播.or_else(模板).map_or(纯黑)`。
- 接线：`build_studio` 新增一个 `streamer_background` 参数；自动投稿路径经 `submit_session` 从 `ctx.live_streamer()` 取，历史文件上传路径按 URL 反查主播行。

## 四处有意的决定

1. **不可用的值等同于没填，继续往下一级回退**，而不是把链断在原地。主播那级写错一个路径
   （比如粘了绝对路径），不该把模板配好的背景一起废掉。空白、带目录、`..`、绝对路径同此。
2. **主播编辑页会静默清空背景，已在端点堵上。** `put_streamers_endpoint` 收的是
   `LiveStreamer` 本身而非独立的 Insert 结构——票 03 用「字段不进 Insert 结构」躲开的那个
   陷阱，这里躲不掉：字段必须在模型里才读得到。实测 serde 把缺失的 `Option` 读成 `None`
   （有测试锁住，否则编辑页会整个 422）。而 `app/ui/OverrideModal.tsx` 是用**显式白名单**
   （`baseValues` 里逐字段列举）拼 PUT 载荷的，`cover_background` 不在其中，于是旧前端
   每保存一次主播就会把它清成 NULL。端点现在的规则是「载荷里整项缺失就沿用库里的值」，
   查库失败往上抛而不是降级——降级会把一次数据库抖动变成一次配置丢失。
3. **历史文件上传路径也做主播反查**（新增 `get_streamer_by_url`）。AC 没要求，但那条路径
   只反查得到 `StreamerInfo`，不查就等于主播级覆盖在手动上传时悄悄失效，与用户故事 5
   「优先级明确」相抵。查不到或查库失败都退到模板级，不阻断上传。
4. **额外三条数据库测试**：两条迁移往返（与票 03 对称），一条锁住
   `update_all_fields` 确实会把背景写成 NULL —— 那正是决定 2 存在的理由，
   理由本身该有测试盯着，否则日后有人把端点里的补偿逻辑删掉也没人发现。

## 给票 08 的约束（review 挖出来的，别踩）

上面那条守卫的规则是「**缺项 = 不改**」，因此**清空必须是一个显式的值，不能是缺项**。

Semi 的 Form 默认 `allowEmpty: false`，空字符串会被当成 undefined、键根本不进 `values`；
`OverrideModal` 又额外做了一次 `filter(value !== undefined)`。两者叠加，票 08 若照常写，
用户点「清空背景」提交上来的就是缺项 → 被守卫还原 → **这个字段永远清不掉**。

票 08 必须二选一：给该字段设 `allowEmpty`（让空字符串真的提交上来，空串会被解析函数
当作未配置，语义正好对），或者在载荷里显式发 `null`（那就得同时改端点的判定，
把「显式 null」与「缺项」区分开——目前 serde 把两者都读成 `None`，区分不了）。
**推荐前者**，端点不用动。

## 关于「这一票是否该做」

票头写着「如果实际使用中就是一个主播对应一个模板，这一级是纯冗余」。既然要做，代价记在这里：
`resolve_background` 从一参变两参，`build_studio` 与 `submit_session` 各多一个参数，
主播编辑页多一次 SELECT。真要回退，删掉迁移 8 与这些参数即可，模板级不受影响。

## 本阶段怎么验证

表单还没有（票 08），直接写库：

```bash
sqlite3 data/data.sqlite3 "UPDATE livestreamers SET cover_background='nebula.jpg' WHERE remark='某主播';"
```

该主播下一次投稿的封面用 `nebula.jpg`，共用同一模板的其他主播仍用模板那张。
