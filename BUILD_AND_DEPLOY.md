# 构建与发布（Build → ACR → 服务器部署）

> 本仓库是 biliup 的 fork，含本地改动。本机（Mac arm64）交叉构建 **linux/amd64** 镜像，
> 推到阿里云 ACR 个人版，阿里云 ECS（amd64）拉取部署。
> 服务器内存小（2 核 2 GiB）编译易 OOM，**绝不在服务器上 build**。

---

## 0. 为什么是这套流程

- 服务器是 **amd64**，开发机是 **arm64** → 必须 `buildx --platform linux/amd64` 交叉构建，否则服务器拉下来跑不了。
- amd64 在 arm Mac 上靠 **QEMU 模拟**编译。release profile 现为 `lto="thin", codegen-units=16`（见 workspace `Cargo.toml`，对 I/O 密集型服务运行性能影响可忽略，但显著缩短交叉编译）。**冷构建**（首次 / 缓存失效）M2 Air 约 **15–20 分钟**；**增量构建**（复用缓存，只改了少量 Rust/前端）约 **9–10 分钟**。详见下方「构建缓存机制」。
- 构建上下文 = 本仓库根目录（含本地改动）。`Dockerfile` 有 `if [ ! -f biliup.spec ]` 守卫：本地存在 `biliup.spec` → 用本地源码而非 clone 上游，**本地改动会进镜像**。

### 构建缓存机制（为什么增量只要 ~10 分钟）
两层缓存共同作用，日常发版基本都走增量：
1. **BuildKit cargo cache mount**（`Dockerfile` 编译阶段）：`--mount=type=cache` 挂了 `/usr/local/cargo/registry`、`/usr/local/cargo/git`、`/biliup/target` 三处。依赖 crate 与已编译产物跨构建复用，改几个文件时只重编受影响的 crate，不再全量重编。⚠️ `target/` 是临时挂载**不进镜像层**，产物 wheel 必须拷到 `/wheels` 普通目录，否则下一阶段 `COPY --from` 取不到。
2. **持久化 buildx 构建器 `biliup-builder`**（docker-container 驱动）：cache mount 存活在这个构建器里，**复用构建器**才有缓存命中，别每次重建。构建器健在即可，`docker buildx ls` 看它在。
- 缓存失效（→ 退回冷构建）的常见诱因：改了 `Cargo.toml`/`Cargo.lock`（依赖树变动触发 `cargo fetch`+重编）、Dockerfile 前置层变动、或删了 `biliup-builder`。
- 提速可复用同一构建器，无需清缓存；只有怀疑缓存损坏时才 `docker buildx prune`。

### 运行架构提醒（改 bug 前必看）
- **Rust 是现役路径**：`crates/biliup-cli`、`crates/biliup`、`crates/stream-gears`。
- `biliup/` 这个 Python 目录是遗留，运行时不走它。
- 运行链路：`biliup server` → python wheel → `stream_gears` → biliup-cli `run()`（不是 `main.rs`）。
- 日志初始化在 `crates/stream-gears/src/server.rs`（不是 main.rs）：`ds_update.log` 按天滚动、留 7 天。

---

## 1. 一次性环境准备

| 项 | 值 / 说明 |
|---|---|
| Docker Hub 账号 | `peari`（注意：GitHub 用户是 `dplei`，Docker Hub 是 `peari`，**两者不同**） |
| ACR 实例 | `crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com` |
| ACR 命名空间 / 仓库 | `peari` / `biliup`（私有，杭州，与 ECS 同地域拉取快） |
| 专网端点（免公网流量） | `crpi-yk3f2yyofxzjbjyy-vpc.cn-hangzhou.personal.cr.aliyuncs.com` |
| 代理 | 本机 Clash `127.0.0.1:7890`，**必须开「允许局域网 Allow LAN」** |

### 1.1 登录 ACR（Mac 端 + ECS 端都要）
```bash
# Mac 端不要加 sudo；ECS 上用 root 直接执行
docker login --username=peari crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com
```
> 私有仓库**未登录拉取会返回 not found**（伪装成无权限），不是真的不存在。

### 1.2 创建 buildx 构建器（走本机 Clash 代理）
代理比公共镜像源快百倍：公共源拉 rust 215MB 层要 ~30min（0.09MB/s），走代理 ~6–7MB/s。

macOS 坑：① Clash TUN 抓不到 Docker VM 流量；② 容器里的 `127.0.0.1` 是容器自己。
所以 Clash 要开 **Allow LAN** 监听 `0.0.0.0`，容器经 `host.docker.internal:7890` 才连得上。

```bash
docker buildx create --name biliup-builder --driver docker-container \
  --driver-opt "env.HTTP_PROXY=http://host.docker.internal:7890" \
  --driver-opt "env.HTTPS_PROXY=http://host.docker.internal:7890" --use
docker buildx inspect biliup-builder --bootstrap   # 拉 buildkit 镜像，可能要重试一次
```
> `--driver-opt` 按逗号分隔，`NO_PROXY` 带逗号会报错，直接省略。
> 构建器健在可复用，不用每次重建。

---

## 2. 构建（每次发版）

> 环境前置：Docker Desktop 已开（`docker info` 确认没挂）；Clash 开 Allow LAN；ACR 已 login。

```bash
cd /Users/leii/Code/record/biliup

# (1) 先本地 check，过了再 build，省一次几十分钟的白跑
#     必须 --workspace：镜像里 maturin 编的是 stream-gears，
#     只 check biliup-cli 看不到它（见下方「check 的盲区」）
SQLX_OFFLINE=true cargo check --workspace

# (2) 交叉构建 amd64，--load 先入本地验证
docker buildx build --builder biliup-builder --platform linux/amd64 \
  --build-arg HTTP_PROXY=http://host.docker.internal:7890 \
  --build-arg HTTPS_PROXY=http://host.docker.internal:7890 \
  -t crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest \
  -t crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:<版本tag> \
  --load .
```
- `<版本tag>` 用语义化标签，如 `1.2.1-season`、`1.2.1-segfix`（版本号取自 workspace `Cargo.toml` 的 `version`）。
- **不要把 build 命令接 `| tail`**：tail 的退出码会被当成结果，失败也报 exit 0。

