# 01 · 修正探测体积与显式线路冷却

Status: ready-for-human

## 范围

- `crates/biliup/src/uploader/line.rs`：读取并校验 `probe.post`，替换固定 10 MB。
- `crates/biliup-cli/src/server/common/upload_line_health.rs`：增加 `probe_failure` 分类。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs`：显式线路只绕过探测失败冷却。
- `crates/biliup-cli/src/server/common/upload.rs`：探测失败按新分类入库。

## 验证

- 探测体积纯函数覆盖服务端提示、缺省/非法输入与上限。
- 从真实迁移后的 sqlite 写入探测冷却，确认显式线路不发起 AUTO 网络探测即可被选中。
- 既有 `transport` 冷却测试继续证明真实上传失败仍会回退。

## Comments

- 已实现服务端提示体积、0.1 MB 缺省值与 10 MB 上限；四路并发时默认总请求体小于 0.5 MB。
- 已把探测失败持久化为 `probe_failure`；配置/手动线路可直接选中，普通 `transport` 冷却仍按旧逻辑回退。
- 验证通过：`cargo test -p biliup`（74 passed，1 ignored）与
  `cargo test -p biliup-cli`（全部通过，忽略项均为既有的环境型测试）。
- 待真实中小带宽环境验收 AUTO 选线与显式切线，因此暂不归档。
