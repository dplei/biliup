# 08 — 主播页背景字段

**What to build:** 主播编辑页面也能配置和上传背景图，并明确提示这个设置会覆盖所属模板的背景。

**如果 05 被砍掉，本票随之消失。**

**Blocked by:** 05（需要主播级背景字段存在）、06（复用其上传接口与表单控件模式）

**Status:** ready-for-agent

- [x] 主播编辑表单新增背景图字段与上传控件
- [x] 字段文案说明该设置会覆盖所属上传模板的背景
- [x] 复用 06 的上传接口，不新增第二个上传接口
- [x] 预览按钮传入主播级的背景值（复用 07 的接口）
- [x] 清空该字段后回退到模板的背景，与 05 的解析逻辑一致
- [x] 该字段清空时必须**提交上来一个空字符串**，不能整项缺失（见下方约束）
- [x] 把 `cover_background` 加进 `OverrideModal` 的 `baseValues` 白名单，否则该字段在主播页保存时不参与提交
- [x] 沿用现有表单组件库，不引入新的 UI 依赖

## 落点

- 字段落在**「配置覆写」弹窗**（`OverrideModal`）而非「录播管理」弹窗：语义正好是
  「主播级覆盖模板级」，与该弹窗的定位一致，也是 spec 点名 `baseValues` 的那一个。
- 共享组件 `app/ui/CoverBackgroundField.tsx`：输入框 + 上传按钮。模板页与主播页要的是
  同一套控件、同一个上传接口，只有文案和排版不同；各写一份的话，哪天上传接口或体积
  上限变了，改漏一处就成了两种行为。`TemplateFields` 一并改用它（净减 40 余行）。
- `CoverPreviewButton` 加 `emptyTemplateHint`：主播页的文字模板来自**所属上传模板**，
  照搬模板页那句提示会把用户支使到错误的地方去改。

## 实施中发现并修复的既有缺陷（06 遗留，不在本票范围）

**模板级背景图从来没能保存成功。** `upload-manager/add` 与 `edit` 两个页面的提交载荷是
**显式白名单**，06 加了 `TemplateFields` 里的输入框和上传控件，却没往这两个白名单里加
`cover_background`——用户填好文件名、点保存，值在提交时被静默丢弃，从未写进库。
06 的 AC 只覆盖到「接口返回文件名，供表单保存进背景字段」（填进表单就算达成），
没有一条盯住「保存之后真的落库」，于是漏了。两处各补一行。

## 三处比 spec 更进一步的地方

1. **`entityFields` 也必须加 `cover_background`**，spec 只提了 `baseValues`。它是
   livestreamers 表上的真实列，不列进 `entityFields` 的话，`handleOk` 那圈循环会把它
   一并塞进 `override` JSON，库里于是同时存在「列上的值」和「override 里的值」，
   而投稿只认前者——一个查起来会很费劲的分裂。
2. **封面区放在 `Collapse` 之外。** Semi 的折叠面板未展开时 children 不挂载，字段也就
   不进 `values`，提交时「用户没展开过」与「用户主动清空」看起来一模一样——而这两者
   一个要保持原值、一个要清空，判错方向就是把用户配好的背景悄悄抹掉。
   放在外面后字段常驻挂载，`values.cover_background ?? ''` 才是可靠的。
3. **区分「模板列表加载中」与「真的没绑模板」。** 两种情况下 `boundTemplate` 都是
   `undefined`，不区分的话，弹窗刚打开就点预览会收到一句错误的「该主播还没有绑定投稿模板」。

## 清空语义（与 05 的守卫如何配合）

Semi 默认 `allowEmpty=false`，用户清空输入框后该键从 `values` 里消失。这里没有走 spec
建议的 `allowEmpty` 路线，而是在拼载荷时 `values.cover_background ?? ''`——更直接，
且不依赖 Semi 某个具体属性名的行为。效果一致：提交显式空串 → 服务端守卫
（`payload.cover_background.is_none()`）不触发 → 空串入库 → 解析侧 trim 后视为未配置
→ 回退到模板的背景。

## 来自 05 的约束（务必先读）

`put_streamers_endpoint` 有一条守卫：**载荷里 `cover_background` 整项缺失就沿用库里的值**。
它存在的理由是本票落地前旧前端会把该字段清成 NULL（`OverrideModal` 用显式白名单拼载荷，
字段不在白名单里就等于缺项）。

副作用是：**清空必须是一个显式的值**。Semi 的 Form 默认 `allowEmpty: false`，空字符串会被
当成 undefined、键不进 `values`；`OverrideModal` 还额外 `filter(value !== undefined)`。
照常写的话，用户点清空提交的是缺项 → 被守卫还原 → 字段永远清不掉。

二选一：给该字段设 `allowEmpty`（空串会被解析函数当作未配置，语义正好对，端点不用动，
**推荐**），或者显式发 `null` 并同时改端点判定（目前 serde 把缺项与显式 null 都读成 `None`，
要区分得上 `Option<Option<String>>`）。
