# 会话 #227 事故恢复只读数据采集

Status: needs-info

## 目标

为会话 #227（`upload_session.id = 227`）的分 P 缺失/重复问题生成只读审计报告，采集以下四类数据：

1. 会话本身：`aid`/`bvid`/`status`/`submit_state`/`submit_claim_token`/`videos_json`。
2. 绑定或同场的所有 `upload_missing_segment` 行：状态、attempts、line_index、last_error、lifecycle_version、normalized_file_path。
3. `22:29:32`、`22:59:56`、`23:30:19` 三个时间点对应的切片文件是否存在、大小、ffprobe 结果。
4. B 站侧该稿件的实际分 P 列表（公开只读接口，用于和本地记录比对）。

本文档只在**生产机（或生产数据库只读副本）**上执行，本地仓库的 `data/data.sqlite3` 不是生产库，
不要用它替代。所有采集结果原样粘贴回来，不要提前下结论、不要脱敏掉本文允许保留的字段。

## 操作限制

- 全程只读：不执行 `UPDATE`/`DELETE`/`INSERT`，不调用 B 站 edit / 分 P 相关写接口。
- 所有 SQL 前必须先 `PRAGMA query_only = ON;`。
- 不要输出 `SESSDATA`、`bili_jct`、`Cookie`、`Authorization` 或任何登录态相关字段/请求头。
- 第六节的 B 站接口是公开只读接口，不需要登录态，不要额外带 Cookie。
- URL 只保留协议、域名、路径和必要的 query（`aid`/`bvid`），不要粘贴其他请求的完整 query。
- 不要重启或重建任何服务、容器进程。

## 一、确定目标数据库

```bash
# Docker 部署：找到容器与数据库挂载路径
docker ps --no-trunc
docker inspect <容器名> --format '{{json .Mounts}}'

# 非 Docker 部署：确认实际运行的数据库文件路径（来自 --database 参数或默认路径）
ps -ef | grep biliup
```

```sql
PRAGMA query_only = ON;
SELECT sqlite_version();
PRAGMA journal_mode;
```

记录数据库路径、mtime、文件大小、`journal_mode`。

## 二、会话 #227 基本信息

```sql
PRAGMA query_only = ON;

SELECT
    id,
    live_streamer_id,
    streamer_info_id,
    aid,
    bvid,
    status,
    submit_attempts,
    submit_state,
    last_submit_at,
    last_submit_error,
    submit_claim_token,
    submit_claimed_at,
    blocked_signature,
    blocked_count,
    created_at,
    updated_at,
    length(videos_json) AS videos_json_bytes,
    json_valid(videos_json) AS videos_json_valid
FROM upload_session
WHERE id = 227;
```

单独取出 `videos_json` 全文（用于比对 order/filename/title，不含敏感信息，可以完整粘贴）：

```sql
PRAGMA query_only = ON;
SELECT videos_json FROM upload_session WHERE id = 227;
```

如果 `videos_json_valid = 0`，額外记录：这是「非空但解析失败」，按 07 号任务文档的结论，
**不能**当作「尚未投稿」处理，必须在报告里原样标注，不要据此建议补传。

关联的主播信息（用于后续按 `live_streamer_id` 找同场 missing 行，URL 去掉 query）：

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
WHERE id = (SELECT live_streamer_id FROM upload_session WHERE id = 227);
```

请把本节查询结果中的 `live_streamer_id`、`streamer_info_id`、`created_at`、`updated_at`
记下来，后面几节会用到。

## 三、绑定或同场的 upload_missing_segment 行

### 3.1 直接绑定到会话 #227 的行

```sql
PRAGMA query_only = ON;

SELECT
    id,
    live_streamer_id,
    streamer_info_id,
    upload_session_id,
    aid,
    segment_order,
    status,
    attempts,
    line_index,
    last_error,
    lifecycle_version,
    normalized_file_path,
    file_path,
    danmaku_file_path,
    total_bytes,
    uploaded_bytes,
    current_line,
    upload_started_at,
    last_progress_at,
    next_retry_at,
    created_at,
    updated_at,
    length(video_json) AS video_json_bytes,
    video_json
FROM upload_missing_segment
WHERE upload_session_id = 227
ORDER BY segment_order, id;
```

### 3.2 同一主播、同一时间窗口但未绑定到 #227 的行（可能是孤儿/未匹配段）

把 `<LIVE_STREAMER_ID>` 替换为二节查出的 `live_streamer_id`，
`<SESSION_CREATED_AT>`/`<SESSION_UPDATED_AT>` 替换为会话的 `created_at`/`updated_at`
（各自前后放宽 2 小时，覆盖下播收尾的时间误差）：

```sql
PRAGMA query_only = ON;

SELECT
    id,
    upload_session_id,
    segment_order,
    status,
    attempts,
    line_index,
    last_error,
    lifecycle_version,
    normalized_file_path,
    file_path,
    created_at,
    updated_at
FROM upload_missing_segment
WHERE live_streamer_id = <LIVE_STREAMER_ID>
  AND (upload_session_id IS NULL OR upload_session_id != 227)
  AND datetime(created_at) BETWEEN datetime(<SESSION_CREATED_AT>, '-2 hours')
                                AND datetime(<SESSION_UPDATED_AT>, '+2 hours')
