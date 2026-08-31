# Issue tracker：本地 Markdown

本仓库的 spec（spec 也常被称作 PRD）与实现 ticket 以 markdown 文件形式存放在 `.scratch/` 下。

> **与 GitHub issue 的分工**：`dplei/biliup` 的 GitHub Issues 是**问题入口与对外记录**——
> 发现问题在那里开 issue；`.scratch/` 是**设计与拆解的工作区**——根因分析、方案取舍、
> ticket 拆分都落在这里，并在 spec 顶部用 `来源：` 链回那条 issue。两边都要有。
>
> 建 issue 用 `gh issue create --repo dplei/biliup`。**`--repo` 不能省**：本仓库是 fork，
> 省掉它 `gh` 会把 issue 开到上游 `biliup/biliup`。

## 约定

- 一个功能一个目录：`.scratch/<feature-slug>/`
- spec 位于 `.scratch/<feature-slug>/spec.md`
- 实现类 issue 一个 ticket 一个文件：`.scratch/<feature-slug>/issues/<NN>-<slug>.md`，从 `01` 开始编号——**不要**把所有 ticket 合并进单个文件
- 每个 issue 文件顶部附近用一行 `Status:` 记录 triage 状态，取值为五个规范角色之一：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`
- 评论与讨论记录追加到文件末尾的 `## Comments` 标题下

## 完结后归档到 `.archive/`

`.scratch/` 只放**还在推进**的 effort。一项工作彻底完结时，把整个
`.scratch/<feature-slug>/` 目录用 `git mv` 原样移动到 `.archive/<feature-slug>/`，
并在 [`.archive/README.md`](../../.archive/README.md) 的「已归档」表格补一行
（目录、内容一句话、归档日期、完结依据）。归档只挪位置，不改内容。

**同时满足才算完结**：

- 目录里每个 issue 的 `Status:` 都是 `resolved` 或 `wontfix`；
- spec / assessment 一类总览文件不再留有待办——`ready-for-human` 和 `needs-info`
  都表示还有人要做的事，不归档；
- 代码已落到 `dev`，验证结论已写进目录内的文件。

只要还剩一条待真实环境验收、待补数据、待部署观察，就留在 `.scratch/`。

收尾一项工作时顺手判断一次；不确定是否完结就留在 `.scratch/`，别抢在验收前归档。

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
