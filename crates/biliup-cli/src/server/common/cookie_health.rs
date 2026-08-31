//! 平台 cookie 健康监测。
//!
//! 直播间检查（`check_stream`）天然区分两种结果：`Ok(Offline)` = 主播确实没播（正常），
//! `Err(_)` 会进一步分类为鉴权、传输、服务端或响应结构错误。只有明确鉴权失败才会
//! 累积 cookie/sessionid 告警；其余错误只做分类计数与诊断。
//!
//! 检测点是监控循环（每 `event_loop_interval` 秒轮询每个直播间，含没开播的）+ 录制时的
//! 断流复查，所以「主动自检」几乎零额外开销。用「连续失败阈值 + 去抖」避免单个坏 URL 或
//! 录制断流时的快速重试风暴造成误报。

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use serde_json::json;
use tracing::{info, warn};

#[derive(Clone, Copy, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthErrorKind {
    Authentication,
    Transport,
    Server,
    InvalidResponse,
}

/// 连续失败达到此数才判定为异常（监控约每 30s 一次，3 次≈1.5 分钟）。
const UNHEALTHY_THRESHOLD: u32 = 3;
/// 去抖窗口：此时间内的重复失败只更新信息、不累加计数。
/// 用于吸收录制断流时 download.rs 的快速重试（4s/8s）风暴，避免瞬时抖动误报。
const ERROR_DEBOUNCE_MS: i64 = 15_000;

/// Where a credential failure was observed. The monitor loop and the recording reconnect path
/// both land here, so one stage name keeps them comparable.
const LIVE_CHECK_STAGE: &str = "live_check";

/// 单个平台的 cookie 健康状态。
#[derive(Clone, Serialize, Default)]
pub struct PlatformHealth {
    /// 平台名（plugin.name()，如 "Douyin"）
    pub platform: String,
    /// 是否判定为异常（cookie 可能失效）
    pub unhealthy: bool,
    /// 当前连续失败次数
    pub consecutive_errors: u32,
    /// 最近一次成功检查的时间戳（ms）
    pub last_ok_ms: Option<i64>,
    /// 最近一次错误信息（截断）
    pub last_error: Option<String>,
    /// 最近一次错误的时间戳（ms）
    pub last_error_ms: Option<i64>,
    /// 进入异常状态的时间戳（ms），用于前端显示「自 X 起检测失败」
    pub since_ms: Option<i64>,
    /// 最近一次失败类别；只有 authentication 会触发 cookie 告警。
    pub last_error_kind: Option<HealthErrorKind>,
    pub authentication_errors: u64,
    pub transport_errors: u64,
    pub server_errors: u64,
    pub invalid_response_errors: u64,
}

static HEALTH: LazyLock<RwLock<HashMap<String, PlatformHealth>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 平台展示名（推送/横幅文案用）。
fn display(platform: &str) -> &str {
    match platform {
        "Douyin" => "抖音",
        other => other,
    }
}

/// 记录一次成功的直播间检查（无论在播/未播）：清零失败计数；若此前为异常则发「已恢复」。
pub fn record_success(platform: &str, webhook: Option<&str>) {
    let mut recovered = false;
    {
        let mut map = HEALTH.write().unwrap();
        let h = map.entry(platform.to_string()).or_default();
        h.platform = platform.to_string();
        h.last_ok_ms = Some(now_ms());
        h.consecutive_errors = 0;
        h.last_error = None;
        h.last_error_kind = None;
        if h.unhealthy {
            h.unhealthy = false;
            h.since_ms = None;
            recovered = true;
        }
    }
    if recovered {
        info!(platform, "cookie 健康已恢复");
        crate::observe::auth::health_changed(platform, "recovered", "authentication_recovered", 0);
        notify(
            webhook,
            &format!("✅ {} cookie 已恢复正常", display(platform)),
            &format!(
                "{} 直播间检查恢复成功，cookie 工作正常。",
                display(platform)
            ),
        );
    }
}

/// 记录一次失败的直播间检查（取流/检测出错或风控）：去抖累计，达阈值→标记异常并推送。
pub fn record_error(platform: &str, err: &str, webhook: Option<&str>) {
    record_error_at(platform, err, webhook, now_ms());
}

