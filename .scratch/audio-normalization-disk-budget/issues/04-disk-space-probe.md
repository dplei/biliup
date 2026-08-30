# 04 — 跨平台可用空间探测模块

Status: implemented / 待验收（见 [`06`](./06-concurrency-acceptance.md)）

## 背景

见 [`spec.md` 第 5 节](../spec.md)。准入水位与转码期硬水位都要问同一个问题：
「这个分段所在的文件系统还剩多少可用字节」。先把它做成一个不依赖调用场景的小模块。

## 改动范围

新增 `crates/biliup-cli/src/server/common/disk_space.rs`，在
[`common/mod.rs`](../../../crates/biliup-cli/src/server/common) 挂上。

```rust
/// 返回 `path` 所在文件系统对**非特权用户**可用的字节数。
/// 探测不了就返回 None——调用方一律按「不做限制」处理（fail-open）。
pub fn available_bytes(path: &Path) -> Option<u64>;
```

要点：

- unix 走 `libc::statvfs`，取 `f_bavail * f_frsize`。用 `f_bavail`（非特权可用）而不是
  `f_bfree`（含 root 保留块），否则会高估。`libc` 已是 `biliup-cli` 的直接依赖
  （[`Cargo.toml:71`](../../../crates/biliup-cli/Cargo.toml)），不新增依赖。
- 非 unix 直接返回 `None`。这与
  [`process_priority.rs`](../../../crates/biliup-cli/src/server/common/process_priority.rs)
  的 `#[cfg(unix)]` no-op 惯例一致：平台能力缺失只降级，不失败。
- 探测目标是**文件所在目录**。传进来的若是文件路径，取其 `parent()`；目录不存在或
  `statvfs` 返回非零，都归为 `None`。
- 纯同步函数，不需要 async：`statvfs` 是一次内存中的 syscall，量级与 `metadata()` 相当，
  不值得为它开 `spawn_blocking`。

## 验收标准

1. 单测：对临时目录调用返回 `Some(n)` 且 `n > 0`。
2. 单测：对该临时目录下一个**文件**路径调用，结果与对目录调用相同（证明取了 parent）。
3. 单测：对不存在的路径返回 `None`，不 panic。
4. 单测：返回值与 `df` 或 `statvfs` 的独立读数在同一量级（允许并发写入造成的漂移，
   断言相对误差而不是相等）。
5. `cargo test -p biliup-cli` 全绿；`cargo check` 在非 unix target 下也能编过
   （至少人工核对 `#[cfg]` 分支完整，无 `unused` 警告）。

## 风险

容器里 `statvfs` 报的是挂载点的空间，overlay 或 bind mount 下可能与宿主感知不一致。
这对本用途无害：我们要限制的正是**产物实际写入的那个文件系统**，`statvfs` 报的就是它。

## 实现记录（2026-08-30）

按计划落地，无偏离。`f_frsize` 为 0 时退回 `f_bsize`；`f_bavail`/`f_frsize` 的宽度随平台
而异，一律走 `try_from`，并对 clippy 的平台相关假阳性加了带注释的 `allow`。