### check 的盲区（曾让一次发版白跑 7 分钟）
`cargo check -p biliup-cli` **检查不到 `stream-gears`**，而镜像里 `maturin build` 编的正是它
（运行链路 `biliup server` → python wheel → `stream_gears` → biliup-cli `run()`）。
两边各有一处对 `Commands` 枚举的穷举 match：`biliup-cli/src/main.rs` 和
`stream-gears/src/server.rs::_main`。**给 CLI 加子命令必须同时改两处**，只改前者的话
`-p biliup-cli` 照样通过，要等到 Docker 构建才报 `E0004: non-exhaustive patterns`。
封面预览子命令就这么漏过一次，从 `c6e69cf` 起每次构建都会炸、直到 `1.2.2-coverbg` 才发现。
→ 所以第 (1) 步固定用 `--workspace`。

---

## 3. 推送 + 部署

```bash
# (3) 推两个 tag 到 ACR
docker push crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest
docker push crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:<版本tag>

# (4) 验证远端 manifest（可选）
docker buildx imagetools inspect \
  crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest
```

### 推完 ACR 就算发完了——ECS 会自己更新

**不需要手动 `docker compose pull`，也不需要提醒仓库主人去执行它。**
ECS 侧已配置定时任务：自动检查 `:latest` 是否有新镜像，并在**空闲时段**（没有正在进行的
录制/上传）自动拉取重启。所以发布流程到第 (4) 步为止；剩下的交给定时任务，不必守着看。

- 助手的职责终点 = 推送成功 + 远端 digest 核对 + 补 §6 版本历史。
- ⚠️ 仍然**不要让本助手 SSH 进生产机**——无论是部署、看日志还是「确认一下有没有起来」。
- 想立刻生效（例如修的是正在犯病的线上问题）时，才由仓库主人自己手动执行：
  ```bash
  docker compose pull && docker compose up -d
  ```
- 因为更新落在空闲时段，**新版本不是推完就立刻上线**；排查线上现象前先确认跑的是哪个 digest，
  别拿旧版本的行为对着新代码看。

> `live-recorder/docker-compose.yml` 已用 `image: <acr>/peari/biliup:latest` + `pull_policy: always`，
> 生产钉版本号更稳，但 `:latest` 也可用（自动更新依赖的正是 `:latest`）。

---

## 4. 踩坑速查

| 现象 | 原因 / 解法 |
|---|---|
| `FROM` 拉基础镜像 **403 Forbidden** | `~/.docker/daemon.json` 配的阿里云个人加速器 `bf6423da.mirror.aliyuncs.com` 2024 起仅限 ECS 内网。用上面的 buildx 代理构建器绕开（docker-container 驱动不读 daemon.json）。 |
| 构建第 2 步 `resolve image config for docker-image://docker.io/docker/dockerfile:1` **DeadlineExceeded** | `# syntax=` 那行的镜像解析走 **dockerd**，不是构建器，而 dockerd 这条出网路径很慢。实测：拉 20KB 的 `hello-world` 要 14s，拉 40MB 的 `docker/dockerfile:1` 要 5 分钟以上，而 resolve 只有 30s deadline。（`daemon.json` 里的 `bf6423da.mirror.aliyuncs.com` 对非 ECS 内网是 **403 快速拒绝**（0.12s），dockerd 立刻 fallback，它只是多一跳、**不是元凶**；慢在 fallback 后经 Docker Desktop 内建代理 `http.docker.internal:3128` 出网。）构建器自身没问题——docker-container 驱动不读 daemon.json，走我们显式配的 Clash 7890，同一个 manifest 3s 到手，所以重启 buildkit 容器无效。解法：先 `docker pull docker/dockerfile:1` 把 syntax 镜像拉进本地 image store（慢但会成功），之后 resolve 命中本地即可继续；镜像留在本地就不会再犯。 |
| 拉取镜像 **not found**（私有仓库） | 没 login，或该 tag 当时确实没推进 `biliup` 仓库。先 `docker login`，再确认 push 目标正确。 |
| push 落到错仓库（曾误落 `biliupatest`） | 目标仓库须在 ACR 控制台先以「本地仓库」类型建好且状态正常，否则可能落到别处。 |
| maturin `SSL connect error: unexpected eof` / `download of shell-words failed` | Cargo.lock 变动后容器内重新 `cargo fetch`，撞上 Clash 代理瞬时 TLS 断。**非代码问题，重试即可**。 |
| `apt-get` / npm `Unable to connect to host.docker.internal:7890` | 代理瞬时掉线。确认 Clash Allow LAN 开着、容器能连代理后重试。 |
| build 中途 Docker Desktop 自己挂 | `open -a Docker` 重启，build 前先 `docker info` 确认。 |
| 公共镜像源（docker.1ms.run 等）拉用户私有仓库 403 | 公共源对用户私有仓库无权，已弃用，统一走 ACR。 |
| 构建到 `maturin build` 才报 `E0004: non-exhaustive patterns: Commands::X not covered` | 给 CLI 加了子命令但只改了 `biliup-cli/src/main.rs`，漏了 `stream-gears/src/server.rs::_main` 里的同一个 match。本地 `-p biliup-cli` 查不出来，见 §2「check 的盲区」。补上分支，并用 `cargo check --workspace` 复验。 |
| `pip3 install` 报 `Could not install packages ... No such file or directory: '/tmp/biliup-<旧版本>.whl\n/tmp/biliup-<新版本>.whl'` | `workspace.package.version` 真正变了号（此前每次发版 Cargo.toml 版本号都停在原地，只换 docker tag，wheel 文件名从未变过、被同名覆盖）。`target/wheels/` 在 wheel-builder 阶段是持久 cache mount，旧版本号的 wheel 文件不会被自动清理，新旧两个 `.whl` 一起被 `cp target/wheels/*.whl /wheels/` 拷进 `/tmp`，`ls /tmp/biliup*.whl` 匹配到两个文件把 `$whl` 撑成两行。已在 `Dockerfile` 的 wheel-builder RUN 里 `maturin build` 前加 `rm -rf target/wheels`，每次构建先清空该目录（`1.3.0-uploadreliability` 起）。 |

