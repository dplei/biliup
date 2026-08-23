# 上传初始化失败分段持久化修复：服务器部署交接

## 结论

本版本可以进入镜像构建和推送流程。

- 修复提交：`7d6419e4f07da93f0b3bc809973dd80a43798e8e`
- 推荐镜像标签：`1.2.2-uploadinit-recovery`
- 推荐服务器镜像：
  `crpi-yk3f2yyofxzjbjyy-vpc.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:1.2.2-uploadinit-recovery`
- 构建、推送按仓库根目录的 `BUILD_AND_DEPLOY.md` 执行。
- 服务器只拉取和运行镜像，禁止在 2C2G ECS 上编译。

本次没有新增配置字段、环境变量或数据库 migration。容器启动仍会自动运行现有
SQLx migrations，数据库位置仍为 `/opt/data/data.sqlite3`。

## 本版本解决什么

当 B 站登录或上传线路初始化失败时：

1. 已缓冲的有效录像分段会先写入持久化缺失分段队列，并同步登记 `filelist`。
2. 失败的上传 channel 会关闭；下一分段会重建 channel 并重新尝试初始化，而不是整场直播都停止尝试上传。
3. 缺失分段会绑定本地 `upload_session`；异常情况下未绑定的记录也能在手动恢复时创建或复用 session。
4. 超出 30 分钟恢复窗口的空 session 会先补传缺失分段，再决定是否投稿。
5. 跨 channel 重建时继续使用持久化的 `segment_order`，避免恢复后分 P 顺序反转。
6. 初始化失败路径与正常上传路径执行相同的 `segment_processor`。

## 发布前提（构建端）

开始服务器部署前，必须由构建端确认以下两个 tag 已推送到 ACR：

```text
crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:latest
crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:1.2.2-uploadinit-recovery
```

并把以下命令得到的远端 digest 交给服务器执行者：

```bash
docker buildx imagetools inspect \
  crpi-yk3f2yyofxzjbjyy.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:1.2.2-uploadinit-recovery
```

服务器执行者不得在 tag 尚不存在、digest 尚未确认时开始部署。

## 给云端 Codex 的任务说明

目标：在不改变 `/opt` 持久化数据、不重新创建业务配置的前提下，把生产 biliup 更新到
`1.2.2-uploadinit-recovery`，完成验证并保留可回滚路径。

### 1. 只读确认现状

先定位生产 compose 目录和当前容器，不要假定仓库根目录的 `docker-compose.yml` 是生产文件；
仓库自带示例仍指向上游 GHCR，不应覆盖生产配置。

执行并记录：

```bash
docker compose config
docker compose ps
docker inspect biliup --format '{{.Config.Image}} {{.Image}}'
docker inspect biliup --format '{{range .Mounts}}{{println .Source "->" .Destination}}{{end}}'
```

确认存在一个挂载到容器 `/opt` 的持久化 bind mount 或 volume。若无法确认 `/opt` 的实际来源，
停止部署并报告，不要重建容器。

### 2. 核对 compose 镜像配置

生产 compose 的 biliup service 应使用阿里云杭州 VPC 地址和不可变版本 tag：

```yaml
services:
  biliup:
    image: crpi-yk3f2yyofxzjbjyy-vpc.cn-hangzhou.personal.cr.aliyuncs.com/peari/biliup:1.2.2-uploadinit-recovery
    pull_policy: always
```

保留现有的 ports、volumes、command、environment、restart policy 和其他 service 配置。
只允许修改 `image` 和缺失时补充 `pull_policy: always`，不要用仓库示例 compose 覆盖生产文件。

如果服务器尚未登录私有 ACR，执行：

```bash
docker login --username=peari \
  crpi-yk3f2yyofxzjbjyy-vpc.cn-hangzhou.personal.cr.aliyuncs.com
```

密码必须由服务器已有 secret/凭据存储或用户输入提供，不得写入 Markdown、compose、shell history
或 Codex 输出。

### 3. 停机前备份

1. 记录当前运行镜像的 image ID 和 tag，作为回滚依据。
2. 先执行 `docker compose stop biliup`，避免复制正在写入的 SQLite。
3. 根据第 1 步解析出的 `/opt` 实际挂载源，备份其中的 `data/` 目录，至少包含：
   - `data.sqlite3`
   - 若存在：`data.sqlite3-wal`
   - 若存在：`data.sqlite3-shm`
4. 验证备份文件非空并记录绝对路径。

不要删除旧容器、旧镜像、数据卷、录像或 cookie 文件。

### 4. 拉取并启动

```bash
docker compose pull biliup
docker compose up -d biliup
docker compose ps
```

如果 pull 返回 `not found`，优先检查 ACR 登录和 tag 是否已推送，不要回退到公共镜像代理。

