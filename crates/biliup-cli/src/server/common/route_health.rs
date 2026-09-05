//! 直播拉流线路健康、熔断与候选选择。
//!
//! 本模块只处理已经由 `check_stream` 确认仍在直播的下载结果。这样 404、正常 EOF
//! 等信号不会越过直播间的下播判定。签名 query 不进入 [`RouteKey`]，刷新签名后仍会
//! 延续同一条线路的故障历史。

use crate::server::core::downloader::DownloadStatus;
use biliup::downloader::live::{LiveStream, StreamCandidate, StreamProtocol};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const FAILURE_WINDOW: Duration = Duration::from_secs(2 * 60);
pub const ROUTE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
pub const ROUTE_STABLE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub host: Option<String>,
    pub protocol: &'static str,
    pub quality: Option<String>,
    pub codec: Option<String>,
}

impl RouteKey {
    pub fn from_stream(stream: &LiveStream) -> Self {
        if let Some(candidate) = stream
            .stream_candidates
            .iter()
            .find(|candidate| candidate.url == stream.raw_stream_url)
        {
            return Self::from_candidate(candidate);
        }
        let parsed = url::Url::parse(&stream.raw_stream_url).ok();
        let path = parsed.as_ref().map(url::Url::path).unwrap_or_default();
        Self {
            host: parsed
                .as_ref()
                .and_then(|url| url.host_str())
                .map(|host| host.to_ascii_lowercase()),
            protocol: if stream.suffix.eq_ignore_ascii_case("m3u8")
                || stream.suffix.eq_ignore_ascii_case("ts")
                || path.ends_with(".m3u8")
            {
                "hls"
            } else {
                "flv"
            },
            quality: stream.recording_quality.clone(),
            codec: None,
        }
    }