/// The clock is a parameter so the debounce window and the unhealthy threshold can be exercised
/// without waiting real seconds. Production always passes the real clock.
pub(crate) fn record_error_at(platform: &str, err: &str, webhook: Option<&str>, now: i64) {
    let err = redact_sensitive(err);
    let kind = classify_error(&err);
    let mut became_unhealthy = false;
    // Both native events are emitted after the write lock is released: a collector must never
    // run while the health map is locked.
    let consecutive = {
        let mut map = HEALTH.write().unwrap();
        let h = map.entry(platform.to_string()).or_default();
        h.platform = platform.to_string();
        h.last_error = Some(err.chars().take(300).collect());
        h.last_error_kind = Some(kind);
        match kind {
            HealthErrorKind::Authentication => {
                h.authentication_errors = h.authentication_errors.saturating_add(1)
            }
            HealthErrorKind::Transport => h.transport_errors = h.transport_errors.saturating_add(1),
            HealthErrorKind::Server => h.server_errors = h.server_errors.saturating_add(1),
            HealthErrorKind::InvalidResponse => {
                h.invalid_response_errors = h.invalid_response_errors.saturating_add(1)
            }
        }
        if kind != HealthErrorKind::Authentication {
            h.last_error_ms = Some(now);
            warn!(
                platform,
                ?kind,
                error = err,
                "live check failed without cookie invalidation"
            );
            drop(map);
            crate::observe::auth::operation_failed(
                platform,
                LIVE_CHECK_STAGE,
                crate::observe::auth::reason_of(kind),
            );
            return;
        }
        // 去抖：窗口内的重复失败只更新信息、不累加（吸收录制断流的快速重试）
        let debounced = h
            .last_error_ms
            .map(|t| now - t < ERROR_DEBOUNCE_MS)
            .unwrap_or(false);
        h.last_error_ms = Some(now);
        if debounced {
            return;
        }
        h.consecutive_errors = h.consecutive_errors.saturating_add(1);
        if !h.unhealthy && h.consecutive_errors >= UNHEALTHY_THRESHOLD {
            h.unhealthy = true;
            h.since_ms = Some(now);
            became_unhealthy = true;
        }
        h.consecutive_errors
    };
    // A debounced repeat updates nothing, so it reports nothing: the event count stays equal to
    // the counted failures rather than to the retry storm behind them.
    crate::observe::auth::operation_failed(
        platform,
        LIVE_CHECK_STAGE,
        crate::observe::auth::reason_of(kind),
    );
    if became_unhealthy {
        warn!(platform, error = err, "cookie 鉴权失败：连续检查失败");
        crate::observe::auth::health_changed(
            platform,
            "failed",
            "authentication_failed",
            consecutive,
        );
        notify(
            webhook,
            &format!("⚠️ {} cookie 鉴权连续失败", display(platform)),
            &format!(
                "{} 直播间连续检查失败，建议尽快更换 cookie（sessionid）。最近错误：{}",
                display(platform),
                err
            ),
        );
    }
}

pub fn classify_error(error: &str) -> HealthErrorKind {
    let lower = error.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("login expired")
        || lower.contains("session invalid")
        || lower.contains("风控")
        || lower.contains("登录态")
    {
        HealthErrorKind::Authentication
    } else if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("dns")
        || lower.contains("certificate")
        || lower.contains("connection")
        || lower.contains("reset")
        || lower.contains("network")
    {
        HealthErrorKind::Transport
    } else if (500..=599).any(|status| lower.contains(&status.to_string())) {
        HealthErrorKind::Server
    } else {
        HealthErrorKind::InvalidResponse
    }
}

/// Redact signed URLs and credential-like key/value pairs before an error crosses a log boundary.
pub fn redact_sensitive(value: &str) -> String {
    static URLS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"https?://[^\s\"'<>\)]+"#).expect("valid URL redaction regex")
    });
    static SECRETS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)(cookie|token|mstoken|a_bogus|verifyfp|sign|signature|expire)=([^&\s,;]+)",
        )
        .expect("valid secret redaction regex")
    });
    let without_queries = URLS.replace_all(value, |captures: &regex::Captures<'_>| {
        let raw = captures.get(0).expect("whole match").as_str();
        url::Url::parse(raw)
            .map(|mut url| {
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|_| "[REDACTED_URL]".to_string())
    });
    SECRETS
        .replace_all(&without_queries, "$1=[REDACTED]")
        .into_owned()
}

