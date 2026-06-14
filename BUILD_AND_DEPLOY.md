# 构建与发布（Build → ACR → 服务器部署）

> 本仓库是 biliup 的 fork，含本地改动。本机（Mac arm64）交叉构建 **linux/amd64** 镜像，
> 推到阿里云 ACR 个人版，阿里云 ECS（amd64）拉取部署。
> 服务器内存小（2 核 2 GiB）编译易 OOM，**绝不在服务器上 build**。

---

## 0. 为什么是这套流程

- 服务器是 **amd64**，开发机是 **arm64** → 必须 `buildx --platform linux/amd64` 交叉构建，否则服务器拉下来跑不了。
- amd64 在 arm Mac 上靠 **QEMU 模拟**编译，且 release profile 开了 `lto=true, codegen-units=1`（见 workspace `Cargo.toml`），编译慢（M2 Air 8 核全满约 **15–20 分钟**），属一次性成本，无法绕过。
- 构建上下文 = 本仓库根目录（含本地改动）。`Dockerfile` 有 `if [ ! -f biliup.spec ]` 守卫：本地存在 `biliup.spec` → 用本地源码而非 clone 上游，**本地改动会进镜像**。

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
SQLX_OFFLINE=true cargo check -p biliup-cli

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

---

## 3. 推送 + 部署

```bash
# (3) 推两个 tag 到 ACR
docker push crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest
docker push crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:<版本tag>

# (4) 验证远端 manifest（可选）
docker buildx imagetools inspect \
  crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest

# (5) 服务器（ECS）拉取并重启
#     ⚠️ 不要让本助手 SSH 进生产机，由用户自己操作
docker compose pull && docker compose up -d
```
> `live-recorder/docker-compose.yml` 已用 `image: <acr>/peari/biliup:latest` + `pull_policy: always`，
> 生产钉版本号更稳，但 `:latest` 也可用。

---

## 4. 踩坑速查

| 现象 | 原因 / 解法 |
|---|---|
| `FROM` 拉基础镜像 **403 Forbidden** | `~/.docker/daemon.json` 配的阿里云个人加速器 `bf6423da.mirror.aliyuncs.com` 2024 起仅限 ECS 内网。用上面的 buildx 代理构建器绕开（docker-container 驱动不读 daemon.json）。 |
| 拉取镜像 **not found**（私有仓库） | 没 login，或该 tag 当时确实没推进 `biliup` 仓库。先 `docker login`，再确认 push 目标正确。 |
| push 落到错仓库（曾误落 `biliupatest`） | 目标仓库须在 ACR 控制台先以「本地仓库」类型建好且状态正常，否则可能落到别处。 |
| maturin `SSL connect error: unexpected eof` / `download of shell-words failed` | Cargo.lock 变动后容器内重新 `cargo fetch`，撞上 Clash 代理瞬时 TLS 断。**非代码问题，重试即可**。 |
| `apt-get` / npm `Unable to connect to host.docker.internal:7890` | 代理瞬时掉线。确认 Clash Allow LAN 开着、容器能连代理后重试。 |
| build 中途 Docker Desktop 自己挂 | `open -a Docker` 重启，build 前先 `docker info` 确认。 |
| 公共镜像源（docker.1ms.run 等）拉用户私有仓库 403 | 公共源对用户私有仓库无权，已弃用，统一走 ACR。 |

---

## 5. 镜像版本历史（最新在前）

| digest 前缀 | tag | 内容 |
|---|---|---|
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