    pub fn from_candidate(candidate: &StreamCandidate) -> Self {
        Self {
            host: url::Url::parse(&candidate.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .or_else(|| candidate.host.as_deref().map(str::to_ascii_lowercase)),
            protocol: protocol_name(candidate.protocol),
            quality: candidate.quality.clone(),
            codec: candidate.codec.as_deref().map(str::to_ascii_lowercase),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthUpdate {
    Unchanged,
    AuthRefresh {
        key: RouteKey,
    },
    Failure {
        key: RouteKey,
        failures: u32,
        circuit_opened: bool,
        alert: bool,
    },
    Recovered {
        key: RouteKey,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteSelection {
    Selected { key: RouteKey, changed: bool },
    Unavailable { retry_after: Duration },
}

#[derive(Debug, Clone)]
pub struct RouteMetricSnapshot {
    pub key: RouteKey,
    pub attempts: u64,
    pub failures: u64,
    pub stable_attempts: u64,
    pub connected_for: Duration,
}

#[derive(Debug, Clone)]
pub struct RouteHealthSnapshot {
    pub connection_failures: u64,
    pub route_switches: u64,
    pub successful_switches: u64,
    pub flv_to_hls_switches: u64,
    pub successful_flv_to_hls_switches: u64,
    pub flv_to_hls_connected_for: Duration,
    pub all_routes_backoffs: u64,
    pub routes: Vec<RouteMetricSnapshot>,
}

#[derive(Debug, Default)]
struct RouteRuntimeMetric {
    attempts: u64,
    failures: u64,
    stable_attempts: u64,
    connected_for: Duration,
}

#[derive(Debug)]
struct PendingSwitch {
    target: RouteKey,
    flv_to_hls: bool,
}

#[derive(Debug, Default)]
struct RouteRecord {
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
    cooldown_until: Option<Instant>,
    auth_refresh_pending: bool,
}

impl RouteRecord {
    fn is_cooling_down(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }

    fn retry_after(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .filter(|until| *until > now)
            .map(|until| until.saturating_duration_since(now))
    }

    fn clear(&mut self) -> bool {
        let was_unhealthy = self.consecutive_failures > 0
            || self.cooldown_until.is_some()
            || self.auth_refresh_pending;
        *self = Self::default();
        was_unhealthy
    }
}

/// 每场直播独立持有的线路健康状态。
#[derive(Debug)]
pub struct RouteHealthState {
    enabled: bool,
    routes: HashMap<RouteKey, RouteRecord>,
    current_route_key: Option<RouteKey>,
    storm_alert_sent: bool,
    metrics: HashMap<RouteKey, RouteRuntimeMetric>,
    connection_failures: u64,
    route_switches: u64,
    successful_switches: u64,
    flv_to_hls_switches: u64,
    successful_flv_to_hls_switches: u64,
    flv_to_hls_connected_for: Duration,
    all_routes_backoffs: u64,
    pending_switch: Option<PendingSwitch>,
}

impl RouteHealthState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            routes: HashMap::new(),
            current_route_key: None,
            storm_alert_sent: false,
            metrics: HashMap::new(),
            connection_failures: 0,
            route_switches: 0,
            successful_switches: 0,
            flv_to_hls_switches: 0,
            successful_flv_to_hls_switches: 0,
            flv_to_hls_connected_for: Duration::ZERO,
            all_routes_backoffs: 0,
            pending_switch: None,
        }
    }

    pub fn current_route_key(&self) -> Option<&RouteKey> {
        self.current_route_key.as_ref()
    }

    pub fn begin_attempt(&mut self, stream: &LiveStream) {
        if self.enabled {
            let key = RouteKey::from_stream(stream);
            self.metrics.entry(key.clone()).or_default().attempts += 1;
            self.current_route_key = Some(key);
        }
    }

    pub fn metrics_snapshot(&self) -> RouteHealthSnapshot {
        let mut routes: Vec<_> = self
            .metrics
            .iter()
            .map(|(key, metric)| RouteMetricSnapshot {
                key: key.clone(),
                attempts: metric.attempts,
                failures: metric.failures,
                stable_attempts: metric.stable_attempts,
                connected_for: metric.connected_for,
            })
            .collect();
        routes.sort_by(|left, right| {
            left.key
                .quality
                .cmp(&right.key.quality)
                .then(left.key.protocol.cmp(right.key.protocol))
                .then(left.key.host.cmp(&right.key.host))
        });
        RouteHealthSnapshot {
            connection_failures: self.connection_failures,
            route_switches: self.route_switches,
            successful_switches: self.successful_switches,
            flv_to_hls_switches: self.flv_to_hls_switches,
            successful_flv_to_hls_switches: self.successful_flv_to_hls_switches,
            flv_to_hls_connected_for: self.flv_to_hls_connected_for,
            all_routes_backoffs: self.all_routes_backoffs,
            routes,
        }
    }

    /// 只可在直播间已确认仍为 Live 后调用。
    pub fn observe_live_attempt(
        &mut self,
        status: Option<&DownloadStatus>,
        connected_for: Duration,
        completed_configured_segment: bool,
        productive_attempt: bool,
        now: Instant,
    ) -> HealthUpdate {
        if !self.enabled || matches!(status, Some(DownloadStatus::Cancelled)) {
            return HealthUpdate::Unchanged;
        }
        let Some(key) = self.current_route_key.clone() else {
            return HealthUpdate::Unchanged;
        };

        let stable_attempt =
            connected_for >= ROUTE_STABLE_THRESHOLD || completed_configured_segment;
        let metric = self.metrics.entry(key.clone()).or_default();
        metric.connected_for = metric.connected_for.saturating_add(connected_for);
        if stable_attempt {
            metric.stable_attempts += 1;
        }
        if self
            .pending_switch
            .as_ref()
            .is_some_and(|pending| pending.target == key)
        {
            let pending = self.pending_switch.take().expect("pending switch exists");
            if pending.flv_to_hls {
                self.flv_to_hls_connected_for =
                    self.flv_to_hls_connected_for.saturating_add(connected_for);
            }
            if stable_attempt {
                self.successful_switches += 1;
                if pending.flv_to_hls {
                    self.successful_flv_to_hls_switches += 1;
                }
            }
        }

        let record = self.routes.entry(key.clone()).or_default();
        let recovered = if stable_attempt || productive_attempt {
            let storm_was_active = self.storm_alert_sent;
            self.storm_alert_sent = false;
            record.clear() || storm_was_active
        } else {
            false
        };

        if matches!(
            status,
            Some(DownloadStatus::HttpStatus { status: 401 | 403 })
        ) && !record.auth_refresh_pending
        {
            record.auth_refresh_pending = true;
            return HealthUpdate::AuthRefresh { key };
        }

        if !is_counted_transport_failure(status) {
            record.auth_refresh_pending = false;
            return if recovered {
                HealthUpdate::Recovered { key }
            } else {
                HealthUpdate::Unchanged
            };
        }

        record.auth_refresh_pending = false;
        self.connection_failures += 1;
        self.metrics.entry(key.clone()).or_default().failures += 1;
        if record
            .last_failure_at
            .is_some_and(|last| now.saturating_duration_since(last) <= FAILURE_WINDOW)
        {
            record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        } else {
            record.consecutive_failures = 1;
        }
        record.last_failure_at = Some(now);
        let circuit_opened = record.consecutive_failures >= 2;
        if circuit_opened {
            record.cooldown_until = Some(now + ROUTE_COOLDOWN);
        }
        let alert = circuit_opened && !self.storm_alert_sent;
        if alert {
            self.storm_alert_sent = true;
        }
        HealthUpdate::Failure {
            key,
            failures: record.consecutive_failures,
            circuit_opened,
            alert,
        }
    }

    /// 从刷新后的真实候选中选择下一条线路，并直接写回下一次下载使用的流信息。
    /// 当前线路健康时保持不动；熔断时从其后开始轮询，避免主动切回高优先级线路。
    pub fn select_route(
        &mut self,
        stream: &mut LiveStream,
        now: Instant,
        failover_enabled: bool,
    ) -> RouteSelection {
        let previous = self.current_route_key.clone();
        if !self.enabled || !failover_enabled || stream.stream_candidates.is_empty() {
            let key = RouteKey::from_stream(stream);
            return RouteSelection::Selected {
                changed: previous.as_ref().is_some_and(|previous| previous != &key),
                key,
            };
        }

        let candidates = &stream.stream_candidates;
        let current_index = previous.as_ref().and_then(|current| {
            candidates
                .iter()
                .position(|candidate| RouteKey::from_candidate(candidate) == *current)
        });
        let current_available = previous
            .as_ref()
            .is_some_and(|key| !self.is_cooling_down(key, now));

        let selected_index = if current_available && current_index.is_some() {
            current_index
        } else {
            let start = current_index.map_or(0, |index| index + 1);
            (0..candidates.len())
                .map(|offset| (start + offset) % candidates.len())
                .find(|index| {
                    let key = RouteKey::from_candidate(&candidates[*index]);
                    !self.is_cooling_down(&key, now)
                })
        };

        let Some(selected_index) = selected_index else {
            self.all_routes_backoffs += 1;
            let retry_after = candidates
                .iter()
                .filter_map(|candidate| {
                    self.routes
                        .get(&RouteKey::from_candidate(candidate))
                        .and_then(|record| record.retry_after(now))
                })
                .min()
                .unwrap_or(Duration::from_secs(30));
            return RouteSelection::Unavailable { retry_after };
        };

        let candidate = candidates[selected_index].clone();
        apply_candidate(stream, &candidate);
        let key = RouteKey::from_candidate(&candidate);
        let changed = previous.as_ref().is_some_and(|previous| previous != &key);
        if changed {
            self.route_switches += 1;
            let flv_to_hls = previous
                .as_ref()
                .is_some_and(|previous| previous.protocol == "flv" && key.protocol == "hls");
            if flv_to_hls {
                self.flv_to_hls_switches += 1;
            }
            self.pending_switch = Some(PendingSwitch {
                target: key.clone(),
                flv_to_hls,
            });
        }
        RouteSelection::Selected { changed, key }
    }

    fn is_cooling_down(&self, key: &RouteKey, now: Instant) -> bool {
        self.routes
            .get(key)
            .is_some_and(|record| record.is_cooling_down(now))
    }
}

fn protocol_name(protocol: StreamProtocol) -> &'static str {
    match protocol {
        StreamProtocol::Flv => "flv",
        StreamProtocol::Hls => "hls",
    }
}

fn apply_candidate(stream: &mut LiveStream, candidate: &StreamCandidate) {
    stream.raw_stream_url.clone_from(&candidate.url);
    stream.suffix = match candidate.protocol {
        StreamProtocol::Flv => "flv",
        StreamProtocol::Hls => "m3u8",
    }
    .to_string();
    stream.recording_quality.clone_from(&candidate.quality);
}

fn is_counted_transport_failure(status: Option<&DownloadStatus>) -> bool {
    match status {
        None
        | Some(
            DownloadStatus::StreamEnded
            | DownloadStatus::IncompleteFrame { .. }
            | DownloadStatus::ReadTimeout { .. }
            | DownloadStatus::Error(_),
        ) => true,
        Some(DownloadStatus::HttpStatus { status }) => {
            *status >= 500 || matches!(*status, 401 | 403 | 404)
        }
        Some(
            DownloadStatus::Downloading
            | DownloadStatus::SegmentCompleted
            | DownloadStatus::Cancelled,
        ) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use biliup::downloader::live::DownloaderHint;
    use chrono::Utc;
    use std::collections::HashMap;

    fn candidate(
        host: &str,
        protocol: StreamProtocol,
        quality: &str,
        codec: &str,
        resolution: &str,
        priority: u16,
    ) -> StreamCandidate {
        let suffix = match protocol {
            StreamProtocol::Flv => "flv",
            StreamProtocol::Hls => "m3u8",
        };
        StreamCandidate {
            url: format!("https://{host}/live/fixture.{suffix}?sign={priority}"),
            host: Some(host.to_string()),
            protocol,
            quality: Some(quality.to_string()),
            codec: Some(codec.to_string()),
            resolution: Some(resolution.to_string()),
            priority,
        }
    }

    fn stream(candidates: Vec<StreamCandidate>) -> LiveStream {
        let first = candidates.first().expect("candidate");
        LiveStream {
            name: "fixture".to_string(),
            url: "https://live.douyin.com/fixture".to_string(),
            title: "fixture".to_string(),
            date: Utc::now(),
            live_cover_url: String::new(),
            raw_stream_url: first.url.clone(),
            platform: "douyin".to_string(),
            stream_headers: HashMap::new(),
            suffix: match first.protocol {
                StreamProtocol::Flv => "flv",
                StreamProtocol::Hls => "m3u8",
            }
            .to_string(),
            danmaku: None,
            downloader_hint: DownloaderHint::StreamGears,
            runtime_options: None,
            recording_quality: first.quality.clone(),
            attempt_id: Some("attempt-fixture".to_string()),
            live_session_key: Some("fixture-room".to_string()),
            stream_candidates: candidates,
        }
    }

    fn fail_twice(health: &mut RouteHealthState, stream: &LiveStream, now: Instant) {
        health.begin_attempt(stream);
        let _ = health.observe_live_attempt(
            Some(&DownloadStatus::ReadTimeout { buffered: 1 }),
            Duration::from_secs(30),
            false,
            false,
            now,
        );
        health.begin_attempt(stream);
        let update = health.observe_live_attempt(
            Some(&DownloadStatus::IncompleteFrame { buffered: 1 }),
            Duration::from_secs(30),
            false,
            false,
            now + Duration::from_secs(31),
        );
        assert!(matches!(
            update,
            HealthUpdate::Failure {
                failures: 2,
                circuit_opened: true,
                ..
            }
        ));
    }

    #[test]
    fn two_flv_failures_switch_to_different_host_before_hls() {
        let now = Instant::now();
        let mut stream = stream(vec![
            candidate(
                "flv-a.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                0,
            ),
            candidate(
                "flv-b.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                1,
            ),
            candidate(
                "hls.example",
                StreamProtocol::Hls,
                "origin",
                "h264",
                "1080p",
                2,
            ),
        ]);
        let mut health = RouteHealthState::new(true);
        fail_twice(&mut health, &stream, now);

        let selection = health.select_route(&mut stream, now + Duration::from_secs(32), true);
        assert!(matches!(
            selection,
            RouteSelection::Selected { changed: true, .. }
        ));
        assert_eq!(
            RouteKey::from_stream(&stream).host.as_deref(),
            Some("flv-b.example")
        );
    }

    #[test]
    fn falls_back_to_hls_when_no_other_flv_host_exists() {
        let now = Instant::now();
        let mut stream = stream(vec![
            candidate(
                "flv.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                0,
            ),
            candidate(
                "hls.example",
                StreamProtocol::Hls,
                "origin",
                "h264",
                "1080p",
                1,
            ),
        ]);
        let mut health = RouteHealthState::new(true);
        fail_twice(&mut health, &stream, now);
        let _ = health.select_route(&mut stream, now + Duration::from_secs(32), true);
        assert_eq!(stream.suffix, "m3u8");
        health.begin_attempt(&stream);
        let _ = health.observe_live_attempt(
            Some(&DownloadStatus::SegmentCompleted),
            Duration::from_secs(90),
            true,
            false,
            now + Duration::from_secs(122),
        );
        let metrics = health.metrics_snapshot();
        assert_eq!(metrics.connection_failures, 2);
        assert_eq!(metrics.route_switches, 1);
        assert_eq!(metrics.successful_switches, 1);
        assert_eq!(metrics.flv_to_hls_switches, 1);
        assert_eq!(metrics.successful_flv_to_hls_switches, 1);
        assert_eq!(metrics.flv_to_hls_connected_for, Duration::from_secs(90));
    }

    #[test]
    fn protocol_fallback_disabled_candidate_set_never_switches_to_hls() {
        let now = Instant::now();
        let mut stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut health = RouteHealthState::new(true);
        fail_twice(&mut health, &stream, now);
        assert!(matches!(
            health.select_route(&mut stream, now + Duration::from_secs(32), true),
            RouteSelection::Unavailable { .. }
        ));
        assert_eq!(stream.suffix, "flv");
    }

    #[test]
    fn cooling_route_is_not_selected_and_all_open_routes_back_off() {
        let now = Instant::now();
        let candidates = vec![
            candidate(
                "flv.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                0,
            ),
            candidate(
                "hls.example",
                StreamProtocol::Hls,
                "origin",
                "h264",
                "1080p",
                1,
            ),
        ];
        let mut health = RouteHealthState::new(true);
        let mut current = stream(candidates.clone());
        fail_twice(&mut health, &current, now);
        let _ = health.select_route(&mut current, now + Duration::from_secs(32), true);
        fail_twice(&mut health, &current, now + Duration::from_secs(40));

        let mut refreshed = stream(candidates);
        let selection = health.select_route(&mut refreshed, now + Duration::from_secs(72), true);
        assert!(
            matches!(selection, RouteSelection::Unavailable { retry_after } if !retry_after.is_zero())
        );
    }

    #[test]
    fn codec_change_has_a_distinct_route_key_and_selection() {
        let h264 = candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        );
        let h265 = candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h265",
            "2160p",
            1,
        );
        assert_ne!(
            RouteKey::from_candidate(&h264),
            RouteKey::from_candidate(&h265)
        );
    }

    #[test]
    fn signed_query_is_not_part_of_the_route_key() {
        let mut first = candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        );
        let mut refreshed = first.clone();
        first.url = "https://flv.example/live/fixture.flv?sign=old".to_string();
        refreshed.url = "https://flv.example/live/fixture.flv?sign=new&expire=2".to_string();
        assert_eq!(
            RouteKey::from_candidate(&first),
            RouteKey::from_candidate(&refreshed)
        );
    }

