# Spec：`pre_upload` 瞬时失败不再终结整个上传 attempt

Status: ready-for-agent

来源：[`dplei/biliup#4`](https://github.com/dplei/biliup/issues/4)

分支：`issue-4-upload-preprocess-reuse`

本文按当前 `dev` 重新收敛 Issue #4。旧方案设计了内容指纹、失败缓存和缓存回收；PR #14 合入后，
这些机制已不再是解决事故所必需的最短路径。

---

## 1. 当前结论

Issue 描述的主要损耗已经被 PR #14 间接消除：默认
`audio_normalization_keep_original=false` 时，校验通过的响度标准化产物会原子替换原片，随后立即在
`upload_missing_segment.audio_normalized_at` 留标记。上传失败后的自动补传、人工补传和换线重投都会
读到同一文件，并因标记跳过第二次 loudnorm。

仍未解决的是触发事故的前半段：服务端 `pre_upload` 仍是单次网络请求。一次 DNS、连接或请求超时会
立即终结当前 attempt，等下一轮恢复调度；同时这个访问 `member.bilibili.com` 的失败还会被错误记到
所选 UPOS 线路的健康账上。

因此本 effort 改成只修两件彼此相邻的事：

1. 服务端 `pre_upload` 的瞬时网络错误在同一个 attempt 内有界重试；
2. `pre_upload` 失败不再污染具体上传线路的 breaker。

## 2. 当前代码流

服务端有两个真实调用点：

- `upload_single_file`：录制期上传、静默补传、人工补传和换线重投最终都走这里；
- 页面整场上传：在 `upload` / `upload_with_task` 的逐文件循环里直接调用 `Line::pre_upload`。

两处都先经过 `upload_rate_gate::before_pre_upload`，再裸调用 `Line::pre_upload`。失败后会调用
`record_non_rate_limit_failure`，并通过 `record_line_kind_failure` 冷却当前线路。

`Line::pre_upload` 请求的是统一的 B 站预上传端点，选中的线路只作为查询参数传入；在拿到 UPOS
bucket 之前还没有连接实际上传线路。因此 DNS、连接、超时、TLS 或 HTTP 失败都不能证明某条 UPOS
线路有问题。

独立 CLI 上传不走响度标准化链，且有自己围绕 601 的进程锁重试语义；本次不改。把重试塞进
`Line::pre_upload` 也不合适，因为底层不知道服务端的全局限流闸，每次真实请求都必须单独计数。

## 3. 设计

### 3.1 复用现有重试器，不增加配置

在 `crates/biliup-cli/src/server/common/upload.rs` 内放一个私有的服务端预上传 helper，两处服务端调用
共同使用。helper 复用 `biliup::retry_with_config`，固定最多重试 3 次；不增加配置项、模块或依赖。

每次真实请求都重新创建 `VideoFile`，并在请求前调用一次 `before_pre_upload`。这样限流闸记录的请求数
与实际发出的请求严格一致。

### 3.2 只重试没有明确远端结论的网络错误

直接复用 `upload_line_health::classify_kind` 的现有分类：

| 分类 | 是否重试 | 理由 |
| --- | --- | --- |
| `ConnectTimeout` / `RequestTimeout` / `Transport` | 是 | 包含事故中的 DNS/连接抖动，没有明确远端结论 |
| `RateLimit601` | 否 | 立即交给持久全局冷却闸 |
| `HttpStatus` | 否 | 当前分类没有保留 4xx/5xx，不能安全猜测 |
| `CertificateExpired` / `CertificateInvalid` | 否 | 同一次请求重试不会改变证书状态 |

若以后需要重试 5xx，应先让分类保留状态码；本次不靠错误字符串再解析一次。

### 3.3 限流闸按每次请求收口

每一次 `pre_upload` 返回后都要立即结束本次 gate 状态：

- 成功：`record_success`；
- 601：`record_rate_limited`，不进入普通重试；
- 其他失败：`record_non_rate_limit_failure`，再由 predicate 决定是否重试。

`record_non_rate_limit_failure` 不能拖到所有重试结束后。若本次请求恰好是冷却结束后的 probe，gate
处于 `Probing`；不先释放它，下一次重试会永远等在 `before_pre_upload`。

`before_pre_upload` 自己失败时没有发出网络请求，直接返回，不套网络重试；601 与普通网络错误也要
保留原始 `Kind` 供 predicate 判断，不能先压成一段字符串再被误判为 `Transport`。

### 3.4 预上传失败不写线路 breaker

两处服务端调用点都删除 `pre_upload` 错误分支里的 `record_line_kind_failure`。只有成功拿到 bucket 后的
实际分块上传失败，才继续更新具体线路健康。

不新增 `LocalResolution` 枚举。用错误文本区分“本机 DNS”和“其他 connect error”既脆弱也没有必要：
它们发生在同一个预上传主机上，都不是实际 UPOS 线路的证据。

### 3.5 attempt 与产物生命周期不变

预处理完成后仍进入 `queued`；重试期间继续持有全局上传 permit，现有两小时 queue deadline 足够覆盖
秒级退避。重试成功时外层 attempt 从未失败，也不会重新进入 loudnorm。

重试耗尽后，默认原地替换的标准化文件和 `audio_normalized_at` 标记仍由 #14 保留，下一轮恢复直接
上传它。`audio_normalization_keep_original=true` 是明确选择保留原片的逃生门，仍沿用临时产物语义；
不为这个非默认模式重新引入长寿命缓存和磁盘回收系统。

## 4. 非目标

- 不实现内容指纹、跨进程产物缓存或缓存回收。
- 不缓存时间戳修复产物。
- 不修改 attempt 阶段模型、恢复调度间隔或上传线路选择。
- 不改独立 CLI 的 601 锁与重试策略。
- 不新增针对某一操作系统或错误文案的 DNS 类型。

## 5. Step

| # | 内容 | 状态 | 依赖 |
| --- | --- | --- | --- |
| [01](./steps/01-preupload-transient-retry.md) | 两个服务端调用点共用有界重试与正确 gate 收口 | resolved | — |
| [02](./steps/02-fingerprint-artifact-naming.md) | 内容指纹命名 | wontfix | 被 #14 的原地替换取代 |
| [03](./steps/03-keep-and-reuse-artifact.md) | 失败缓存与命中复用 | wontfix | 被 #14 的原地替换与账本标记取代 |
| [04](./steps/04-artifact-cache-reaping.md) | 缓存回收 | wontfix | 不再创建长寿命缓存 |
| [05](./steps/05-local-dns-failure-kind.md) | `LocalResolution` 类型 | wontfix | 改为所有预上传失败都不写线路 breaker |
| [06](./steps/06-end-to-end-verification.md) | 自动回归与 dev 验收 | ready-for-agent | 01 |

## 6. 整体验收

1. 服务端预上传前两次返回瞬时网络错误、第三次成功：同一个 attempt 成功，实际请求和 gate 计数均为
   3，没有线路失败记录。
2. 返回 601：只请求一次，进入既有全局冷却，不进行普通重试，也不写线路 breaker。
3. 返回 HTTP 或证书错误：只请求一次，不写线路 breaker。
4. 瞬时网络错误耗尽：外层 attempt 正常失败；gate 不滞留在 `Probing`，线路 breaker 不变。
5. 默认原地替换模式下，失败后的下一轮恢复继续跳过 loudnorm；换线重投同样不重复编码。
6. `cargo test -p biliup-cli` 与 `python3 scripts/check_code_index.py` 通过。

## Comments

- 2026-09-08：Step 01 已实现；两个服务端入口共用 typed transient retry，gate 在每次真实请求后收口，
  预上传失败不再写入具体线路 breaker。完整故障注入与归档仍留给 Step 06。
- 2026-09-08：Step 02 复核关闭；默认原地替换与 `audio_normalized_at` 已覆盖跨 attempt 复用，未新增
  无消费者的内容指纹协议。
- 2026-09-08：Step 03 复核关闭；自动补传与人工/换线补传已共用持久标记判据，默认模式无需独立
  失败缓存，`keep_original` 继续保持短命临时件语义。
- 2026-09-08：按最新 `dev` 重审。PR #14 已让默认链路在标准化成功后原地替换并持久标记，原来的
  指纹缓存、缓存保留和回收设计全部撤销；剩余改动收敛为服务端 `pre_upload` 有界重试与归因修正。
