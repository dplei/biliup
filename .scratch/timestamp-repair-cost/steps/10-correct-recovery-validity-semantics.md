# 10 · 修正取回通道有效性的 skill 判据

Status: resolved
Blocked by: —
优先级：P1——避免 agent 因描述符非空而错误承诺原片仍可取回

## 为什么

生产验证确认：`upos_recovery_json` 仍非空时，带原始 auth 的 GET 也可能已经返回 403。
数据库的 7 天 TTL 是敏感字段的保留上限，不是 UPOS 凭证有效期；现有 `segment-recover` 把两者
写成同一件事，会在真正救片时作出错误判断。

## 做法

- 同步修改 `.agents/skills/segment-recover/SKILL.md` 与
  `.claude/skills/segment-recover/SKILL.md`：描述符非空只表示“可以尝试”，实际 GET 成功并完成
  字节校验才证明当前可取回。
- 保留 `bldsa` 的已知硬拒绝；其它已测线路也不能再承诺永久可取回。GET 403 在核对描述符未被
  截断后按“凭证已失效或对象不可访问”停止，不反复变换请求方式，也不继续修复/回推。
- 同步更正 `docs/agents/skills.md` 与本 effort 中面向当前行为的说明；历史实测记录保留事实，
  只追加“后续验证推翻了有效期假设”的指针。
- 不调整数据库 TTL：目前只知道它不是有效期，尚不知道正确有效期；skill 改为实测后，保留一条
  已失效描述符不会再造成错误授权。

## 验收

- 两份 skill 除 `Claude` / `Codex` 描述差异外保持一致。
- 用 skill-creator 的 `quick_validate.py` 分别校验两个 skill 目录。
- 搜索仓库，不再出现“描述符未过 7 天即可取回”或“非空就有通道”的现行判断。

## 明确不做

- 不新增定时 HEAD/GET 探测；真正使用时的 GET 已是最权威且最少的一次请求。
- 不把一次生产观测猜成固定的 N 小时有效期。
- 不改上传线路白名单；线路能力与临时凭证寿命是两件事。

## Answer

两份 `segment-recover` skill 已同步收紧判据：描述符非空只能授权一次实际 GET 尝试，GET 403
且字段完整时按凭证失效或对象不可访问停止；只有 GET 成功且下载字节数等于 `total_bytes` 才
继续修复。`docs/agents/skills.md` 已同步，06/07 的历史记录保留原文并追加后续验证指针。

数据库 7 天清理 TTL、上传线路白名单与取回实现均未改动；目前没有证据支持猜测固定凭证寿命。

## Comments

这是一项窄的 skill/文档修复，不需要等待 09 或新的坏片。
