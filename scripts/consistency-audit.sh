#!/usr/bin/env bash
# 投稿一致性巡检：对所有已投稿会话比对 videos_json 与本地分段账本，
# 找出「同一内容进稿两次」「上传了没进稿」「序号不连续」三类错位。
#
# 全程只读。可以反复跑，也可以挂 cron 定期跑并 diff 结果。
#
# 用法：
#   DB=<数据库路径> bash scripts/consistency-audit.sh
#   DB=... bash scripts/consistency-audit.sh > audit-$(date +%F).txt   # 存档以便日后比对
#
# 不给 DB 时只在仓库内找 dev 库；核对生产库请显式传 DB=。

set -uo pipefail
DB="${DB:-}"
if [ -z "$DB" ]; then
  for c in ./data/data.sqlite3 ../data/data.sqlite3; do
    [ -f "$c" ] && DB="$c" && break
  done
fi
[ -f "$DB" ] || { echo "找不到数据库，用 DB=/path/to/data.sqlite3 指定"; exit 1; }

q() { sqlite3 -readonly -box "$DB" "PRAGMA query_only=ON; $1" 2>&1; }

# date -Is 是 GNU 扩展，macOS 的 BSD date 不认，退回到显式格式。
echo "投稿一致性巡检 @ $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "数据库：$DB"

echo
echo "== 1. 一致性总览（只看有补救记录的会话）=="
echo "   注意：upload_missing_segment 是补救账本，不是全量分段账本——走正常 pipeline 的会话"
echo "   在这张表里一行都没有，所以必须先筛掉无补救记录的会话，否则健康状态会被误判成异常。"
echo "   era: legacy=新投稿意图逻辑生效前  current=生效后"
q "
WITH v AS (
  SELECT s.id AS sid,
         json_extract(j.value,'\$.filename') AS fn,
         json_extract(j.value,'\$.title')    AS title
  FROM upload_session s, json_each(s.videos_json) j
  WHERE json_valid(s.videos_json) AND json_array_length(s.videos_json) > 0
),
vagg AS (
  SELECT sid,
         COUNT(*)                         AS n_videos,
         COUNT(*) - COUNT(DISTINCT fn)    AS dup_filename,
         COUNT(*) - COUNT(DISTINCT title) AS dup_title
  FROM v GROUP BY sid
),
seg AS (
  SELECT upload_session_id AS sid,
         COUNT(*)                  AS n_rows,
         SUM(status = 'succeeded') AS n_succeeded,
         SUM(video_json IS NOT NULL AND video_json != '' AND video_json != 'null') AS n_with_vjson
  FROM upload_missing_segment
  WHERE upload_session_id IS NOT NULL
  GROUP BY upload_session_id
)
SELECT s.id AS session,
       CASE WHEN s.submit_requested_at IS NULL THEN 'legacy' ELSE 'current' END AS era,
       vagg.n_videos, seg.n_rows, seg.n_succeeded,
       vagg.dup_filename, vagg.dup_title, seg.n_with_vjson,
       date(s.created_at) AS day
FROM upload_session s
JOIN vagg ON vagg.sid = s.id
JOIN seg  ON seg.sid  = s.id          -- INNER JOIN：无补救记录的会话不参与
WHERE vagg.dup_filename > 0
   OR vagg.dup_title > 0
ORDER BY s.id;"

echo "== 2. 对照组：有补救记录且无重复的会话数 =="
q "
WITH v AS (
  SELECT s.id AS sid, json_extract(j.value,'\$.filename') AS fn,
         json_extract(j.value,'\$.title') AS title
  FROM upload_session s, json_each(s.videos_json) j
  WHERE json_valid(s.videos_json) AND json_array_length(s.videos_json) > 0
),
vagg AS (SELECT sid, COUNT(*)-COUNT(DISTINCT fn) AS dup_fn,
                COUNT(*)-COUNT(DISTINCT title) AS dup_ti FROM v GROUP BY sid),
