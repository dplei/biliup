# 同秒切段文件名撞车：后一段覆盖前一段，短段合并自拼

来源：[dplei/biliup#88](https://github.com/dplei/biliup/issues/88)。

Status: ready-for-human（[#89](https://github.com/dplei/biliup/pull/89) 已合入，1.3.43 发版，待生产验证；清单见 #88）

关联：#75（`.scratch/timestamp-start-skew/`，同为 21588，但本例各流起点对齐，跳变在流内）。

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义。

## 一句话

`LifecycleFile::create` 按模板（精确到秒）命名、不查重，`finalize` 的 `rename` 静默覆盖；
同一秒内切出第二段时第一段被覆盖，两条关闭事件指向同一路径，短段合并把同一个文件与自己
拼接，产物在首段末尾前跳一个「原片起点时间戳」的量，被 B 站 21588 拒稿。

## 机制

1. 开播首个 tag 触发 `Different h264 sequence header` 切段 → 同秒 `create()` 两次，同名。
2. 第一段（725 B 空壳）先 finalize、登记为可恢复短段；第二段 finalize 时覆盖它，再登记一次。
3. `plan_short_segment_flush` 看到两个 pending → `merge_compatible_segments` → concat list 里
   同一路径两行。
4. concat demuxer 以 FLV 末包时间戳作 duration；原片起点约 16s，第二份被放到 26.1s 处，
   产物在约 9.95s 处两路流同时前跳约 16s。本地前跳门槛 30s，未拦下。

复现：留存原片 `-f concat -c copy -avoid_negative_ts make_zero` 自拼，产物字节数与生产一致，
v/a 均在 9.95 → 26.13 前跳。

## 修复

一处：`create()` 撞名（目标或其 `.part` 已存在）时追加 `-1`、`-2`… 序号。所有下载器都经此建
文件，覆盖全部调用方；下游无按文件名解析时间的代码（已检索 `parse_from_str` 等）。

不做：pending 队列按路径去重——路径唯一后不可能重复，属于症状层补丁。

## 不在本 effort

- 前跳门槛 30s 放过 16s 跳变——另议。
- 触发本问题的那一场的补救属于运维处置，不在代码范围。

## 步骤

| # | 步骤 | 状态 |
|---|---|---|
| 01 | [文件名撞车时追加序号](steps/01-unique-segment-filename.md) | 已合入，待生产验证 |