ORDER BY created_at, id;
```

### 3.3 按文件名时间戳直接查（用于交叉核对 3.1/3.2 是否有遗漏）

录制文件名默认包含 `HH_MM_SS`（分隔符取决于该主播 `filename_prefix` 配置，可能是 `_`、`-`
或 `:` 的转义形式，如实际不匹配请按 3.2 结果里的真实文件名调整通配符）：

```sql
PRAGMA query_only = ON;

SELECT id, upload_session_id, segment_order, status, file_path, normalized_file_path, created_at
FROM upload_missing_segment
WHERE live_streamer_id = <LIVE_STREAMER_ID>
  AND (
      file_path LIKE '%22_29_32%' OR file_path LIKE '%22-29-32%' OR file_path LIKE '%22:29:32%'
      OR file_path LIKE '%22_59_56%' OR file_path LIKE '%22-59-56%' OR file_path LIKE '%22:59:56%'
      OR file_path LIKE '%23_30_19%' OR file_path LIKE '%23-30-19%' OR file_path LIKE '%23:30:19%'
  )
ORDER BY created_at, id;
```

## 四、三个切片文件的落盘状态

用 3.1/3.2/3.3 结果里对应行的 `normalized_file_path`（为空则用 `file_path`），
分别对 `22:29:32`、`22:59:56`、`23:30:19` 三个文件执行：

```bash
# 存在性 + 大小 + mtime（Linux）
stat --format='path=%n size=%s mtime=%y' <FILE_PATH> 2>&1 || echo "MISSING: <FILE_PATH>"

# 找不到 upload_missing_segment 里的路径时，按录制目录 + 时间戳模糊定位
find <RECORDING_DIR> -maxdepth 2 -iname '*22_29_32*' -o -iname '*22-29-32*'
find <RECORDING_DIR> -maxdepth 2 -iname '*22_59_56*' -o -iname '*22-59-56*'
find <RECORDING_DIR> -maxdepth 2 -iname '*23_30_19*' -o -iname '*23-30-19*'

# 媒体有效性
ffprobe -v error -show_streams -show_format -of json <FILE_PATH>
```

若文件不存在，直接记录 `MISSING`，不要凭空猜测大小/时长。

## 五、04:54:30 重复项的 identity 证据

先确认这个时间戳属于三个文件里的哪一个还是另一条 missing 行/videos_json 条目
（07 号任务文档只给了 22:29:32/22:59:56/23:30:19 三个切片时间，`04:54:30` 大概率是次日凌晨的
另一个 order，需要在 3.x 结果或 `videos_json` 里单独定位，不要假设它是上面三个之一）。

定位到具体 `upload_missing_segment` 行或 `videos_json` 条目后，采集：

```sql
PRAGMA query_only = ON;

SELECT id, segment_order, status, file_path, normalized_file_path, video_json, created_at, updated_at
FROM upload_missing_segment
WHERE live_streamer_id = <LIVE_STREAMER_ID>
  AND (file_path LIKE '%04_54_30%' OR file_path LIKE '%04-54-30%' OR file_path LIKE '%04:54:30%')
ORDER BY created_at, id;
```

对每一条候选重复行，采集：

- `normalized_file_path`（源文件路径去重后的唯一标识）；
- `video_json` 里的远端 `filename` 字段（B 站返回的稿件内部文件名，不是本地路径）；
- 对应本地文件的 `ffprobe` 时长，和 videos_json/video_json 里如果记录了时长的话做交叉核对。

**只有标题相同不构成重复证据**，必须给出源路径或远端 `filename` 至少一项一致/不一致的判断依据，
在 [audit-report.md](audit-report.md) 里原样引用。

## 六、B 站实际分 P 列表（公开接口，不需要登录态）

用二节拿到的 `aid`（没有 `aid` 就用 `bvid` 换算或直接传 `bvid`）：

```bash
curl -fsS "https://api.bilibili.com/x/web-interface/view?aid=<AID>" |
jq '{
  code,
  message,
  title: .data.title,
  bvid: .data.bvid,
  aid: .data.aid,
  videos: .data.videos,
  pages: [.data.pages[] | {page, part, cid, duration, first_frame}]
}'
```

这是公开接口，不需要 `SESSDATA`；如果生产机没有出网权限或该稿件不可公开访问（未过审/私密），
把上面命令原样列进回传结果，标注「待人工在有出网权限的环境执行」，不要用生产授权 cookie 硬套。

## 回传格式

请按以下顺序原样回传，不要只给结论：

### 1. 数据库信息

```text
数据库路径：
mtime：
文件大小：
journal_mode：
```

### 2. 会话 #227

```text
（二节 SQL 完整结果，包括 videos_json 全文）
```

### 3. missing 行

```text
（3.1 / 3.2 / 3.3 三个查询的完整结果）
```

### 4. 三个切片文件

```text
22:29:32：存在性 / 大小 / mtime / ffprobe 摘要
22:59:56：存在性 / 大小 / mtime / ffprobe 摘要
23:30:19：存在性 / 大小 / mtime / ffprobe 摘要
```

### 5. 04:54:30 重复项证据

```text
候选行列表 + normalized_file_path / 远端 filename / 时长交叉核对结果
```

### 6. B 站实际分 P

```json
{}
```

（或注明「待人工在有出网权限的环境执行第六节命令」）