---

## 5. 抖音断流韧性灰度

阶段 5 的备用网络出口暂不实施。本轮只发布短分段止损、线路健康退避和抖音真实候选切换。

### 安全默认值

- `preserve_recoverable_short_segments`：默认关闭，与网页开关显示一致；灰度主播显式开启。
- `route_health_enabled`：默认关闭，与网页开关显示一致；开启后启用线路计数和有界退避。
- `douyin_route_failover`：**默认关闭**，避免发布后一次性影响全部主播。
- `douyin_protocol_fallback`：默认开启；只有同时开启 `douyin_route_failover` 才会切到 HLS。
- `douyin_quality_fallback`：默认关闭，不允许无感知降画质。

先在单个测试主播的「配置覆写」中设置：

```yaml
douyin_route_failover: true
douyin_protocol_fallback: true
douyin_quality_fallback: false
```

至少观察 7 天或 20 场抖音直播，确认没有错误下播、重复投稿、不可播放分 P 或磁盘异常后，
才能扩大到全部抖音主播；自动降画质仍需单独评估和显式开启。

### 观测口径

每场结束会写一条 `event=download_resilience_session_summary`，包含连接失败数、自动切线及成功数、
FLV→HLS 次数和持续时间、全线路熔断退避次数、估算缺失时间、有效/可恢复/合并/无效分段数与
进入上传队列的数量。每个 RouteKey 另写一条 `event=download_resilience_route_summary`，包含：

- `host` / `protocol` / `quality` / `codec`（签名 query 不记录）；
- `attempts` / `failures` / `failure_rate`；
- `stable_attempts` / `average_connected_ms`。

上传成功率继续按同场的上传/投稿日志统计；`segments_queued_for_upload` 只表示已交给上传队列，
不能当作上传成功数。灰度期间重点比较 FLV→HLS 前后的持续时间、短分段保留数量和同期上传错误。

回滚时在空间或主播覆写中设置 `douyin_route_failover: false`；已落盘分段仍照常上传。

---

## 6. 镜像版本历史（最新在前）