/// 供 `/v1/health/cookie` 接口返回的快照。
pub fn snapshot() -> serde_json::Value {
    let map = HEALTH.read().unwrap();
    let mut platforms: Vec<PlatformHealth> = map.values().cloned().collect();
    platforms.sort_by(|a, b| a.platform.cmp(&b.platform));
    json!({ "platforms": platforms })
}

/// webhook 通知：URL 含 `{title}`/`{content}` 占位 → GET 替换（兼容 Bark/Server酱）；
/// 否则 POST JSON `{"title":..,"content":..}`（兼容企业微信/钉钉/自建）。失败只记日志、不影响主流程。
fn notify(webhook: Option<&str>, title: &str, content: &str) {
    let Some(url) = webhook else {
        return;
    };
    let url = url.trim().to_string();
    if url.is_empty() {
        return;
    }
    let title = title.to_string();
    let content = content.to_string();
    tokio::spawn(async move {
        match send_webhook(&url, &title, &content).await {
            Ok(()) => info!("cookie 健康通知已发送"),
            Err(e) => warn!(error = e, "cookie 健康通知发送失败"),
        }
    });
}

/// 可等待、可判定投递结果的 Webhook 底层传输。租约通知以它作为持久重试边界；
/// 原有健康告警继续由上面的 fire-and-forget 包装调用。
pub async fn send_webhook(url: &str, title: &str, content: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| format!("创建 Webhook 客户端失败：{error}"))?;
    let is_dingtalk = url.contains("oapi.dingtalk.com");
    let response = if is_dingtalk {
        // 钉钉自定义机器人：固定带 biliup 关键词，便于使用机器人的关键词安全策略。
        let body = json!({
            "msgtype": "text",
            "text": { "content": format!("【biliup】{title}\n{content}") }
        });
        client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
    } else if url.contains("qyapi.weixin.qq.com") {
        let body = json!({
            "msgtype": "text",
            "text": { "content": format!("{title}\n{content}") }
        });
        client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
    } else if url.contains("{title}") || url.contains("{content}") {
        let final_url = url
            .replace("{title}", &urlencoding::encode(title))
            .replace("{content}", &urlencoding::encode(content));
        client.get(&final_url).send().await
    } else {
        client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({ "title": title, "content": content }))
            .send()
            .await
    }
    .map_err(|error| format!("Webhook 请求失败：{error}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 Webhook 响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("Webhook 返回 HTTP {status}"));
    }
    if is_dingtalk {
        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| format!("钉钉响应不是合法 JSON：{error}"))?;
        if json.get("errcode").and_then(|value| value.as_i64()) != Some(0) {
            let message = json
                .get("errmsg")
                .and_then(|value| value.as_str())
                .unwrap_or("未知错误");
            return Err(format!("钉钉机器人拒绝消息：{message}"));
        }
    }
    Ok(())
}

/// 对外告警入口：复用 cookie 健康的 webhook 分发逻辑，用于时间戳修复等其它告警场景。
pub fn notify_alert(webhook: Option<&str>, title: &str, content: &str) {
    notify(webhook, title, content);
}

/// 画质代码排名，越小越高；未知画质排到最低。
fn quality_rank(code: &str) -> usize {
    const ITEMS: [&str; 6] = ["origin", "uhd", "hd", "sd", "ld", "md"];
    ITEMS.iter().position(|i| *i == code).unwrap_or(ITEMS.len())
}

/// 画质代码 → 中文展示名（未知码原样返回）。
pub fn quality_display(code: &str) -> String {
    match code {
        "origin" => "原画".to_string(),
        "uhd" => "蓝光".to_string(),
        "hd" => "超清".to_string(),
        "sd" => "高清".to_string(),
        "ld" => "标清".to_string(),
        "md" => "流畅".to_string(),
        other => other.to_string(),
    }
}