### 5. 启动验证

执行：

```bash
docker compose logs --since=10m biliup
docker exec biliup biliup --version
docker inspect biliup --format '{{.Config.Image}} {{.Image}}'
```

检查要求：

- 容器状态为 running，且没有 restart loop。
- 日志包含数据库 migration 启动信息，且没有 migration error。
- 日志没有 SQLite schema、cookie 文件读取、配置反序列化或端口绑定错误。
- 实际 image ID 对应构建端提供的 ACR digest。
- Web 端口能返回 HTTP 响应；启用认证时 `401`/跳转也表示服务已经监听。

可用 Python 标准库只读检查现有恢复表，不要求容器安装 sqlite3 CLI：

```bash
docker exec -i biliup python - <<'PY'
import sqlite3

db = sqlite3.connect('/opt/data/data.sqlite3')
for table in ('filelist', 'upload_session', 'upload_missing_segment'):
    found = db.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
    ).fetchone()
    print(f'{table}:', 'ok' if found else 'missing')
print('active missing segments:', db.execute(
    "SELECT COUNT(*) FROM upload_missing_segment "
    "WHERE status IN ('pending', 'uploading', 'failed')"
).fetchone()[0])
db.close()
PY
```

### 6. 本次不需要的配置

不要为了此修复修改以下内容：

- B 站 cookie 或上传模板；
- `route_health_enabled`、`douyin_route_failover` 等下载韧性灰度开关；
- `recovery_window_minutes`；
- `segment_processor` 或 `postprocessor`；
- 数据库 schema 或 migration 表。

这些配置与“上传初始化失败时先持久化分段”的修复没有启用依赖。若 cookie 本身已失效，仍应按
正常流程重新登录；本修复只保证登录失败期间录像不再静默成为孤儿。

## 历史遗留文件：懒懒椰椰

本版本不会扫描磁盘并自动回填升级前已经产生的孤儿文件。部署后先只读确认：

```bash
docker exec biliup test -f '/opt/懒懒椰椰 2026-08-22 22:42:30.flv'
docker exec biliup ffmpeg -v error \
  -i '/opt/懒懒椰椰 2026-08-22 22:42:30.flv' -f null -
```

再查询它是否已经存在于索引或缺失队列：

```bash
docker exec -i biliup python - <<'PY'
import sqlite3

path = '/opt/懒懒椰椰 2026-08-22 22:42:30.flv'
db = sqlite3.connect('/opt/data/data.sqlite3')
print('filelist:', db.execute(
    'SELECT id, streamer_info_id, file FROM filelist WHERE file=?', (path,)
).fetchall())
print('missing queue:', db.execute(
    'SELECT id, upload_session_id, status, attempts, last_error '
    'FROM upload_missing_segment WHERE file_path=?', (path,)
).fetchall())
print('matching streamer info:', db.execute(
    "SELECT id, name, url, date FROM streamerinfo "
    "WHERE name LIKE '%懒懒椰椰%' ORDER BY date DESC LIMIT 10"
).fetchall())
print('matching rooms:', db.execute(
    "SELECT id, remark, url, upload_streamers_id FROM livestreamers "
    "WHERE remark LIKE '%懒懒椰椰%'"
).fetchall())
db.close()
PY
```

如果文件有效但两个表都没有记录：

1. 保留文件，不运行 postprocessor，不删除。
2. 把上面四项查询结果交回用户确认主播、房间和上传模板映射。
3. 映射确认后，再单独执行一次受控的缺失队列回填或通过历史文件上传功能投稿。
4. 不要在未确认 `live_streamer_id` 与 `streamer_info_id` 前直接写数据库。

这是升级前历史数据修复，不是新版本运行所需配置，可与镜像部署分开处理。

## 回滚

若新容器出现启动失败或持续异常：

1. 保留新版本启动日志。
2. 把 compose 的 image 改回部署前记录的不可变 tag；当前已知上一版为
   `1.2.2-shortstorm`。
3. 执行：

```bash
docker compose pull biliup
docker compose up -d biliup
docker compose ps
```

本次没有新 migration，正常情况下不需要恢复数据库备份。只有确认数据库文件在部署过程中损坏时，
才允许停容器后使用第 3 步备份恢复；不要用回滚镜像作为删除新数据的理由。

## 云端 Codex 完成后应回报

- 部署目录与实际使用的 compose 文件路径；
- 部署前 image ID/tag；
- 新 image ID、tag 与构建端 digest 的比对结果；
- `/opt` 挂载来源和备份绝对路径；
- 容器状态、启动日志摘要、HTTP 探测结果；
- 三张恢复相关表是否存在及 active missing 数量；
- “懒懒椰椰”文件完整性和数据库命中情况；
- 是否执行回滚，以及任何未完成项。