    #[test]
    fn failures_outside_the_two_minute_window_restart_the_counter() {
        let now = Instant::now();
        let stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream);
        let _ = health.observe_live_attempt(None, Duration::ZERO, false, false, now);
        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                None,
                Duration::ZERO,
                false,
                false,
                now + FAILURE_WINDOW + Duration::from_secs(1),
            ),
            HealthUpdate::Failure {
                failures: 1,
                circuit_opened: false,
                ..
            }
        ));
    }

    #[test]
    fn stable_attempt_clears_failure_count_and_cooldown() {
        let now = Instant::now();
        let stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut health = RouteHealthState::new(true);
        fail_twice(&mut health, &stream, now);
        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                Some(&DownloadStatus::SegmentCompleted),
                ROUTE_STABLE_THRESHOLD,
                true,
                false,
                now + ROUTE_STABLE_THRESHOLD,
            ),
            HealthUpdate::Recovered { .. }
        ));
        let mut refreshed = stream.clone();
        assert!(matches!(
            health.select_route(&mut refreshed, now + ROUTE_STABLE_THRESHOLD, true),
            RouteSelection::Selected { .. }
        ));
    }

    #[test]
    fn productive_short_eof_never_accumulates_failures() {
        let now = Instant::now();
        let stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut health = RouteHealthState::new(true);

        for attempt in 0..20 {
            health.begin_attempt(&stream);
            assert!(matches!(
                health.observe_live_attempt(
                    Some(&DownloadStatus::StreamEnded),
                    Duration::from_secs(30),
                    false,
                    true,
                    now + Duration::from_secs(attempt),
                ),
                HealthUpdate::Failure {
                    failures: 1,
                    circuit_opened: false,
                    ..
                }
            ));
        }
        assert_eq!(health.metrics_snapshot().routes[0].stable_attempts, 0);
    }

    #[test]
    fn auth_failure_refreshes_before_it_is_counted() {
        let now = Instant::now();
        let stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                Some(&DownloadStatus::HttpStatus { status: 403 }),
                Duration::ZERO,
                false,
                false,
                now,
            ),
            HealthUpdate::AuthRefresh { .. }
        ));
        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                Some(&DownloadStatus::HttpStatus { status: 403 }),
                Duration::ZERO,
                false,
                false,
                now + Duration::from_secs(1),
            ),
            HealthUpdate::Failure { failures: 1, .. }
        ));
    }

    #[test]
    fn cancellation_and_disabled_health_do_not_count_failures() {
        let now = Instant::now();
        let stream = stream(vec![candidate(
            "flv.example",
            StreamProtocol::Flv,
            "origin",
            "h264",
            "1080p",
            0,
        )]);
        let mut enabled = RouteHealthState::new(true);
        enabled.begin_attempt(&stream);
        assert_eq!(
            enabled.observe_live_attempt(
                Some(&DownloadStatus::Cancelled),
                Duration::ZERO,
                false,
                false,
                now,
            ),
            HealthUpdate::Unchanged
        );

        let mut disabled = RouteHealthState::new(false);
        disabled.begin_attempt(&stream);
        assert_eq!(
            disabled.observe_live_attempt(None, Duration::ZERO, false, false, now),
            HealthUpdate::Unchanged
        );
    }

    #[test]
    fn failover_rollback_keeps_the_refreshed_primary_route() {
        let now = Instant::now();
        let mut stream = stream(vec![
            candidate(
                "flv.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                0,
            ),
            candidate(
                "hls.example",
                StreamProtocol::Hls,
                "origin",
                "h264",
                "1080p",
                1,
            ),
        ]);
        let mut health = RouteHealthState::new(true);
        fail_twice(&mut health, &stream, now);
        assert!(matches!(
            health.select_route(&mut stream, now + Duration::from_secs(32), false),
            RouteSelection::Selected { changed: false, .. }
        ));
        assert_eq!(stream.suffix, "flv");
    }

    #[test]
    fn failure_storm_alert_is_emitted_once_until_recovery() {
        let now = Instant::now();
        let mut stream = stream(vec![
            candidate(
                "flv.example",
                StreamProtocol::Flv,
                "origin",
                "h264",
                "1080p",
                0,
            ),
            candidate(
                "hls.example",
                StreamProtocol::Hls,
                "origin",
                "h264",
                "1080p",
                1,
            ),
        ]);
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream);
        let _ = health.observe_live_attempt(None, Duration::ZERO, false, false, now);
        health.begin_attempt(&stream);
        let first = health.observe_live_attempt(
            None,
            Duration::ZERO,
            false,
            false,
            now + Duration::from_secs(1),
        );
        assert!(matches!(first, HealthUpdate::Failure { alert: true, .. }));
        let _ = health.select_route(&mut stream, now + Duration::from_secs(2), true);
        health.begin_attempt(&stream);
        let _ = health.observe_live_attempt(
            None,
            Duration::ZERO,
            false,
            false,
            now + Duration::from_secs(3),
        );
        health.begin_attempt(&stream);
        let second = health.observe_live_attempt(
            None,
            Duration::ZERO,
            false,
            false,
            now + Duration::from_secs(4),
        );
        assert!(matches!(second, HealthUpdate::Failure { alert: false, .. }));

        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                Some(&DownloadStatus::SegmentCompleted),
                Duration::from_secs(60),
                true,
                false,
                now + Duration::from_secs(5),
            ),
            HealthUpdate::Recovered { .. }
        ));
        health.begin_attempt(&stream);
        let _ = health.observe_live_attempt(
            None,
            Duration::ZERO,
            false,
            false,
            now + Duration::from_secs(6),
        );
        health.begin_attempt(&stream);
        assert!(matches!(
            health.observe_live_attempt(
                None,
                Duration::ZERO,
                false,
                false,
                now + Duration::from_secs(7),
            ),
            HealthUpdate::Failure { alert: true, .. }
        ));
    }
}