/// 默认告警阈值：抖音实际录到的画质低于此档即推送。
/// 调整默认值或将来做成可配置项时，只需改这一处。
pub const DEFAULT_QUALITY_ALERT: &str = "uhd";

/// 解析生效的告警阈值：None/空白 → [`DEFAULT_QUALITY_ALERT`]；其余（含 "off"）trim 后原样返回。
/// 判定与文案展示共用此函数，避免缺省逻辑多处重复。
pub fn effective_quality_alert(threshold: Option<&str>) -> &str {
    threshold
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_QUALITY_ALERT)
}

/// 实际画质是否低于告警阈值（应推送）。
/// threshold 为 None/空 → 默认 [`DEFAULT_QUALITY_ALERT`]；"off" → 关闭（恒 false）。
pub fn quality_below_alert(actual: &str, threshold: Option<&str>) -> bool {
    let threshold = effective_quality_alert(threshold);
    if threshold == "off" {
        return false;
    }
    quality_rank(actual) > quality_rank(threshold)
}

#[cfg(test)]
mod alert_tests {
    use super::notify_alert;

    #[tokio::test]
    async fn notify_alert_with_none_webhook_is_noop() {
        // 不应 panic；webhook 为 None 时静默返回
        notify_alert(None, "t", "c");
        notify_alert(Some(""), "t", "c");
    }
}

#[cfg(test)]
mod health_classification_tests {
    use super::*;

    #[test]
    fn timeout_is_transport_not_authentication() {
        assert_eq!(
            classify_error("request timed out while connecting"),
            HealthErrorKind::Transport
        );
    }

    #[test]
    fn forbidden_is_authentication() {
        assert_eq!(
            classify_error("HTTP status 403 Forbidden"),
            HealthErrorKind::Authentication
        );
    }

    #[test]
    fn signed_query_and_cookie_are_redacted() {
        let redacted = redact_sensitive(
            "GET https://example.test/live.flv?msToken=secret&a_bogus=also-secret Cookie=sessionid",
        );
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("msToken="));
        assert!(!redacted.contains("a_bogus="));
        assert!(!redacted.contains("sessionid"));
    }
}

#[cfg(test)]
mod quality_alert_tests {
    use super::{effective_quality_alert, quality_below_alert, quality_display};

    #[test]
    fn effective_threshold_resolves_default_and_passthrough() {
        assert_eq!(effective_quality_alert(None), "uhd");
        assert_eq!(effective_quality_alert(Some("")), "uhd");
        assert_eq!(effective_quality_alert(Some("  ")), "uhd");
        assert_eq!(effective_quality_alert(Some("hd")), "hd");
        assert_eq!(effective_quality_alert(Some(" off ")), "off");
    }

    #[test]
    fn below_threshold_triggers() {
        assert!(quality_below_alert("hd", Some("uhd")));
    }

    #[test]
    fn at_or_above_threshold_no_trigger() {
        assert!(!quality_below_alert("uhd", Some("uhd")));
        assert!(!quality_below_alert("origin", Some("uhd")));
    }

    #[test]
    fn off_disables() {
        assert!(!quality_below_alert("md", Some("off")));
    }

    #[test]
    fn none_or_empty_defaults_to_uhd() {
        assert!(quality_below_alert("hd", None));
        assert!(quality_below_alert("hd", Some("")));
        assert!(!quality_below_alert("uhd", None));
    }

    #[test]
    fn display_maps_codes() {
        assert_eq!(quality_display("uhd"), "蓝光");
        assert_eq!(quality_display("hd"), "超清");
        assert_eq!(quality_display("xxx"), "xxx");
    }
}

