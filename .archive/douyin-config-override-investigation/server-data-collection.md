# 抖音主播配置覆写诊断数据采集

Status: wontfix（前提不成立，见文末结论）

## 目标

排查以下三个主播级覆写开关在前端显示为开启，但运行结果为 `false` 的原因：

- `route_health_enabled`
- `douyin_route_failover`
- `douyin_protocol_fallback`

需要区分四层状态：

1. SQLite 数据库实际落盘值；
2. `/v1/streamers` API 返回值；
3. 当前 Worker 内存中的主播覆写值；
4. 全局配置与主播覆写合并后的预期有效值。

## 操作限制

- 只执行只读命令；不要修改数据库、配置文件或代码。
- 不调用 PUT、POST、DELETE API。
- 不重启或重新创建容器。
- 不输出 Cookie、Token、Webhook 完整地址、签名参数或直播流完整 URL。
- URL 只保留协议、域名和路径，删除 query。
- 最终严格按照本文末尾的“回传格式”返回一个 Markdown 文档。

## 一、确定目标主播

优先根据用户提供的主播备注或直播间 URL 确定目标主播，记录：

```text
TARGET_STREAMER_ID=
TARGET_STREAMER_REMARK=
```

如果不能确定，列出具有相关覆写字段的候选主播，只返回 `id`、`remark` 和去掉 query 的 URL：

```sql
PRAGMA query_only = ON;

SELECT
    id,
    remark,
    CASE
        WHEN instr(url, '?') > 0 THEN substr(url, 1, instr(url, '?') - 1)
        ELSE url
    END AS sanitized_url
FROM livestreamers
WHERE
    json_extract("override", '$.route_health_enabled') IS NOT NULL
    OR json_extract("override", '$.douyin_route_failover') IS NOT NULL
    OR json_extract("override", '$.douyin_protocol_fallback') IS NOT NULL
ORDER BY id;
```

## 二、确定实际数据库和运行版本

### Docker 部署

收集容器名称、镜像 tag、镜像 ID、RepoDigest、容器创建时间以及数据库的宿主机/容器内路径。不要输出整个环境变量。

```bash
docker ps --no-trunc
docker inspect <容器名> --format '{{json .Mounts}}'
docker inspect <容器名> --format 'image={{.Config.Image}} image_id={{.Image}} created={{.Created}}'
docker image inspect <镜像ID> --format 'id={{.Id}} created={{.Created}} digests={{json .RepoDigests}}'
```

### 非 Docker 部署

收集：

- 进程命令行；
- 可执行文件路径及 SHA-256；
- `biliup --version`；
- 部署仓库当前 commit（如果存在 Git 仓库）。

不要输出进程环境变量。

### SQLite 信息

对实际运行数据库执行：

```bash
sqlite3 <实际数据库路径> 'PRAGMA query_only=ON; SELECT sqlite_version(); PRAGMA journal_mode;'
```

记录数据库路径、mtime、文件大小、SQLite 版本和 `journal_mode`。

## 三、采集数据库落盘值

所有 SQL 都必须先设置：

```sql
PRAGMA query_only = ON;
```

### 3.1 全局配置

只提取指定字段，不返回完整 `configuration.value`，其中可能包含敏感配置：

```sql
SELECT
    id,
    key,
    json_valid(value) AS json_valid,
    json_type(value, '$.route_health_enabled') AS route_health_type,
    json_extract(value, '$.route_health_enabled') AS route_health_enabled,
    json_type(value, '$.douyin_route_failover') AS failover_type,
    json_extract(value, '$.douyin_route_failover') AS douyin_route_failover,
    json_type(value, '$.douyin_protocol_fallback') AS protocol_fallback_type,
    json_extract(value, '$.douyin_protocol_fallback') AS douyin_protocol_fallback
FROM configuration
WHERE key = 'config';
```

检查是否意外存在多条全局配置：

```sql
SELECT key, COUNT(*) AS row_count
FROM configuration
WHERE key = 'config'
GROUP BY key;
```

### 3.2 目标主播覆写

将 `<TARGET_STREAMER_ID>` 替换为目标 ID：

