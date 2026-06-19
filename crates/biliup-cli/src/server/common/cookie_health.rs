//! 平台 cookie 健康监测。
//!
//! 直播间检查（`check_stream`）天然区分两种结果：`Ok(Offline)` = 主播确实没播（正常），
//! `Err(_)` = 取流/检测出错或风控（cookie/sessionid 失效的信号）。本模块把这些结果汇总成
//! 「按平台」的健康状态，供前端横幅轮询与 webhook 主动推送使用。
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

/// 连续失败达到此数才判定为异常（监控约每 30s 一次，3 次≈1.5 分钟）。
const UNHEALTHY_THRESHOLD: u32 = 3;
/// 去抖窗口：此时间内的重复失败只更新信息、不累加计数。
/// 用于吸收录制断流时 download.rs 的快速重试（4s/8s）风暴，避免瞬时抖动误报。
const ERROR_DEBOUNCE_MS: i64 = 15_000;

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
        if h.unhealthy {
            h.unhealthy = false;
            h.since_ms = None;
            recovered = true;
        }
    }
    if recovered {
        info!(platform, "cookie 健康已恢复");
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
    let mut became_unhealthy = false;
    let now = now_ms();
    {
        let mut map = HEALTH.write().unwrap();
        let h = map.entry(platform.to_string()).or_default();
        h.platform = platform.to_string();
        h.last_error = Some(err.chars().take(300).collect());
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
    }
    if became_unhealthy {
        warn!(platform, err, "cookie 可能已失效：连续检查失败");
        notify(
            webhook,
            &format!("⚠️ {} cookie 可能已失效", display(platform)),
            &format!(
                "{} 直播间连续检查失败，建议尽快更换 cookie（sessionid）。最近错误：{}",
                display(platform),
                err
            ),
        );
    }
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
        let client = reqwest::Client::new();
        let result = if url.contains("oapi.dingtalk.com") {
            // 钉钉自定义机器人：需 {msgtype:text, text:{content}}。安全设置用「自定义关键词」
            // 时，文案必须含该关键词——这里固定前缀「【biliup】」，用户把关键词设为 biliup 即可。
            let body = json!({
                "msgtype": "text",
                "text": { "content": format!("【biliup】{title}\n{content}") }
            })
            .to_string();
            client
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
        } else if url.contains("qyapi.weixin.qq.com") {
            // 企业微信群机器人：同样需 {msgtype:text, text:{content}}
            let body = json!({
                "msgtype": "text",
                "text": { "content": format!("{title}\n{content}") }
            })
            .to_string();
            client
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
        } else if url.contains("{title}") || url.contains("{content}") {
            let final_url = url
                .replace("{title}", &urlencoding::encode(&title))
                .replace("{content}", &urlencoding::encode(&content));
            client.get(&final_url).send().await
        } else {
            let body = json!({ "title": title, "content": content }).to_string();
            client
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
        };
        match result {
            Ok(resp) => info!(status = ?resp.status(), "cookie 健康通知已发送"),
            Err(e) => warn!(error = ?e, "cookie 健康通知发送失败"),
        }
    });
}

/// 对外告警入口：复用 cookie 健康的 webhook 分发逻辑，用于时间戳修复等其它告警场景。
pub fn notify_alert(webhook: Option<&str>, title: &str, content: &str) {
    notify(webhook, title, content);
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
