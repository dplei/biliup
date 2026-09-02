# 仓库自带的 agent skill：怎么用、怎么装

本仓库把几件「需要判断力、而不只是跑个命令」的事做成了 skill，交给 Claude Code 或 Codex
执行。这份文档说清每个 skill 干什么、怎么触发、以及在两个工具里各自怎么启用。

## 有哪些

| skill | 干什么 | 什么时候用 |
| --- | --- | --- |
| [`segment-recover`](../../.claude/skills/segment-recover/SKILL.md) | 把已经传上 B 站、但时间戳坏掉的分段取回本机，修好，换回原稿件的那一个分P | B 站审核说某个分P时间戳跳变；本地原片已经按流程删掉了 |
| [`cover-background`](../../.claude/skills/cover-background/SKILL.md) | 把一张原图调成压得住白字的 1146×717 封面背景 | 手上有张图想当封面底图，但不知道该压暗/模糊多少 |

根目录还有一个 [`SKILL.md`](../../SKILL.md)，那是给**别人**用的——教 agent 怎么安装和
操作 `biliup` 命令行。和上面两个互不引用。

## Claude Code

**不用装。** `.claude/skills/` 会被自动发现，开一个新会话就在了。两种触发方式：

- 直接说事：「B 站说我这个稿件第 3P 时间戳跳变，帮我修一下」——skill 的 `description`
  写了触发语，模型自己会调。
- 打斜杠命令：`/segment-recover`。

想确认它在不在，问一句「你现在有哪些 skill」即可。

## Codex

`.claude/` 那份 Codex 不读，所以仓库里另存了一份 [`.agents/skills/`](../../.agents/skills/)，
两份内容一致，只差描述里的「Claude」和「Codex」。

Codex 自己的 skill 目录是 `~/.codex/skills/<名字>/SKILL.md`，格式和这里完全一样。
**做一个软链就能让 Codex 用上仓库里的版本，而且改了仓库不用再同步一次：**

```bash
mkdir -p ~/.codex/skills && ln -sfn "$PWD/.agents/skills/segment-recover" ~/.codex/skills/segment-recover
```

封面那个同理：

```bash
ln -sfn "$PWD/.agents/skills/cover-background" ~/.codex/skills/cover-background
```

不想动全局配置也行——SKILL.md 本身就是一份自足的操作说明，直接让 Codex
「读 `.agents/skills/segment-recover/SKILL.md`，按它做」一样能跑完整条流程。

## 加一个新 skill 时

1. 写 `.claude/skills/<名字>/SKILL.md`，frontmatter 只要 `name` 和 `description`。
   **`description` 是触发器**：把用户可能怎么开口说这件事写进去，别只写功能摘要。
2. 复制一份到 `.agents/skills/<名字>/SKILL.md`，把描述里的「Claude」改成「Codex」，
   其余保持逐字一致。
3. 回来更新本文的表格。
4. `.gitignore` 里 `.claude/*` 整体忽略、`!.claude/skills/` 例外放行，所以 skill 是入库的，
   `settings.local.json` 之类仍然不入库。新增时不用改 `.gitignore`。

写 skill 的要点是**把判断留给 agent**，不是把命令列成清单。`cover-background` 强制
agent 把渲染出的样图读进来真的看；`segment-recover` 强制 agent 自己跑复检看数字、
并在回推前先跑预演等用户点头。少了这一层，skill 就退化成一份可以直接贴给用户的 README。

## 用 `segment-recover` 之前要准备什么

它需要三样东西，开工前先凑齐（细节见 skill 正文）：

- **稿件与分P**：`av`/`BV` 号，加上坏掉的是第几个分P（B 站页面上的 P1/P2/P3）。
- **取回描述符**：生产库里
  `select upos_recovery_json from upload_missing_segment where id = <missing_id>;`
  的结果，含 `endpoint` / `upos_uri` / `auth` 三个字段。**7 天 TTL，过期会被清成 NULL。**
- **原始大小**：同一行的 `total_bytes`，用来验证下载完整。

两条已知的止损线，省得白忙：

- `endpoint` 是 **`bldsa`** 的分段**拿不回来**。那条线路只让 `HEAD` 过，`GET` 一律 403。
  （上传选路现在会优先避开这类线路，但更早传上去的历史分段仍可能落在上面。）
- 描述符是 NULL，或者那次上传发生在凭证落库这个功能之前 → 没有取回通道。

`auth` 是凭证：**不要写进任何会提交的文件，不要贴进 commit / PR / issue。**
