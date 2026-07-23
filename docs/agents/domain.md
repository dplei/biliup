# 领域文档（Domain Docs）

各工程类 skill 在探索本仓库代码前，应如何消费领域文档。

本仓库采用**单 context** 布局：根目录一份 `CONTEXT.md`，加 `docs/adr/`。不使用 `CONTEXT-MAP.md`。

## 探索前先读这些

- 根目录的 **`CONTEXT.md`** —— 领域术语表与核心概念
- **`docs/adr/`** —— 读与当前改动区域相关的 ADR

这些文件**目前尚不存在**。不存在时**静默继续**：不要提示它们缺失，也不要主动建议先创建。`/domain-modeling` skill（经 `/grill-with-docs` 与 `/improve-codebase-architecture` 触发）会在术语或决策真正需要沉淀时惰性创建它们。

## 目录结构

```
/
├── CONTEXT.md                ← 领域术语表（尚未创建）
├── docs/
│   ├── adr/                  ← 架构决策记录（尚未创建）
│   │   ├── 0001-xxx.md
│   │   └── 0002-xxx.md
│   ├── agents/               ← 本目录：skill 配置
│   └── superpowers/          ← 既有的 plans / specs 归档
├── crates/                   ← Rust 后端、CLI、录制与弹幕
│   ├── biliup/
│   ├── biliup-cli/
│   ├── stream-gears/
│   └── danmaku/
└── app/                      ← Next.js 前端
```

Cargo workspace 下的 4 个 crate 是同一产品的内部分层，共享同一套领域语言，因此归属同一个 context，不各自持有 `CONTEXT.md`。

## 使用术语表里的词汇

当你的产出提到某个领域概念（issue 标题、重构提案、假设、测试名），使用 `CONTEXT.md` 中定义的那个词。不要漂移到术语表明确回避的同义词。

如果需要的概念还不在术语表里，这本身就是个信号——要么你在发明项目并不使用的语言（应重新考虑），要么确实存在缺口（记下来交给 `/domain-modeling`）。

## 标记与 ADR 的冲突

如果你的产出与既有 ADR 相矛盾，请显式点出，而不是无声地覆盖它：

> _与 ADR-0007 相矛盾——但值得重新讨论，因为……_