#[cfg(test)]
mod health_event_tests {
    use super::*;
    use biliup_observability::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    struct Memory(Arc<Mutex<Vec<Event>>>);
    impl Consumer for Memory {
        fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
            self.0.lock().unwrap().extend_from_slice(batch);
            Ok(Commit::default())
        }
    }

    /// Each test uses its own platform key, so the process-wide health map never couples them.
    fn collect(body: impl FnOnce()) -> Vec<Event> {
        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink = events.clone();
        let mut runtime = Runtime::start(
            "synthetic",
            "test",
            Options {
                enabled: true,
                ..Options::default()
            },
            move || Ok(Memory(sink.clone())),
        )
        .unwrap();
        let subscriber =
            tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered());
        tracing::subscriber::with_default(subscriber, body);
        runtime.shutdown(std::time::Duration::from_secs(5));
        events.lock().unwrap().clone()
    }

    fn native(events: &[Event], name: &str) -> Vec<EventData> {
        events
            .iter()
            .map(Event::data)
            .filter(|data| data.event_name == name)
            .cloned()
            .collect()
    }

    fn field(data: &EventData, key: &str) -> String {
        data.fields
            .get(key)
            .map(|value| value.as_str().unwrap_or_default().to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn only_the_state_machine_declares_unhealthy_and_recovered() {
        let platform = "SyntheticTransition";
        let events = collect(|| {
            // Spaced past the debounce window: three counted authentication failures are what
            // the threshold is defined in terms of.
            for step in 0..3 {
                record_error_at(
                    platform,
                    "401 unauthorized",
                    None,
                    step * ERROR_DEBOUNCE_MS * 2,
                );
            }
            record_success(platform, None);
        });

        let failures = native(&events, "auth.operation_failed");
        assert_eq!(failures.len(), 3);
        for failure in &failures {
            assert_eq!(field(failure, "reason_code"), "authentication_failed");
            assert_eq!(field(failure, "stage"), "live_check");
            assert_eq!(field(failure, "platform"), platform);
            assert_eq!(field(failure, "outcome"), "failed");
        }

        // The two transitions, in order, and nothing in between: a failure below the threshold
        // must not claim the platform is unhealthy.
        let changes = native(&events, "auth.health_changed");
        assert_eq!(changes.len(), 2);
        assert_eq!(field(&changes[0], "outcome"), "failed");
        assert_eq!(field(&changes[0], "reason_code"), "authentication_failed");
        assert_eq!(changes[0].fields.get("count").unwrap().as_u64(), Some(3));
        assert_eq!(changes[0].level, Level::Warn);
        assert_eq!(field(&changes[1], "outcome"), "recovered");
        assert_eq!(
            field(&changes[1], "reason_code"),
            "authentication_recovered"
        );
        assert_eq!(changes[1].level, Level::Info);

        // A second success does not repeat the recovery: the state is already healthy.
        let quiet = collect(|| record_success(platform, None));
        assert!(native(&quiet, "auth.health_changed").is_empty());
    }

    #[test]
    fn debounced_repeats_do_not_multiply_events() {
        let platform = "SyntheticDebounce";
        let events = collect(|| {
            // A reconnect storm inside one debounce window updates the record without counting,
            // so it must not turn into a stream of events either.
            for step in 0..5 {
                record_error_at(platform, "403 forbidden", None, 1_000 + step * 100);
            }
        });
        assert_eq!(native(&events, "auth.operation_failed").len(), 1);
        assert!(native(&events, "auth.health_changed").is_empty());
    }

    #[test]
    fn failure_kind_is_typed_and_carries_no_error_text() {
        let platform = "SyntheticKinds";
        let secret = "connection reset while fetching \
             https://example.invalid/live?token=super-secret-value";
        let events = collect(|| {
            record_error_at(platform, secret, None, 1_000);
            record_error_at(platform, "503 service unavailable", None, 2_000);
            record_error_at(platform, "unexpected payload", None, 3_000);
        });

        let failures = native(&events, "auth.operation_failed");
        let reasons: Vec<String> = failures
            .iter()
            .map(|data| field(data, "reason_code"))
            .collect();
        assert_eq!(
            reasons,
            ["transport_error", "server_error", "invalid_response"]
        );
        // None of these are authentication failures, so the health state never moves.
        assert!(native(&events, "auth.health_changed").is_empty());

        for failure in &failures {
            for (_, value) in failure.fields.iter() {
                let rendered = value.to_string();
                assert!(!rendered.contains("super-secret-value"), "{rendered}");
                assert!(!rendered.contains("example.invalid"), "{rendered}");
            }
            assert!(!failure.message.contains("super-secret-value"));
        }
    }
}