```sql
SELECT
    id,
    remark,
    json_valid("override") AS override_json_valid,
    length("override") AS override_bytes,
    json_type("override", '$.route_health_enabled') AS route_health_type,
    json_extract("override", '$.route_health_enabled') AS route_health_enabled,
    json_type("override", '$.douyin_route_failover') AS failover_type,
    json_extract("override", '$.douyin_route_failover') AS douyin_route_failover,
    json_type("override", '$.douyin_protocol_fallback') AS protocol_fallback_type,
    json_extract("override", '$.douyin_protocol_fallback') AS douyin_protocol_fallback
FROM livestreamers
WHERE id = <TARGET_STREAMER_ID>;
```

### 3.3 检查相近拼写和重复含义字段

不要返回完整覆写 JSON，只列出可能相关的键、JSON 类型和值：

```sql
SELECT
    j.key,
    j.type,
    j.value
FROM livestreamers AS l,
     json_each(CASE
         WHEN json_valid(l."override") THEN l."override"
         ELSE '{}'
     END) AS j
WHERE l.id = <TARGET_STREAMER_ID>
  AND (
      lower(j.key) LIKE '%route%health%'
      OR lower(j.key) LIKE '%route%fail%'
      OR lower(j.key) LIKE 'douyin%fallback%'
      OR lower(j.key) LIKE 'douyin%fallbac%'
      OR lower(j.key) LIKE '%protocol%fallback%'
  )
ORDER BY j.key;
```

特别检查：

```text
douyin_protocol_fallbac
douyin_protocol_fallbak
douyin_route_fallback
route_health_enable
```

明确报告每项的 `json_type`，用于发现字段是否被错误保存成字符串 `"true"`，或者根本不存在。

## 四、采集 API 和 Worker 状态

先确定服务监听地址。只访问 localhost/容器内部服务，不绕过认证，不回传认证头或 Cookie。

### 4.1 `/v1/streamers`

```bash
curl -fsS http://127.0.0.1:<PORT>/v1/streamers |
jq --argjson id <TARGET_STREAMER_ID> '
  .[]
  | select(.id == $id)
  | {
      id,
      remark,
      status,
      override: {
        route_health_enabled: .override.route_health_enabled,
        douyin_route_failover: .override.douyin_route_failover,
        douyin_protocol_fallback: .override.douyin_protocol_fallback
      },
      override_types: {
        route_health_enabled: (.override.route_health_enabled | type),
        douyin_route_failover: (.override.douyin_route_failover | type),
        douyin_protocol_fallback: (.override.douyin_protocol_fallback | type)
      }
    }
'
```

### 4.2 `/v1/status`

此接口可能包含敏感全局配置，禁止回传原始响应，必须直接用 `jq` 过滤：

```bash
curl -fsS http://127.0.0.1:<PORT>/v1/status |
jq --argjson id <TARGET_STREAMER_ID> '
  {
    version,
    global_config: {
      route_health_enabled: .config.route_health_enabled,
      douyin_route_failover: .config.douyin_route_failover,
      douyin_protocol_fallback: .config.douyin_protocol_fallback
    },
    target_room: (
      [
        .rooms[]
        | select(.live_streamer.id == $id)
        | {
            downloader_status,
            uploader_status,
            live_streamer: {
              id: .live_streamer.id,
              remark: .live_streamer.remark,
              override: {
                route_health_enabled: .live_streamer.override.route_health_enabled,
                douyin_route_failover: .live_streamer.override.douyin_route_failover,
                douyin_protocol_fallback: .live_streamer.override.douyin_protocol_fallback
              }
            }
          }
      ] | first
    )
  }
'
```

明确说明：

- `target_room` 是否存在；
- 数据库值与 Worker 内存中的 `live_streamer.override` 是否一致；
- `/v1/status.config` 是全局配置，不是合并后的主播有效配置。

## 五、计算预期有效配置

不要修改程序。根据采集值手工计算：

```text
effective.route_health_enabled =
    主播 override 中存在该字段 ? 主播值 : 全局值

effective.douyin_route_failover =
    主播 override 中存在该字段 ? 主播值 : 全局值（缺失时运行默认 false）

effective.douyin_protocol_fallback =
    主播 override 中存在该字段 ? 主播值 : 全局值（缺失时运行默认 true）

runtime.route_failover_enabled =
    平台是 douyin
    && effective.route_health_enabled
    && effective.douyin_route_failover
```

