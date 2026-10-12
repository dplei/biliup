# 开发工具与验证

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

本地启动、索引校验、受控演练、证据导出、时间戳应急与一致性巡检脚本。

**从这里开始：** 按脚本职责选择已有验证入口；具体使用方式另见 `scripts/README.md`。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `scripts/check_code_index.py` | 联合校验根导航与领域子索引的路径、文档链接、重复条目和悬空关系；回归见 `scripts/test_check_code_index.py`。 | `validate_index`、`main` |
| `scripts/structured_logging/check_diagnostic_classification.py` | 校验 P3/14 的机器分类目录：四个 Rust 运行时源码根里每个含 tracing 级别宏的文件必须恰好有一个默认处置，新增/删除文件会阻断；只做文件级漂移检测，不替代调用点语义复核。 | `main`、`scanned_files` |
| `.scratch/structured-logging/diagnostic-classification-v1.json` | P3/14 未迁移诊断的机器目录：按文件冻结 native/bridge/no-persistence/coverage-gap 默认处置，并另列有理由的明确不支持能力边界。人类可读语义与阶段结论见同目录 `diagnostic-classification.md`。 | `version`、`groups`、`explicitly_unsupported_boundaries` |
| `scripts/dev.sh` | 本机开发环境启动脚本：按需构建前端产物与后端二进制，可选带起 Next.js 热重载，绑 127.0.0.1 起服务。 | — |
| `scripts/timestamp_shift.py` | 算出把回退的时间戳「平移」接回去所需的 setts 参数并打印现成的 ffmpeg 命令：逐流找回退点、按典型帧间隔补偏移、多次回退累加。服务端已改成增量模型（`timestamp_repair.rs` 的 `delta_setts`）后两者效果相当，本脚本留给本机应急与服务端修不好的存量片；只算回退，前跳形态尚未覆盖。`segment-recover` skill 用它。 | `find_drops`、`expression`、`typical_delta` |
| `scripts/normalization-disk-sample.py` | 采样响度标准化中间件的数量与字节峰值并判定是否超过上限；只读，直播中可跑。 | `scan`、`Peaks` |
| `scripts/structured_logging/evidence.py` | 有界只读双源证据导出与确定性校验：固定高水位分批、原生/桥接分列、旧文件代次与字节边界、批内一致匿名映射，manifest 记录不完整原因。 | `export`、`validate`、`Bundle`、`Budget`、`readonly` |
| `scripts/structured_logging/recording_pilot.py` | P3/12 的受控录制演练：本地回环发合成 FLV（含人工注入的 DTS 倒退与中途截断），跑真实 Python 下载入口，核对身份链与分段/断连事件，并生成证据包请求与预期事实清单。 | `inject_dts_backward`、`serve`、`check`、`expectations` |
| `scripts/structured_logging/reconcile.py` | 按证据包生成互不可见的双源分析视图、校验报告引用合法性，并单列桥接文本传输结论（永不为原生覆盖计分）。 | `prepare`、`cross`、`check_report`、`bridge_transport` |
| `scripts/structured_logging/upload_entries.py` | 独立上传入口的无账号演练：缺凭据的命令/配置/追加与 Python 调用、三态、重复调用、关闭回退及双源证据导出，不冒充成功远端运行。 | `main`、`run`、`embedded` |
| `scripts/structured_logging/page_upload_entries.py` | 页面上传的隔离 Rust/wheel HTTP 演练：缺失/损坏凭据、并发请求 task 查询、采集三态与关闭回退，导出双源和分离视图，不执行远端上传。 | `main`、`run`、`call` |
| `scripts/structured_logging/hls_entries.py` | HLS 三下载入口受控演练：CLI 提取输出 fixture、真实回环 TS、主列表/序列缺口/不连续/坏列表/HTTP 错误，核对输出字节、task/DA/S、三态与回退并导出双源。不代表真实平台提取或服务整链验收。 | `main`、`run`、`embedded`、`verify_native` |
| `scripts/structured_logging/smoke_entries.py` | 手工触发的入口冒烟：5 个支持入口 × 采集关闭/开启/新库不可用三态加一次关闭回退，全程合成媒体与空业务库，不做账号或网络动作。 | `main`、`child` |
| `scripts/consistency-audit.sh` | 只读巡检：比对每个已投稿会话的 `videos_json` 与本地分段账本，按投稿意图是否为空切成 legacy/current 两组，找出重复进稿、上传未进稿和序号不连续三类错位。 | — |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `scripts/structured_logging/reconcile.py` → `scripts/structured_logging/evidence.py`（`validate`）：组视图与交叉包前先做确定性校验，校验失败的包不进入比较。
- `scripts/structured_logging/check_diagnostic_classification.py` → `.scratch/structured-logging/diagnostic-classification-v1.json`：扫描实际 tracing 文件并与机器目录做集合等价校验；人类可读理由与阶段门槛见同目录 `diagnostic-classification.md`。
