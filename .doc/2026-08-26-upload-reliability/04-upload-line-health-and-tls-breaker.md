# 子任务 04：上传线路健康与 TLS 熔断

Status: completed

Blocked by: 01, 02

## 目标

将 bldsa 证书过期视为单线路上游故障，持久冷却并自动回退；所有上传入口共享线路健康结果，TLS 验证始终开启。

## 数据模型

migration 12 新增 `upload_line_health`：

- `line_key TEXT PRIMARY KEY`
- `consecutive_failures INTEGER NOT NULL DEFAULT 0`
- `cooldown_until DATETIME`
- `last_failure_kind TEXT`
- `last_error TEXT`
- `updated_at DATETIME NOT NULL`

`last_error` 保存有长度上限且脱敏的摘要。

## 详细步骤

### 1. 错误分类

- [x] 为 `Kind::Reqwest`、`Kind::ReqwestMiddleware` 和 error-stack source chain 提供统一分类器。
- [x] 至少区分 certificate_expired、certificate_invalid、connect_timeout、request_timeout、http_status、rate_limit_601 和 transport。
- [x] 分类器不得仅检查最外层 Display；必须遍历 source chain。
- [x] 分类日志不输出上传鉴权 query、X-Upos-Auth 或 Cookie。

### 2. 熔断策略

- [x] certificate_expired/certificate_invalid 首次失败立即熔断该 line 24 小时。
- [x] 普通网络故障按连续失败次数短冷却，不与 TLS 24 小时规则混淆。
- [x] 601 继续交由现有全局 UploadRateGate，不记作单线路故障。
- [x] 成功上传清零该线路普通失败计数。
- [x] 冷却到期只允许一次探测；探测失败重新进入冷却。

### 3. 线路选择

- [x] 恢复线路默认序列固定为 bda2、tx、auto，不包含 bldsa。
- [x] line index 表示下一候选位置；失败时只推进一次。
- [x] 选择前查询 health，跳过仍在冷却的显式线路。
- [x] auto Probe 不向冷却线路发探测请求。
- [x] 用户显式配置 bldsa 但其处于冷却时，记录 override 日志并回退到下一健康线路。
- [x] 所有候选均不可用时保持 failed/due，不回退到 `Line::default()`；默认值当前是 bldsa，必须避免隐式命中。

### 4. 与 attempt 状态机集成

- [x] claim 前确定实际 current_line 并写入 missing 行。
- [x] pre-upload、分块上传和完成请求的线路错误都更新同一 health 记录。
- [x] watchdog 超时只标记当前线路普通故障，不错误分类为 TLS。
- [x] 手动 retry 与静默恢复使用相同 selector，禁止再直接使用模板默认 line。

### 5. 可见性和告警

- [x] missing 页面显示当前/下次线路及跳过某线路的原因。
- [x] 健康接口返回线路状态、冷却截止时间和脱敏错误分类。
- [x] TLS 证书错误首次打开熔断时发一次 webhook，冷却期间不重复轰炸。
- [x] 文案明确“B 站上游需续签 bldsa 证书”；不建议关闭证书验证。

## 测试

- [x] bldsa 证书过期后立即选择 bda2/tx/auto 中的健康线路。
- [x] 重启进程后 bldsa 冷却仍然有效。
- [x] 显式 bldsa 配置不能绕过冷却。
- [x] 其他线路成功不被 bldsa 故障影响。
- [x] 601 不会错误打开线路熔断。
- [x] 所有线路失败时任务保留 failed，不使用默认 bldsa。

## 验收标准

- 证书过期窗口内对 bldsa 不再产生重复 probe/pre-upload。
- 页面和日志能解释实际选线及跳过原因。
- 代码和配置中不存在 `danger_accept_invalid_certs` 或等价关闭 TLS 校验行为。

## 完成记录

- 完成日期：2026-08-26
- 提交：`a738e6b`（`fix: persist upload line circuit breakers`）
- migration 12 新增持久线路健康表；TLS 故障 24 小时冷却，普通网络故障短冷却，冷却到期以数据库 CAS 限制单探测。
- 直播上传、CLI 上传、静默恢复和手动补传共享健康选择与失败记账；恢复序列固定为 `bda2 -> tx -> auto`。
- `auto` 探测会排除冷却线路，并把未选中线路的 probe 失败返回给熔断器，不再吞掉 `bldsa` 证书错误。
- `/v1/health/upload-lines` 暴露脱敏健康状态；missing 页面显示实际/下次线路和跳过原因；首次 TLS 熔断复用 webhook 告警。
- 验证：workspace check 通过；`biliup-cli --lib` 195 passed / 1 ignored；事故集成测试 11 passed / 3 ignored；`biliup --lib` 45 passed；TLS 禁用配置扫描为空。
- 前端 `next build` 编译成功，但类型检查仍被既有的 `app/ui/TemplateFields.tsx:117` 回调签名错误阻断，与本任务无关。