回传实际结果：

```json
{
  "effective": {
    "route_health_enabled": null,
    "douyin_route_failover": null,
    "douyin_protocol_fallback": null
  },
  "runtime_expected": {
    "route_failover_enabled": null
  }
}
```

将 `null` 替换为实际布尔值。

## 六、采集运行日志

从目标主播最近一次保存配置之后的开播检查开始，采集这些关键词：

```text
douyin stream candidates observed
douyin stream candidate
download_resilience_session_summary
route_failover_enabled
enabled_candidate_count
flv_to_hls_switches
successful_flv_to_hls_switches
```

Docker 示例：

```bash
docker logs --timestamps --since 72h <容器名> 2>&1 |
grep -E 'douyin stream candidates observed|douyin stream candidate|download_resilience_session_summary|route_failover_enabled|enabled_candidate_count|flv_to_hls_switches'
```

要求：

- 保留时间戳；
- 优先截取目标主播最近一场直播对应的完整相关日志；
- 每条日志前后最多附带 3 行上下文；
- 删除 URL query，并遮盖 Cookie、Token、签名和 Webhook 地址；
- 不回传无关主播的大量日志。

重点报告：

```text
candidate_count=
enabled_candidate_count=
route_failover_enabled=
flv_to_hls_switches=
successful_flv_to_hls_switches=
```

解释口径：

- `enabled_candidate_count > 1`：抖音插件生成了可切换候选；
- `candidate_count > 1` 但 `enabled_candidate_count = 1`：响应存在多个候选，但运行配置未启用备用候选；
- `route_failover_enabled=false`：下载重试层总开关未开启；
- `route_failover_enabled=true` 但没有切换：配置生效，但该场可能没有达到连续失败/熔断条件。

## 七、检查更新时间线

收集以下时间，精确到秒：

```text
前端点击“确定”的大致时间（若日志或访问日志可确认）
livestreamers 数据库文件 mtime
当前容器创建时间
当前进程启动时间
目标主播最近一次 Worker 重建/房间更新日志时间
目标主播最近一次开播检查时间
```

用于判断：

- 数据库已更新但 Worker 没有重建；
- Worker 已重建但运行的是旧镜像；
- 前端保存请求未成功；
- 保存后马上开播，但任务使用了保存前的 Worker。

禁止为了采集而重启服务。

## 回传格式

请严格按以下格式返回，不要只给结论。

### 1. 运行环境

```text
部署方式：
容器/进程名称：
biliup 版本：
镜像 tag：
镜像 ID：
RepoDigest：
容器创建时间：
进程启动时间：
数据库路径：
数据库 mtime：
Git commit（如可得）：
```

### 2. 目标主播

```text
id：
remark：
脱敏 URL：
当前 downloader_status：
当前 Worker 是否存在：
```

### 3. 数据库原始提取结果

#### 全局配置

```text
configuration config 行数：
route_health_enabled: value=..., json_type=...
douyin_route_failover: value=..., json_type=...
douyin_protocol_fallback: value=..., json_type=...
```

#### 主播 override

```text
override_json_valid：
override_bytes：
route_health_enabled: value=..., json_type=...
douyin_route_failover: value=..., json_type=...
douyin_protocol_fallback: value=..., json_type=...
```

#### 相近或疑似拼错字段

```text
没有 / 列出 key、type、value
```

### 4. API 与 Worker 状态

#### `/v1/streamers`

```json
{}
```

#### `/v1/status` 过滤结果

```json
{}
```

### 5. 预期合并结果

```json
{
  "effective": {
    "route_health_enabled": true,
    "douyin_route_failover": true,
    "douyin_protocol_fallback": true
  },
  "runtime_expected": {
    "route_failover_enabled": true
  }
}
```

以上只是格式示例，必须替换为实际值。

### 6. 相关日志

```text
粘贴已脱敏的目标主播相关日志
```

汇总：

```text
candidate_count：
enabled_candidate_count：
route_failover_enabled：
flv_to_hls_switches：
successful_flv_to_hls_switches：
```

### 7. 四层对比表

