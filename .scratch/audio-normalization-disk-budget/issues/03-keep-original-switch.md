# 03 — `keep_original` 开关与行为变更说明

Status: implemented / 待验收（见 [`06`](./06-concurrency-acceptance.md)）
Blocked by: 01

## 背景

[`01`](./01-replace-original-in-place.md) 让原片被产物覆盖，两处对外可见行为随之改变：

- 用户自定义 postprocessor（[`upload.rs:2793`](../../../crates/biliup-cli/src/server/common/upload.rs)
  的 `execute_postprocessor`）此后拿到的是标准化后的文件；
- 想留档原始音轨的用户失去了原片。

需要一个逃生门，也需要把变更写清楚。

## 改动范围

### 1. 配置项

[`config.rs`](../../../crates/biliup-cli/src/server/config.rs) 在既有两个字段旁新增：

```yaml
audio_normalization_keep_original: false
```

默认 `false`（即默认原地替换）。理由：`audio_normalization_enabled` 本身默认 `false`，
本变更只影响主动开启标准化的用户；若默认保留原片，整个 effort 等于没做。

`true` 时行为完全退回当前实现——产出独立 `TempArtifact`、上传它、上传后清理，峰值也退回
`2·N·S`。这条路径必须保留可用，不是形式上的开关。

字段由现有 `ConfigPatch` 自动支持主播 override，与另外两个字段一致；首版前端只在全局设置
暴露，不进主播弹窗。

### 2. 文档

- `public/config.yaml`：加字段与注释，注释里点明「关闭后原片会被标准化结果覆盖，
  postprocessor 收到的是标准化文件」。
- ~~`CHANGELOG.md`~~：**不改**。该文件是上游文件，最后一次改动来自上游 commit，本
  fork 从未在其中记录过自己的变更；往里加条目只会在同步 upstream 时制造冲突。行为变更
  改由 `public/config.yaml` 注释与前端开关的说明文案承担——那也是用户实际会看到的两处。
- 前端全局设置的响度区块加一行开关与说明文案。

## 验收标准

1. 配置单测：默认 `false`；YAML 与 JSON 两种来源都能解析；per-streamer override 生效。
2. 单测：`keep_original = true` 时走旧路径——产出临时件、上传临时件、上传后临时件被删、
   原片未被修改。
3. 单测：`keep_original = false` 时走 [`01`](./01-replace-original-in-place.md) 的替换路径。
4. `public/config.yaml` 注释与前端说明文案已写明「原片会被覆盖、postprocessor 收到的是
   标准化文件」。
5. `cargo test -p biliup-cli` 全绿，前端 `npm run build` 通过。

## 风险

两条路径并存意味着 `NormalizationOutcome` 要同时表达「已就地替换」与「产出了临时件」两种
形态。实现时保持一个枚举两个分支，不要让调用方靠配置项自己猜当前是哪种——把形态编码进
返回值，调用方只匹配。

## 实现记录（2026-08-30）

配置项落地，默认 `false`。前端在响度区块内新增「保留原始录像」开关，文案写明原片会被
覆盖、后处理脚本收到的是标准化文件。

CHANGELOG.md 按上文所述未改，理由已写在「改动范围 / 文档」一节。
