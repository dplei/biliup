# 显式上传线路改为按 upcdn key 构造

issue：[#67](https://github.com/dplei/biliup/issues/67)。根因、方案与验收标准以 issue 为准，这里只放拆解与进度。

## 事实核对（2026-09-20，已确认）

- `crates/biliup/src/uploader/line.rs` 15 条 `query` 全为 `zone=cs&upcdn=<key>&probe_version=20221109`，仅 key 不同。
- `probe_url` 全仓只在 `Probe::probe_line`（AUTO）读取；显式路径 `resolve_planned_line` → `pre_upload` 只用 `query`。
- 当前索引 `preupload?r=probe`：`estx`、`akbd`、`bldsa`、`bda2`、`tx`。旧表中 `bda` 不可达，`anitama.*` 五条 301。

## 拆解

| 步 | 文件 | 状态 |
|---|---|---|
| [01](steps/01-line-explicit.md) | `Line::explicit`，删 15 个构造函数，`explicit_upload_line` 改校验+构造 | 完成 |
| [02](steps/02-drop-enums.md) | 删 `biliup-cli` / `stream-gears` 的 `UploadLine` 枚举与 match，CLI `--line` 改自由字串 | 完成 |
| [03](steps/03-lines-endpoint.md) | `GET /v1/upload-lines` + 两处前端下拉改为动态渲染 | 完成 |
| [04](steps/04-real-upload.md) | 本地 dev 真实录制上传验收（issue 清单「真实上传」段），全过才建 PR | 完成 |

## 不做

- 不写同步源码表的脚本：表被删掉后没有可同步的对象。
- 不动 `IMPLICIT_FALLBACKS` 与按线路取回能力（`upos_recovery_round_trip_by_line`）。
- 不迁移旧配置里的 key：索引已下线的 key 由 preupload 阶段自然失败进冷却。
