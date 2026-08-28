<div align="center">
  <img src="https://docs.biliup.rs/home.png" alt="biliup" width="220" height="220"/>

  <h3>biliup · dplei fork</h3>
  <p>面向 7×24 无人值守录播 → 自动投稿的增强分支</p>

[![License](https://img.shields.io/github/license/biliup/biliup)](./LICENSE)
[![Upstream](https://img.shields.io/badge/upstream-biliup%2Fbiliup-blue)](https://github.com/biliup/biliup)
[![Stars](https://img.shields.io/github/stars/dplei/biliup?label=Stars)](https://github.com/dplei/biliup/stargazers)

</div>

> [!NOTE]
> **这是 [biliup/biliup](https://github.com/biliup/biliup) 的个人分支。**
> `master` 跟随上游，日常开发与部署都在 **`dev`** 分支上，两边已经分道发展。
> 本分支的所有改动围绕一个目标：**开一场直播，从检测开播到稿件出现在个人空间，中间不需要人。**
> 代码开放，欢迎自取自用；不提供技术支持，也不保证与上游后续版本兼容。

> [!IMPORTANT]
> **免责声明 / Disclaimer**
> 本项目仅供个人学习研究，不保证稳定性，不提供技术支持；使用产生的一切后果由用户自行承担；禁止商业用途，请遵守版权及平台规定。
> For personal learning and research only. No stability guarantee or support. Users bear all consequences. Commercial use prohibited. Please respect copyright and platform ToS.

---

## ✨ 本分支特有功能

上游提供的是「录下来、传上去」；本分支补的是**中间那段会出事的部分**——断流、掉线、限流、传一半进程挂了、传完了没提交、提交了封面是黑的。

### 📤 投稿：一场直播 = 一个稿件

| 能力 | 说明 |
|---|---|
| **会话化增量投稿** | 录制期每个分段传完即落库（`upload_session` 账本），下播后一次性提交成稿，避免分段各自建稿 / 反复触发重新审核。 |
| **严格完整性闸门** | 账本判定「本场分段全部成功」才允许投稿，不完整的会话绝不提交半成品。 |
| **持久投稿意图 + 协调器** | 下播、分段成功、人工触发、启动/周期扫描都只唤醒同一个协调器；意图落库，进程中途退出重启后会自己把稿件补提交出去。 |
| **重启续接** | 进程重启后按平台场次键找回同场未完结的会话续用同一 `aid`，不会给一场直播造出两个稿件（`recovery_window_minutes` 兜底）。 |
| **自动合集** | 投稿成功后自动把稿件加入该主播的专属合集（`season_section_id`，可按主播 override）。 |
| **缺少投稿模板不录制** | 主播未绑定投稿模板时直接跳过录制并在列表打「缺少投稿」标签，不再录一堆传不出去的文件。 |

### 🛟 上传可靠性与补传

| 能力 | 说明 |
|---|---|
| **缺失分段补传页** | 独立的补传控制页：待投稿会话五态、分段 attempt 阶段/进度/线路健康/完整性、线路历史，支持恢复、换线重投、停止、删除、本场补扫。 |
| **attempt 租约 + watchdog** | 上传尝试分阶段计时并持有租约，卡死的尝试被后台收割前先真正取消进程内任务，杜绝幽灵上传。 |
| **上传线路健康与熔断** | 按线路统计失败、持久化冷却状态，自动线路探测会把冷却中的线路排除在外；成功/失败反向更新，重启不丢。 |
| **全局 601 限流闸门** | 进程级 pre-upload 节流 + B 站 601 指数退避冷却（`upload_rate_gate_enabled` 等），多房间同时下播不再互相打死。 |
| **历史会话回填** | 把旧版本留下的 `videos_json` / 遗留 missing 行回填成 v2 生命周期账本，老数据也能走新的完整性闸门。 |
| **上传 trace 链路** | 补传去向打通会话 `aid`，页面上可以直接点到 B 站稿件。 |

### 🎬 录制韧性

| 能力 | 说明 |
|---|---|
| **抖音断流 failover** | 候选线路模型 + 自动切线（`douyin_route_failover`），可选同画质备用协议（FLV→HLS）与受控降画质（`douyin_quality_fallback` / `douyin_min_fallback_quality`）。 |
| **画质降级告警** | 实际录到的画质低于阈值（`douyin_quality_alert`）时 webhook 推送，列表卡片上带录制画质 tag。 |
| **拉流线路健康退避** | 独立的线路失败计数与指数退避（`route_health_enabled`）。 |
| **短分段止损保留** | 断流产生的小分段不再按体积一刀切丢掉，通过媒体探测则保留并按 `merge_or_defer` 合并/延迟恢复。 |
| **流中断防分稿件** | `delay` 宽限期内的短暂断流视作同一场，不会把一场直播切成多个稿件。 |
| **录制期限（租约）** | 给房间设一个到期时间，到点自动暂停录制并通知；到期时仍在直播的场次走可证明匹配的 grace 收敛，不会拦腰砍断。 |
| **主动检查直播流** | 直播管理卡片上一键立即检查，与轮询共用同一原子入口，不会把同一场拉起两次录制。 |

### 🖼️ 自动封面

| 能力 | 说明 |
|---|---|
| **文字模板封面** | `cover_template` 与 `cover_path` 对称：按模板渲染居中文字封面，无需每场手动做图。 |
| **图片背景** | 支持上传背景图（模板级 / 主播级覆盖），文字叠在背景上。 |
| **网页预览** | WebUI 里直接预览生成效果，不用传上去才知道长什么样。 |
| **本地调参子命令 + skill** | `cover-preview` 子命令渲染样图，配套 [`cover-background`](.claude/skills/cover-background/SKILL.md) skill 帮忙把原图调成压暗/模糊合适的底图。 |

### 🎚️ 上传前的音视频处理

| 能力 | 说明 |
|---|---|
| **时间戳异常检测与修复** | 每个分段上传前扫描时间戳，异常则自动 remux / 重编码（`timestamp_repair`，默认开），避免 B 站转码因时间戳跳变失败；正常片零额外写盘。 |
| **响度标准化** | 上传前统一录音响度（`audio_normalization_enabled`，默认关），只重编码主音轨不动视频，网页推子可在 -6..+4 dB 间微调目标。 |
| **分 P 标题取原始录像名** | 中间件处理过的文件不会把中间件名泄漏成分 P 标题。 |

### 🔭 运维与观测

| 能力 | 说明 |
|---|---|
| **Cookie 健康监测** | 连续检查失败判定平台 cookie 可能失效，网页横幅提示 + webhook 推送（`cookie_health_webhook`，兼容 Bark / Server酱 / 企业微信 / 钉钉 / 自建）。 |
| **实时日志** | `ds_update.log` 按天滚动保留 7 天，WebUI 通过 WS 读取最新滚动文件。 |
| **删除时机开关** | `segment_delete_mode`：`per_segment` 每片传完即删（磁盘峰值≈单个切片，适合小盘机器）/ `stream_end` 下播后统一删。 |
| **静态文件接口收口** | 修掉了上游静态文件接口的任意文件读取问题。 |

> 完整的版本演进、每次发版改了什么、踩过哪些坑，见 [`BUILD_AND_DEPLOY.md`](./BUILD_AND_DEPLOY.md) 的镜像版本历史。

---

## 🚀 快速开始

### 直接安装（Linux / macOS）

```shell
uv tool install biliup   # 安装上游版本
biliup server --auth     # 访问 http://your-ip:19159
```

> ⚠️ PyPI 上的 `biliup` 是**上游版本**，不含本分支功能。要用本分支需要自行构建。

### 使用本分支

```shell
git clone -b dev https://github.com/dplei/biliup.git
cd biliup
npm i && npm run build          # 前端产物
cargo build --release --bin biliup
./target/release/biliup server --auth
```

Docker 部署（交叉构建 amd64 → 镜像仓库 → 服务器）的完整流程见 [`BUILD_AND_DEPLOY.md`](./BUILD_AND_DEPLOY.md)。

### Windows / Termux

- Windows：下载上游 Release [bbup-app](https://github.com/biliup/biliup/releases/latest)
- Termux：见上游 [Wiki](https://github.com/biliup/biliup/wiki/Termux-%E4%B8%AD%E4%BD%BF%E7%94%A8-biliup)

<details>
<summary>命令行参数</summary>

```shell
Usage: biliup [OPTIONS] <COMMAND>

Commands:
  login     登录B站并保存登录信息
  renew     手动验证并刷新登录信息
  upload    上传视频
  append    是否要对某稿件追加视频
  show      打印视频详情
  comments  查看视频评论
  reply     回复视频评论，默认只打印将要回复的内容
  dump-flv  输出flv元数据
  download  下载视频
  server    启动web服务，默认端口19159
  list      列出所有已上传的视频

Options:
  -p, --proxy <PROXY>              配置代理
  -u, --user-cookie <USER_COOKIE>  登录信息文件 [default: cookies.json]
      --rust-log <RUST_LOG>        [default: tower_http=debug,info]
```

`biliup server` 支持 `-b/--bind`（默认 `0.0.0.0`）、`-p/--port`（默认 `19159`）、`--auth` 开启密码认证、`-c/--config` 用 1.0.7 风格配置文件启动。

支持短信登录、账号密码登录、扫码登录、浏览器登录、网页 Cookie 登录，cookie 与 token 保存在 `cookies.json`，可供其他项目复用。

</details>

---

## 🧑‍💻 开发

<details>
<summary>架构与本地开发</summary>

Rust 后端 + 精简 Python 包 + Next.js 前端的混合架构：

- `crates/biliup` — 核心库：直播解析 / 下载 / 上传
- `crates/biliup-cli` — CLI 与 Web 服务：REST API、WebUI、录制调度、投稿会话
- `crates/danmaku` — 弹幕录制：多平台协议 / XML 输出
- `crates/stream-gears` — PyO3 绑定，`python -m biliup` 入口
- `app/` — Next.js + Semi UI 前端
- 数据层：SQLite（配置、任务状态、上传会话账本、日志）

**前端**：Node ≥ 18，`npm i` → `npm run dev` → http://localhost:3000
**Rust CLI**：`npm run build` → `cargo build --release --bin biliup`
**Python**：`maturin dev` → `npm run build` → `python3 -m biliup`
**一键起本机 dev 环境**：`scripts/dev.sh`

代码导航先查 [`CODE_INDEX.md`](./CODE_INDEX.md)。

</details>

---

## 🤝 Credits

- 上游项目 [biliup/biliup](https://github.com/biliup/biliup) — 本分支的全部基础
- Thanks `ykdl, youtube-dl, streamlink` provides downloader
- Thanks `THMonster/danmaku`

---

## 💴 捐赠

### ☕ 支持本分支

<table>
<tr>
<td width="200" align="center">
  <a href="https://suibianwanwan.fun/donate"><img src=".github/resource/donate-dplei.png" width="170" alt="扫码打开捐赠页" /></a>
</td>
<td>

本分支的断流韧性、补传、投稿一致性，都是踩着一场场真实事故改出来的。如果它帮你少丢了几个稿件，欢迎请我喝杯咖啡。

[![爱发电](https://img.shields.io/badge/%E7%88%B1%E5%8F%91%E7%94%B5-%E5%BE%AE%E4%BF%A1%20%7C%20%E6%94%AF%E4%BB%98%E5%AE%9D-946CE6?style=for-the-badge)](https://afdian.com/a/feedmycode)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-Worldwide-FF5E5B?style=for-the-badge&logo=kofi&logoColor=white)](https://ko-fi.com/dplay0216)

扫码或访问 **[suibianwanwan.fun/donate](https://suibianwanwan.fun/donate)**

</td>
</tr>
</table>

<details>
<summary>💧 支持上游项目 biliup</summary>

<img src=".github/resource/Image.jpg" width="150" />

[爱发电 »](https://afdian.com/a/biliup)

</details>

---

## ⭐ Stars

[![Star History Chart](https://api.star-history.com/svg?repos=biliup/biliup&type=Date)](https://star-history.com/#biliup/biliup&Date)