| 字段 | 全局数据库 | 主播数据库 override | `/v1/streamers` | Worker `/v1/status` | 预期有效值 |
|---|---:|---:|---:|---:|---:|
| route_health_enabled |  |  |  |  |  |
| douyin_route_failover |  |  |  |  |  |
| douyin_protocol_fallback |  |  |  |  |  |

### 8. 初步判断

只根据证据选择一个或多个：

- [ ] 前端没有把 `true` 提交到服务端
- [ ] 服务端收到请求但数据库没有正确落盘
- [ ] 数据库正确，但 `/v1/streamers` 序列化错误
- [ ] 数据库正确，但 Worker 持有旧主播配置
- [ ] 三项覆写均正确，问题发生在有效配置合并之后
- [ ] 运行的是不包含当前修复的旧镜像
- [ ] 字段拼写或 JSON 类型错误
- [ ] 日志被误读：显示的是全局值而非主播有效值
- [ ] 其他：请写明

最后给出一句最小结论，但不要修改任何文件或服务。

## Comments

- 用户截图确认主播配置覆写页面中三个开关均已打开。


---

## 结论（2026-08-29）：覆写没有失效，本调查的前提是误判

生产只读采集 + 本地代码核对之后，「三个开关前端显示开启、运行结果为 false」这个前提不成立。
按 issue tracker 的角色定义标 `wontfix`——不是「不修」，是「没有这个 bug」。

### 采集结果

- 全局 `configuration` 只有一行，三个开关都是真 JSON 布尔 `true`。
- 有覆写的主播里，`json_type` 全部返回 `true`（布尔），API `/v1/streamers` 同样返回 `boolean`。
  **怀疑过的「布尔被存成字符串 `"true"`」不存在。**
- 有一个主播的两个开关是显式 JSON `null`（不是缺键），另外所有带覆写的主播都存着两个值为
  `null` 的降级相关键。

### 显式 null 是安全的

`Config` 用 `struct_patch::Patch` 派生 `ConfigPatch`，`Option<T>` 字段的 patch 类型是
`Option<Option<T>>`。JSON `null` 落到它上面时，serde 默认解析为**外层 `None`**，`apply` 时
不覆盖，回退全局值。

只有显式标注了 `deserialize_with = "deserialize_option_patch"` 的字段才会把 `null` 解析成
`Some(None)`（强制置空）——全仓只有 `file_size` 一个字段这么标
（[`config.rs:20`](../../crates/biliup-cli/src/server/config.rs#L20)，函数定义在
[`config.rs:544`](../../crates/biliup-cli/src/server/config.rs#L544)）。
三个抖音开关都没有这个标注，所以显式 `null` 只是回退全局，不会强制关闭。

### 覆写链路是通的

[`live.rs:21`](../../crates/biliup-cli/src/server/core/live.rs#L21) 的 `live_request` 取的是
`worker.get_config()`，而 `get_config()`
（[`context.rs:250`](../../crates/biliup-cli/src/server/infrastructure/context.rs#L250)）
正是「全局配置 clone 后 `apply(override)`」的合并结果，不是 `get_global_config()`。
下载路径读的也是 `ctx.config()`。

### 生产日志反而证明覆写生效了

候选启用条件在 [`douyin.rs:546/571`](../../crates/biliup/src/downloader/live/douyin.rs#L546)：

```rust
if route_failover && quality_fallback   // 才生成低一档画质候选
let protocols = if route_failover && protocol_fallback  // 才保留同画质的 HLS 候选
```

生产日志里 `candidate_count=4 enabled_candidate_count=2`，启用的两个是同画质的 flv 与 hls，
低一档画质的两个未启用。这**恰好**是 `route_failover=true` + `protocol_fallback=true` +
`quality_fallback` 未开（生产覆写里它是 `null`，回退全局默认 `false`）的预期结果。
若 `route_failover` 真是 `false`，候选模型根本不会建立。

### 真正的问题在别处

「开关开了却没看到降级行为」的观感，来自熔断打开后**从不切换到那个已启用的备用候选**，
反复重试同一个 `selected` 候选——那是
[`dplei/biliup#6`](https://github.com/dplei/biliup/issues/6)，与配置覆写无关。