seg AS (SELECT upload_session_id AS sid FROM upload_missing_segment
        WHERE upload_session_id IS NOT NULL GROUP BY upload_session_id)
SELECT CASE WHEN s.submit_requested_at IS NULL THEN 'legacy' ELSE 'current' END AS era,
       COUNT(*) AS clean_sessions
FROM upload_session s JOIN vagg ON vagg.sid=s.id JOIN seg ON seg.sid=s.id
WHERE vagg.dup_fn = 0 AND vagg.dup_ti = 0
GROUP BY era;"
echo "   参考：全库会话总数与有稿件的会话数"
q "SELECT COUNT(*) AS all_sessions,
          SUM(bvid IS NOT NULL AND bvid != '') AS with_archive,
          SUM(id IN (SELECT DISTINCT upload_session_id FROM upload_missing_segment
                     WHERE upload_session_id IS NOT NULL)) AS with_recovery_rows
   FROM upload_session;"

echo "== 3. 异常会话里重复的具体条目（判断是真重复还是同模板标题的巧合）=="
echo "   同一 title 对应两个不同 filename = 真重复；filename 相同 = 同一次上传被记两次"
q "
WITH v AS (
  SELECT s.id AS sid,
         json_extract(j.value,'\$.filename') AS fn,
         json_extract(j.value,'\$.title')    AS title
  FROM upload_session s, json_each(s.videos_json) j
  WHERE json_valid(s.videos_json)
)
SELECT sid AS session, title,
       COUNT(*) AS times_in_archive,
       COUNT(DISTINCT fn) AS distinct_remote_files,
       CASE WHEN COUNT(DISTINCT fn) > 1 THEN '真重复：同内容上传两次'
            ELSE '同一次上传被记两次' END AS verdict
FROM v GROUP BY sid, title HAVING COUNT(*) > 1 ORDER BY sid;"

echo
echo "== 4. 分段行 video_json 的回填率（决定上面第 3 类探针是否可用）=="
echo "   n_with_vjson=0 的会话无法从分段行侧交叉验证，只能信 videos_json 一侧"
q "
SELECT CASE WHEN s.submit_requested_at IS NULL THEN 'legacy' ELSE 'current' END AS era,
       COUNT(DISTINCT m.upload_session_id) AS sessions,
       SUM(m.video_json IS NOT NULL AND m.video_json != '' AND m.video_json != 'null') AS rows_with_vjson,
       COUNT(*) AS rows_total
FROM upload_missing_segment m
JOIN upload_session s ON s.id = m.upload_session_id
GROUP BY era;"

echo
echo "== 5. last_error 的真实形态（issue #7 的影响面）=="
echo "   关键指标是「已成功却仍挂着 last_error」——重试成功后不清空，会让任何按"
echo "   last_error IS NOT NULL 判健康的逻辑全面误报。"
q "
SELECT CASE
         WHEN last_error IS NULL OR last_error = '' THEN '(空)'
         WHEN last_error LIKE 'Manually queued%'      THEN '非错误：人工入队'
         WHEN last_error LIKE 'Temporary overnight%'  THEN '非错误：兜底入队'
         WHEN last_error LIKE 'source file reappeared%' THEN '非错误：源文件重现'
         ELSE '真错误栈（含已成功后的残留）'
       END AS kind,
       COUNT(*) AS rows,
       SUM(status = 'succeeded') AS of_which_succeeded
FROM upload_missing_segment
GROUP BY kind ORDER BY rows DESC;"
echo "   汇总占比："
q "
SELECT SUM(status='succeeded' AND last_error IS NOT NULL AND last_error != '') AS succeeded_with_stale_error,
       COUNT(*) AS rows_total,
       ROUND(100.0 * SUM(status='succeeded' AND last_error IS NOT NULL AND last_error != '') / COUNT(*), 1) AS pct
FROM upload_missing_segment;"

echo
echo "巡检完毕，全部只读。"
