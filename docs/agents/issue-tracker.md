# Issue tracker：本地 Markdown

本仓库的 issue 与 spec（spec 也常被称作 PRD）以 markdown 文件形式存放在 `.scratch/` 下。

> 说明：`dplei/biliup` 是个人部署用的 fork，GitHub Issues 已关闭；`gh issue create` 会落到上游 `biliup/biliup`，因此**不要**用 `gh` 建 issue。

## 约定

- 一个功能一个目录：`.scratch/<feature-slug>/`
- spec 位于 `.scratch/<feature-slug>/spec.md`
- 实现类 issue 一个 ticket 一个文件：`.scratch/<feature-slug>/issues/<NN>-<slug>.md`，从 `01` 开始编号——**不要**把所有 ticket 合并进单个文件
- 每个 issue 文件顶部附近用一行 `Status:` 记录 triage 状态，取值为五个规范角色之一：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`
- 评论与讨论记录追加到文件末尾的 `## Comments` 标题下

## 当某个 skill 说「发布到 issue tracker」时

在 `.scratch/<feature-slug>/` 下新建文件（目录不存在则创建）。

## 当某个 skill 说「取出相关 ticket」时

读取被引用路径的文件。通常主人会直接给出路径或 issue 编号。

## 与 `docs/superpowers/` 的关系

`docs/superpowers/{plans,specs}` 里已有的设计文档是历史沉淀，属于长期保留的成品文档；`.scratch/` 是流程中的工作区。新工作走 `.scratch/`，定稿后若值得长期留存再归档到 `docs/superpowers/`。

## Wayfinding 操作

供 `/wayfinder` 使用。**map** 是一个文件，其下每个 ticket 一个**子**文件。

- **Map**：`.scratch/<effort>/map.md` —— 包含 Notes / Decisions-so-far / Fog 正文。
- **子 ticket**：`.scratch/<effort>/issues/NN-<slug>.md`，从 `01` 开始编号，正文写问题。`Type:` 行记录 ticket 类型（`research`/`prototype`/`grilling`/`task`）；`Status:` 行记录 `claimed`/`resolved`。
- **阻塞**：顶部附近写一行 `Blocked by: NN, NN`。当它列出的每个文件都变为 `resolved` 时，该 ticket 解除阻塞。
- **Frontier**：扫描 `.scratch/<effort>/issues/`，找出未关闭、未被阻塞、未被认领的文件；编号最小者优先。
- **认领**：开工前先把 `Status:` 设为 `claimed` 并保存。
- **解决**：在 `## Answer` 标题下追加答案，把 `Status:` 设为 `resolved`，然后把一条 context 指针（要点 + 链接）追加到 `map.md` 的 Decisions-so-far。