| digest 前缀 | tag | 内容 |
|---|---|---|
| `6187c6a5` | `1.3.0-streamcheck` | **直播管理主动检查直播流 + 录制期限备注改选填**：①新增 `POST /v1/streamers/{id}/check` 与卡片上的「立即检查直播流」按钮（编辑之后、暂停之前）。轮询是所有房间排队、每轮睡一个 `event_loop_interval`，服务意外重启后要绕完一整圈才轮得到某个房间，已经在播的场次白白少录一段；按钮把那一次检查提前。轮询循环里的单房间检查抽成 `Monitor::check_room_once`，轮询与按钮共用同一份实现，会话复用（`live_session_key`）、租约准入、下载池许可一个都不绕过——这些判断一旦分裂成两份，两条路径迟早会对同一场直播给出不同结论。②并发安全：摘队列由 `RoomsActor` 单线程原子完成（`TakeForCheck`），「房间不在任何队列里」等价于轮询此刻正拿着它检查，直接回 Busy 而不是开第二次检查，否则同一场直播会被拉起两次录制；已在录制回 `already_recording`、暂停回 `paused`。未拉起录制的分支照旧 `wake_waker` 放回队列，顺带还能把已退出的平台轮询任务重新拉起。③接口语义：正常状态 200 + `outcome`（started/offline/already_recording/checking/paused/no_upload_template/download_pool_full/lease_rejected）+ 中文 message；检查直播间出错回 502（脱敏原因）、已开播但建会话失败回 500、房间不存在回 404，不用 200 把失败包装成正常结果。④录制期限的「客户/需求备注」由必填改选填：校验只保留 200 字上限，留空存空串（不改列约束），请求字段加 `serde(default)`，到期通知文案在空备注时显示「（未填写）」，避免推送出一行光秃秃的「客户/需求：」。⑤修按钮首版的 `Cannot read properties of undefined (reading 'addNotice')`——Semi 的 `Notification.success` 被摘成变量再调用会丢 `this`，接口其实已经 200，炸在提示那一行且顺带吞掉了列表刷新；改回分支直调，并把 `mutate('/v1/streamers')` 提到提示之前。无迁移。验证：`SQLX_OFFLINE=true cargo check --workspace` 通过，`cargo test -p biliup-cli --lib` 253 passed（含 3 条新增用例覆盖摘队列：Ready→Busy→放回后再 Ready、暂停拒绝、未知房间），`tsc --noEmit` / `next build` 通过；本机 dev 环境实测按钮 `POST .../check → 200` 并弹出「主播当前未开播」、期限备注留空保存与延期均 200（验证租约已清除还原）。构建冒烟：容器 `uname -m`=`x86_64`、`biliup --version`=`biliup-cli 1.3.0`、ffmpeg 含 `loudnorm`；推送 `latest` + `1.3.0-streamcheck`，远端 digest=`sha256:6187c6a5f2ccc06c15d07aef9425c0b1cfa78f482124ab4d354c5b0a3ef665bc`。⚠️本次构建三次倒在 syntax 镜像解析上，见 §4 新增的那条。 |
| `a8c20f0a` | `1.3.0-uploadincident2` | **补传租约误杀、接口同步阻塞、线路选择分裂与会话续接断裂（round2 九张票）+ 3 个随手修复**：①**attempt 分阶段租约**：`upload_missing_segment` 加 `attempt_phase`/`phase_started_at`/`last_heartbeat_at`/`line_source`/`last_chunk_*`（迁移 `16`），attempt 拆成 preprocessing / queued / transferring 三段各自计时——预处理配额按体积算（10min + 10min/GiB），排队用 2 小时，只有真正开始推网络字节后才启用 5 分钟无进度判据。根因是旧代码在 `claim_enrolled_attempt` 就把 `last_progress_at` 盖成当前时间，于是一个正在跑 ffmpeg 响度标准化的健康 attempt 被「5 分钟无网络进度」收割成幽灵上传。跨进程僵尸改由 15 秒心跳识别（3 分钟过期），且 `StaleReason::HeartbeatLost` 明确 **不** 归咎线路——上一轮正是 watchdog 超时被算到 `bda2` 头上冷却 1 小时才雪崩。②**补传接口异步化**：`recover`/`retry` 改为同步 claim + 后台执行并立刻 202 返回，反代读超时不再 drop 掉上传和 watchdog 把行留在 `uploading`；新增 `stop` 与 `attempts` 端点。③前端统一错误映射（400/403/404/409/429/5xx）与详情截断。④**线路决策收口**：录制期、整场上传、静默补传、手动补传共用 `decide_upload_line`，候选序列固定 `configured -> bda2 -> tx -> auto`，`bldsa` 不再作隐式候选，回退原因随决策落日志并回传页面。⑤`upload_attempt` 追加式历史表（迁移 `16`）+ 补传页展开行，看得到每次 attempt 用哪条线、到哪个阶段、为何结束。⑥`start_due_recovery_scan` 每 60 秒主动扫到期行（此前只有新分段到达才会触发，重启后已到期的分段要等下次开播）。⑦**场次标识**：`streamerinfo`/`upload_session` 加 `live_session_key`（迁移 `17`），抖音取 `room.id_str`、B 站取 `profile.room_id`；`monitor` 复用既有 `streamer_info` 而不是每次检测到开播就插新行——旧行为让「录制中重启」必然换掉 `ctx.id()`，一场直播被劈成两个会话两个稿件。⑧补传页缺失控件补齐（线路下拉、停止、阶段感知进度）。⑨会话续接改为 精确 id → `live_session_key` → 时钟窗口 三级判定。**随手修复**：⑩**分P标题泄漏中间件名**——开启响度标准化后稿件每个分P标题都成了 `xxx.audio-normalized-<hash>.part`，因为 `Parcel::upload_with_observer` 在 B 站不回标题时用**上传文件**词干兜底，而喂进去的是 loudnorm 产物；改由 `upload_single_file_with_repair` 用原始录像名覆盖（标题此刻只在本地 `videos_json`，投稿要等下播，无需事后编辑稿件）。⑪移除 `app/(app)/layout.tsx` 里重复的 `<html>`/`<body>`（与 RootLayout 各渲染一遍，每个页面都报 React hydration 失败）。⑫本机 dev 环境：`scripts/dev.sh` 一键起后端+前端热重载，`.cargo/config.toml` 的 `PYO3_PYTHON` 指向 Homebrew framework Python （macOS 上 pyo3 默认解析到 Xcode CLT 的 3.9，不给测试二进制留 LC_RPATH，`cargo test -p stream-gears` 启动前就被 dyld SIGABRT，workspace 全量测试从来没跑完过），该目录已加进 `.dockerignore` 免得进镜像误导 Linux 构建。⚠️**含两个迁移**：`16_add_attempt_phase_and_history.sql`、`17_add_live_session_key.sql`。验证：`SQLX_OFFLINE=true cargo check --workspace` 通过，`cargo test --workspace` 首次在本机完整跑完（biliup-cli lib 241 + 集成测试、biliup、danmaku 均 0 failed）；**并在本地 dev 环境跑了一场 75 分钟真实抖音直播端到端验证**：6 段 15 分钟分段全部上传成功并一次性投出稿件 `BV1Fchc6nEfk`（`is_only_self=1`，B 站侧已确认），真实观测到 `live_session_key=7678521965569379112` 落库、`the transfer watchdog stays paused`、重启后 `续接同一场直播（复用 streamer_info）` 且 `streamerinfo`/`upload_session` 均未分裂、重启制造的跨进程僵尸租约在 3 分 11 秒被判 `HeartbeatLost` 收割为 failed 且 `upload_line_health` 纹丝不动、新代码产出的分P标题干净（`本地验证-抖音2026-08-27T10_54_58`）。构建冒烟：容器 `uname -m`=`x86_64`、`biliup --version`=`biliup-cli 1.3.0`、`ffmpeg` 含 `loudnorm`；推送 `latest` + `1.3.0-uploadincident2`，远端 digest=`sha256:a8c20f0a6244e72afff62b83d87846b5ac960a19b18299ce1bdde9e73befffeb`。 |
| `961cea2f` | `1.3.0-uploadreliability` | **上传分段可靠性修复（任务 01–07，`workspace.package.version` 首次实际提升到 1.3.0）**：`validated media segment` 与数据库之间改为同一 durable 事务边界，不再经内存 channel 丢失身份；投稿前统一检查 `pending/uploading/failed`，卡住的 `uploading` 纳入自动恢复查询；新增无进度（5 分钟）和总时长（2 小时）watchdog，配合可撤销的 attempt lease 让手动重试可靠取消旧请求；bldsa 等线路的 TLS 证书失败改为持久熔断 + 24 小时冷却，按 `bda2 -> tx -> auto` 顺序换线；上传成功不再删除 missing 行，源分段永久保留幂等身份；补扫和未绑定恢复统一经 `check_recovery_eligibility` 判定，尊重 finalized 边界不再产生新 session；上传进度落库改为节流写入（5 秒或 16 MiB）。任务 07 用 Rust 实现历史 session 到 v2 账本的 backfill（复用 `upload_lifecycle_backfill` 断点日志表，无法读取的历史分段快照直接拒绝，不用错误 identity 静默合并），并产出会话 #227 的只读审计清单（未自动改动生产稿件）。任务 08 复查时顺带修了两处真实回归：`crates/stream-gears/src/server.rs` 未处理新增的 `Commands::BackfillLifecycle`（`cargo check -p biliup-cli` 看不到，Docker 构建才会炸）、`upload_missing_segment.last_error` 落库/页面展示未脱敏（统一复用 `sanitize_error`）；并补齐 enrollment 成功路径、attempt_token 生命周期、watchdog 触发、每线路失败分类此前完全空白的结构化日志。⚠️Dockerfile 修复：`workspace.package.version` 真正变化后，wheel-builder 阶段的 cache mount 目录 `target/wheels/` 里残留的旧版本 wheel 与新版本 wheel 一起被拷进 `/tmp`，`ls /tmp/biliup*.whl` 匹配两个文件炸掉 `pip3 install`（此前每次发版版本号都停在原地，从未触发）；已在 `maturin build` 前加 `rm -rf target/wheels` 清空该目录。灰度（08 号任务第 5 节）按仓库主人要求本轮跳过，直接发生产，出问题后续读日志排查。验证：`SQLX_OFFLINE=true cargo check --workspace` 通过；`cargo test -p biliup` 45/45、`cargo test -p biliup-cli`（lib 213 + 4 个集成测试文件共 44 项）、`cargo test -p danmaku` 38/38 均 0 failed；buildx `linux/amd64` 冷构建成功（首次因上述 wheel bug 失败，修复后第二次通过）；容器内 `biliup --version` 输出 `biliup-cli 1.3.0`、`uname -m` 为 `x86_64`，冒烟通过；推送 `latest` + `1.3.0-uploadreliability`，远端 digest=`sha256:961cea2fc68a2c6b542b1b756f62b11c1bf3f1a65f7bae149f293ffa6755f7e0`。 |
| `dd60dfe2` | `1.2.2-uploadconcurrent` | **多主播上传并发调度 + 有效分段 durable enrollment + 本场补扫恢复**：修复单个长直播的 `SegmentEvent` receiver 独占上传 Actor，导致同时间其他主播虽已输出 `validated media segment`，却始终没有 `Starting process with upload`、`upload_session` 或 `upload_missing_segment` 的问题。每场直播上传管道改为独立 task，实际 B 站网络上传仍由全局 semaphore 单并发；网络初始化前即创建/续接本地 `upload_session`。每个有效分段在上传前先以 pending 行绑定 session，上传成功时 `videos_json` 更新与 pending 删除在同一 SQLite 事务提交；segment processor、初始化、上传或持久化失败均保留本地文件并使其在「缺失补传」可见。新增 `POST /v1/uploads/missing/rescan` 与页面「补扫本场」入口，按 `streamer_info_id`、本场时间和主播文件名前缀扫描本地媒体，真实容器探测后只把 Valid 文件按顺序加入当前 session；13-byte 空 FLV 明确跳过。验证：上传测试 20/20、missing queue 测试 20/20、新增 SQLite+13-byte FLV 补扫测试通过，`SQLX_OFFLINE=true cargo check --workspace` 通过；Docker 内 Next.js production build 通过（仅既有 `<img>`/Semi CSS 警告），buildx `linux/amd64` release wheel 与镜像构建成功，本地架构和 `biliup --version` 冒烟通过；推送 `latest` + `1.2.2-uploadconcurrent`，远端 digest=`sha256:dd60dfe2f3330145b60d98c154119692288f552fcf3b32ccacaef9772a0135be`。 |
| `c4535cdd` | `1.2.2-shortstorm` | **抖音短分段上传风暴止损**：新增进程级 `pre_upload` 速率门与可持久化的 B 站 601 指数冷却，冷却后只放行单探针，网页、自动上传和 `stream-gears` 生产入口统一受控；新增迁移 `9_add_upload_rate_gate.sql`。短分段改为“兼容分组后分层 concat/remux，失败则整批 Deferred”，不再回退为逐片上传；恢复批次写 SQLite 与 durable manifest，上传队列有界并记录峰值，新增 `/v1/health/upload-rate`、`/v1/recovery-batches` 与迁移 `10_add_recoverable_short_batch.sql`。同时修正 FLV 时长按媒体时间跨度计算、限制 ffmpeg 诊断长度、细分 cookie 错误并脱敏 URL/token。验证：`cargo fmt --all -- --check`、`SQLX_OFFLINE=true cargo check --workspace`、biliup 44 项、biliup-cli 164 项及 29 项集成测试、danmaku 38 项均通过；macOS 的 `stream-gears` 测试二进制因本机缺少链接所需的 Python 3.9 framework 在测试启动前被 dyld 中止，但 Docker 内 `linux/amd64` release wheel 已成功编译该 crate。前端 `tsc --noEmit` / `next lint` / `next build` 通过（仅既有 `<img>` lint 提示）；容器架构与 `biliup --version` 冒烟通过。推送 `latest` + `1.2.2-shortstorm`，远端 digest=`sha256:c4535cdd137fd23647c788423a292c01259962f1ce55348e2de482f6706f7a2b`。 |
| `0321700e` | `1.2.2-dyresilience-defaultfix` | 修复抖音韧性开关“界面关闭、后端实际开启”的状态错位：`preserve_recoverable_short_segments`、`route_health_enabled` 从三态 `Option<bool>` 收敛为默认 `false` 的 `bool`，旧配置缺字段时 API、网页 Switch 和下载运行态现在统一为关闭；显式保存 `true` 才启用，主播覆写缺字段仍继承全局、显式 `false` 可关闭。同步网页说明、示例配置、灰度计划与部署文档。验证：`cargo fmt --all -- --check`、相关开关/覆写 5 项测试、`SQLX_OFFLINE=true cargo check --workspace`、`npx tsc --noEmit` 通过；Docker buildx `linux/amd64` 构建及容器 `biliup --version` 冒烟通过；推送 `latest` + `1.2.2-dyresilience-defaultfix`，远端 digest=`sha256:0321700e90d69cf10f2aa124dcd835f51668d0ad5603cc96077d8164696d289d`。 |
| `0e7c2431` | `1.2.2-dyresilience` | **抖音下载韧性阶段 0–4、6（阶段 5 备用网络出口按要求跳过）**：可恢复短分段保留/合并并进入上传队列；RouteKey 线路健康计数、熔断冷却和取消感知退避；按真实候选依次尝试同画质备用 FLV host、HLS，并可显式开启降画质；401/403 先刷新、重复失败再计数，404 仅在复查仍直播时计数；切线创建新下载器/新文件，避免跨协议或编码混写；每场及每线路输出结构化成功率、持续时间、切线、缺失时间和分段指标。阶段 6 采用安全灰度默认：`douyin_route_failover=false`、协议回退开关预置开启但仅随 failover 生效、画质回退默认关闭。验证：`cargo fmt --all -- --check`、`SQLX_OFFLINE=true cargo check --workspace`、biliup 41 项测试、biliup-cli 155 项及 29 项集成测试、前端 `tsc --noEmit` / `next lint` / `next build` 均通过（仅既有 `<img>` lint 提示）；Docker buildx `linux/amd64` 构建，容器 `biliup --version` 冒烟通过；推送 `latest` + `1.2.2-dyresilience`，远端 digest=`sha256:0e7c2431ab5cec541b101d8aede31583620b545310ac8770144e4fd763ce0c5c`。 |
| `ee05ab8c` | `1.2.2-coverbg` | **封面背景图特性（8 票全量）+ 1 个安全修复 + 4 个 bug 修复**，自 `1.2.2-histfill` 起累积 13 个提交。①🔒**静态文件接口收口**：`/static/{path}` 此前把用户路径直接交给 `ServeFile`，等同任意文件读取；统一收到 `path_safety::resolve_within`（拒 `..`/绝对路径/越界符号链接，根取工作目录——不能收窄到图片目录，否则破坏日志下载与录播回放，含 Range 请求）。②**封面支持图片背景**：`Background` 枚举加图片/内存位图变体，按 `object-fit: cover` 语义等比缩放+居中裁剪；坏图/缺图/极端宽高比（1×3000 需 11.8 GB，设 6400 万像素上限）一律回退纯黑而不向上抛错。③**三级背景解析**：主播 → 模板 → 纯黑，任一级值不可用就往下走；库里只存单段文件名，带目录/`..`/绝对路径按未配置处理。④**网页上传背景图** `POST /v1/cover-backgrounds`（multipart，校验顺序：路径 → 单段+扩展名 → 文件头，10 MB 上限，落 `data/cover-backgrounds`）。⑤**网页封面预览** `GET /v1/cover-preview?template=&background=`，不读库、复用投稿时同一个 `render_cover`，直接返回 JPG；渲染丢 `spawn_blocking` 免得占住 worker 线程。⑥**主播页背景字段**（「配置覆写」弹窗），覆盖所属模板的设置。⑦**本地调参子命令** `biliup cover-preview`（`--dim`/`--blur`/`--background-only`，参数只烘焙进输出图、不进库不进服务端）+ 配套 skill。⑧修 `Recorder::format` 遇非法时间格式串 **panic**（chrono 的 `to_string()` 对 `Item::Error` 直接炸，模板改由用户当场输入后成了可打崩请求的输入）→ 新增 `try_format` 先验后格式化，投稿路径退化为「占位符原样保留」。⑨修**模板级背景保存不上**：`upload-manager` 的 add/edit 页载荷是显式白名单，06 加了输入框却没加 `cover_background`，填好点保存值被静默丢弃、从未落库。⑩修**扫码登录 dev 下必崩**：`abort(reason)` 带 reason 时 fetch reject 成该 reason 本身，cleanup 传的是字符串 → `e.message` 为 `undefined` → `url.startsWith` 抛（StrictMode 双次 effect 才触发，生产不复现）。⑪修**镜像构建失败**：`Commands` 枚举有两处穷举 match，票 01 只改了 `main.rs`、漏了 `stream-gears/src/server.rs::_main`，而生产链路恰恰走后者；`cargo check -p biliup-cli` 看不到该 crate，故从 `c6e69cf` 起每次构建都会炸、本次发版才发现（文档 §2 的 check 已改为 `--workspace`）。⚠️**含两个迁移**：`7_add_cover_background.sql`、`8_add_streamer_cover_background.sql`（均为加列）。验证：`cargo check --workspace` 通过；`cargo test -p biliup-cli` 130+9+11+9 全过（新增封面预览 11 条、背景上传 9 条集成测试）；前端 `tsc --noEmit` / `next lint` / `next build` 全过；首次构建因 ⑪ 失败（~7min），修后增量构建成功，推送 `latest`+`1.2.2-coverbg`，远端 digest=`sha256:ee05ab8c0f97a391c460c654539e07eb93ed9d9c95a5d543623e4756d7f3823a`。 |
| `1bef1de1` | `1.2.2-histfill` | 修两处上传遗漏：①**手动缺失补传后不删本地文件**：`manual_recover_missing_segment` 补传成功后只删了时间戳修复临时文件、从不删原始录像（与自动路径 `recover_due_missing_segments` 不一致，磁盘堆积）。修复：补传成功并入稿/入会话后，载入主播 `LiveStreamer.postprocessor` 跑 `process_video` 清理本地文件（`Unfixable` 保留待手动处理），对齐自动路径。②**投稿管理历史文件上传丢主播信息**：`post_uploads` 硬编码伪造占位 `StreamerInfo`（name=模板名、url="stream_title"、title=空），标题/简介模板 `{streamer}`/`{title}`/`{url}` 退化为通用模板。修复：新增纯函数 `filename_stem` + `match_streamer_by_filename`（按 `files[0]` 的 basename 词干在 `filelist` 反查 `streamer_info_id`），`post_uploads` 载入真实 `StreamerInfo` 填模板、未命中占位兜底，返回 `{matched, streamer_name}`，前端 Semi `Toast`/`Notification` 提示匹配到的主播。前提：下载器用 **stream-gears**（文件名带扩展名可匹配；ffmpeg 去扩展名不在范围）。含 `filename_stem`/`match_streamer_by_filename` 单测；`SQLX_OFFLINE=true cargo check -p biliup-cli` 通过；buildx `linux/amd64` 增量构建 ~10.4min（缓存命中），推送 `latest`+`1.2.2-histfill`，远端 digest=`sha256:1bef1de1707c348efb55f6f653534f03f3359369330bf4bbaf354edd6de98ff1`。 |
| `e038a020` | `1.2.2-overridefix` | 修复录播管理「配置覆写」保存后清空投稿模板/既有录播字段：覆写弹窗提交时保留当前主播的 `upload_streamers_id`、`filename_prefix`、`time_range`、处理器等持久字段，只把下载/平台设置写入 `override`，并修正前端类型字段 `filename_prefix`。验证：`SQLX_OFFLINE=true cargo check -p biliup-cli` 通过；Docker buildx `linux/amd64` 增量构建并推送 `latest` + `1.2.2-overridefix`，远端 digest=`sha256:e038a02026518bcd77fdcf149516f0db7dc323a85316a7129478e357a449f78b`。 |
| `a7af970d` | `1.2.2-probeclass` | 上传自动选线探测加并发和 maxtime：列表探测 5s、单线探测 4s、总探测 10s、并发 4，避免预先 ping 长时间拖累整体上传进度；下载侧先做原因分类，不改变重连/切片策略，区分 `IncompleteFrame`/`ReadTimeout`/`HttpStatus`/通用错误，便于后续按真实原因优化。验证：`SQLX_OFFLINE=true cargo check -p biliup-cli` 通过；Docker buildx `linux/amd64` 构建并推送 `latest` + `1.2.2-probeclass`，远端 digest=`sha256:a7af970d304949269e3cdbdbcd96b6462b8e2e4aa68d7607c8f9db4cedd3c699`。 |
| `03a508f7` | `1.2.2-missingctl` | 缺失补传删除/重试控制 + 严格选路 + 小分段诊断：①状态助手 `can_delete_missing_segment`(仅 pending/failed)、`reset_for_manual_retry`(uploading→failed 置 next_retry_at=now)、幂等清理 `remove_missing_segment_files`(视频+弹幕，缺文件不报错)。②`DELETE /v1/uploads/missing/{id}` 删缺失记录：先 claim 置 deleting 原子占用再删本地文件与 DB 行，清理失败回写 last_error。③`POST /v1/uploads/missing/{id}/retry` 重试卡住的 uploading 行：先 reset 为 failed 再复用 manual_recover，绕开原子 claim 的 no-op。④严格自动选路：`Probe::probe` 逐条记录每条线路探测，全部失败即报 "no upload line probe succeeded"，不再 `unwrap_or_default()` 静默退回默认线（纯函数 `choose_fastest_successful_line` 含单测，替换 server/CLI 共 4 处静默回退）。⑤前端 missing 页 uploading 行「重新补投」、pending/failed 行「补传+删除」。⑥小分段诊断：httpflv chunk 读取拆四分支(正常/EOF/读错/超时)分别告警、stream_gears 记录流 host、download check_stream 记录 check_elapsed。cargo test missing_segment 16 passed + choose_fastest_successful_line 2 passed。⚠️前端在本机 pnpm 半安装(blockExoticSubdeps 拦 mpegts.js git 子依赖)未跑 build，待部署机验证。 |
| `5195fd39` | `1.2.2-loginretry-wsalive` | 上传登录瞬时失败容错 + WS 日志保活：①`upload.rs` 把 `login_by_cookies` 抽成 `login_with_retry`（4 次退避 3s→6s→12s），避免一次出口网络抖动就 `?` 返回、drop 整场 rx 报销整场录像；失败打全 `{:?}` 错误链、分段失败日志 `upload.rs:1089` 由 `{}` 改 `{:?}`（此前底层原因被 `AppError::Unknown`+Display 吞成 "Unknown Error"，实为 token 校验请求瞬时网络失败而非 cookie 过期）。②`ws.rs` 网页实时日志安静期被前置代理(NPM/nginx ~60s idle)掐断报 "Connection reset without closing handshake"，加每 30s 主动 Ping 保活 + 该良性断连从 `error!` 降 `debug!`。⚠️push 注意 zsh 坑：`$ACR:latest` 的 `:l` 会被当小写修饰符推到 `biliupatest` 错仓库，必须 `"${ACR}:latest"` 加花括号。 |
| `34320574` | `1.2.2-tracedest` | 缺失补传「去向」打通会话 aid：`get_missing_uploads` JOIN `upload_session` 返回 `session_aid`/`session_bvid`/`session_status`；前端去向列番号优先取 missing 行自身 aid，回退到所属会话的 aid/bvid，已投出渲染可点「已投稿 av{aid}」直达 B 站，未投出显示「待提交（会话 #id，尚未投稿）」。解决方案B 下 `missing.aid` 恒空、去向只显示会话号无法定位稿件的问题（`missing.aid` 投稿后不回填，真正番号在 upload_session 上）。 |
| `37ec562c` | `1.2.2-uploadtrace` | 录播上传 trace 链路与可观测性：以 `upload_session.id` 作 per-session trace_id，`tracing` span 让上传→投稿→拿 aid 全链路日志带 `session=<id>`；`upload_session` 加 `submit_attempts`/`last_submit_at`/`last_submit_error`/`submit_state`(ok_with_aid/ok_no_aid/failed，迁移 `6_add_session_submit_trace.sql`) 持久化投稿结果，捕捉「投稿成功却无 aid」「写回失败」等静默异常；`submit_session` 三分支写状态+显式日志，手动补传补 `manual_recover_to_session`/`manual_recover_edit_archive` 日志；「缺失补传」页加 `?status=active/succeeded/all` 筛选 + 去向列(av链接/会话#id)+完成时间列。本期只记录异常，`ok_no_aid` 保持会话 uploading、不改投稿/防重复决策。纯函数 `submit_state_label`/`missing_status_where` 含单测，cargo test 69 passed |
| `fb4f6843` | `1.2.2-dyquality` | 抖音画质降级告警 + 录制画质 tag：抖音实际录到的画质低于可配阈值（默认蓝光 uhd）时复用 `cookie_health_webhook` 推送（每场开播一次，断流复查不补推、录制结束清空，文案用录播备注 remark）；直播管理页录制中房间旁显示「<画质> 录制中」tag；新增 `douyin_quality_alert` 阈值配置（空间配置全局 + 直播管理单房间，含「关闭通知」，缺省=蓝光，机制同 douyin_quality）；缺省逻辑收敛为 `DEFAULT_QUALITY_ALERT`/`effective_quality_alert`。链路：douyin 选流写 `LiveStream.recording_quality` → `start_download_workflow` 判定推送 + 写 `Worker.recording_quality` → `/v1/streamers` 返回 → 前端 tag。⚠️本镜像起 release profile 改 thin LTO + cargo cache mount（见 §2），增量构建 ~30min→~9min |
| `58c19e5d` | `1.2.2-tsrepair` | 上传前时间戳异常检测/修复：分段上传前用 ffmpeg 全片扫(`-c copy -f null`)检测时间戳跳变(典型 FLV 32 位回绕，B 站转码失败根因)，异常则 `-c copy` 重封装→重编码逐级修复；覆盖主链路+两条补传路径(`upload_single_file_with_repair`)；全局开关 `timestamp_repair` 默认开，正常片零额外写盘；极罕见不可修片保留本地+webhook 告警。⚠️检测 stderr 模式未经真实 ffmpeg 跑验，部署后留意首批转码结果 |
| `f9554efd` | `1.2.2-templatefix` | ①文件名模板支持冒号 token（`%H:%M:%S` 等时间格式不再被错误转义）；②封面渲染改进：实际嵌入 `NotoEmoji-Regular.ttf`(OFL) 做逐字字体回退混排，思源黑体缺字形改用 emoji 字体渲染白色单色轮廓 |
| `579c535f` | `1.2.1-coverfix` | 修封面文字模板两个 bug：①字面 `\n` 不换行——`split_template_lines()` 先把 `\n` 还原为真实换行再切分（前端单行输入框存的是字面 `\n`，旧代码 `split('\n')` 切的是真实换行符，切不开）；②emoji 显示成豆腐块——内嵌 `NotoEmoji-Regular.ttf`(OFL) 做逐字字体回退混排，思源黑体缺的字形改用 emoji 字体。⚠️ `ab_glyph` 只能画单色轮廓，emoji 是白色单色剪影非彩色 |
| `2d04cd4a` | `1.2.1-submitonce` | **修迁移崩溃重发**：`a1f279ea` 误把已应用的 migration 4 改了字节(校验和不符)导致 sqlx VersionMismatch 启动 panic、19159 拒连。已用生产 DB 副本核对 `_sqlx_migrations` v4 校验和=`2352d0f3…`，还原 migration 4 至部署字节(`4f0e51a`)，并对生产 DB 副本本地跑通迁移启动验证。功能同下。**已应用的迁移文件绝不可改字节(含注释)，改动须新建迁移** |
| ~~`a1f279ea`~~ 作废 | ~~`1.2.1-submitonce`~~ | 方案B：录制期每段上传即落 `upload_session`(uploading)并删本地，**下播一次性提交**(整场只审一次，消除「过审后追加→重新审核」)；重启续接窗口内会话、开播补提交废弃会话；未绑投稿则不录制并显示「缺少投稿」标签；修 `cover_template` 不落库。⚠️改了已应用迁移导致启动崩溃，被 `2d04cd4a` 取代 |
| `071f0ada` | `1.2.1-incrsubmit` | 增量投稿：每段上传即建稿/edit 追加并落 `upload_session` 表，崩溃/重启按 room+30min 窗口续接同一稿件，每段成功后删本地（防丢已传内容） |
| `788e7ff1` | `1.2.1-autocover` | 自动封面：上传模板 `cover_template` 填写则生成「黑底+主播名+直播时间」封面（优先于 cover_path），内嵌思源黑体 |
| `33f1dd17` | `1.2.1-season` | 投稿后自动加主播专属合集 + ds_update.log 按天滚动留 7 天 |
| `6b1510f8` | `1.2.1-dingtalk-filelog` | web 实时日志写 ds_update.log + cookie 推送适配钉钉/企业微信 |
| `3a924769` | `1.2.1-cookiehealth-retry` | cookie 健康监测 + delay 宽限期防分稿件 |
| `a3f6584a` | `1.2.1-segfix` | 逐片删「开播锁死」+ 前端 initValue 修复 |
| `d1ba7f7d` | `1.2.1` | 转载来源自动填 + 删除时机下拉框 |

发版后把新行补到表头即可。
