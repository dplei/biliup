# 03 — 前端错误提示统一，不再透传原始 HTML

Status: resolved
Model: Haiku 4.5 —— 单文件、规格明确的前端改动，无并发与数据语义，改完肉眼可验。

## 背景

对应评估报告 B（前端部分）。504 时页面弹出整段 OpenResty HTML。

## 根因

[`handleResponse`](app/lib/api-streamer.ts:40) 在 `!res.ok` 时把响应体原文 `throw`：

```ts
const text = await res.text().catch(() => '')
throw new Error(text || `HTTP ${res.status}`)
```

补传页 [`app/(app)/missing/page.tsx`](app/(app)/missing/page.tsx) 的 `handleRecover` /
`handleRetry` / `handleDelete` / `handleRescan` 直接把 `e.message` 塞进 `Toast.error`。

## 改动范围

- `handleResponse` 判断响应 `Content-Type`：非 JSON（尤其是 `text/html`）不透传正文，按 status
  映射中文提示。至少覆盖 502 / 503 / 504 / 500 与网络中断。
  - 504 → 「服务端处理超时，任务可能仍在后台执行，请刷新查看状态」
- JSON 错误体保留现有行为（后端已有的中文/结构化错误不能被这层吞掉）。
- 保留 401 跳登录逻辑不变。
- 正文超长时截断，避免 Toast 撑爆。

## 验收

- 人为让接口返回 HTML 504，页面显示的是简洁中文提示，不出现任何 HTML 标签。
- 后端返回的 JSON 错误信息仍然完整可见。

## 备注

02 落地后 504 会大幅减少，但这层兜底仍需保留（其它接口同样可能超时）。两者可并行推进。

## Answer

已实现。`app/lib/api-streamer.ts` 的 `handleResponse` 改为调用新的 `describeError`：

- `application/json` 响应体按 `message` / `error` / `detail` 取字段透传（后端的中文错误不会被吞）；
- `text/html` 一律丢弃正文，按状态码翻译（504 →「服务端处理超时，任务可能仍在后台执行，请刷新查看状态」，
  另覆盖 400/403/404/409/429/500/502/503）；
- 纯文本短错误仍然展示（后端不少 handler 就是 `(StatusCode, &str)`），但以 `<` 开头的一律当 HTML 丢掉；
- 正文超过 300 字符截断。

401 跳登录逻辑未动。
