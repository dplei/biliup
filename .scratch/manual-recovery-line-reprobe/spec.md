# 手动补传二次探测导致手选线路失效

来源：https://github.com/dplei/biliup/issues/101

## 根因

`claim_manual_recovery` 已按页面所选线路决策并 claim；`run_claimed_recovery` 又经
`initialize_upload_context` 按 `config.lines`（AUTO）、`forced=None` 再探测一次。全部线路
处于 probe 冷却时第二次决策零候选报错，手选线路被拖死。第二次的结果原本会被手选线路覆盖。

## 方案

`initialize_upload_context` 增加 `preselected: Option<SelectedLine>`：有值直接用，否则照旧
`decide_upload_line(.., "session_init")`。手动补传传入 claim 时的决策，录制期上传传 `None`。
删除 `run_claimed_recovery` 里事后覆盖 line/key/source 的代码块。

## 不在范围

同机大文件上传时 4 秒探测超时、失败累积冷却导致 AUTO 全线冷却——见 issue 「诱因」一节，
另开 effort。

## 进度

- [x] 01 修复（`steps/01-reuse-claim-decision.md`）——PR #102 已合并，发版 1.3.49，待生产验收
