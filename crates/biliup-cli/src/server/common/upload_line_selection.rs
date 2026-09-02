//! The single upload-line decision.
//!
//! There used to be three: recording-time upload read `config.lines`, page uploads read
//! `config.lines` again through a different parser, and both recovery paths ignored it entirely
//! in favour of a hardcoded `bda2 -> tx -> auto` constant — which is why a box configured for
//! `alia` recovered over `bda2`, and why the page's "next line" column was consistently wrong in
//! exactly the same way (it reimplemented the same constant).
//!
//! Everything now goes through [`plan_upload_line`], a pure function over a cooldown snapshot,
//! plus [`resolve_planned_line`], which turns the planned key into a concrete [`Line`] (probing
//! only for `auto`). Because the plan is pure, the API can show the page the same decision the
//! uploader will make, without any network I/O.

use crate::server::common::upload_line_health::{self, LineAvailability};
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use biliup::uploader::line;
use biliup::uploader::line::{Line, Probe};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tracing::{info, warn};

/// Implicit fallbacks, in order, after the operator's own choice. `bldsa` is deliberately absent:
/// it is only ever used when explicitly configured.
const IMPLICIT_FALLBACKS: [&str; 2] = ["bda2", "tx"];

/// 上传完还能凭原始 `X-Upos-Auth` 把源对象整体 GET 回来的线路。
///
/// **取回通道是按线路存在的**：同一个 bucket、同一套 auth 机制，`bldsa` 只给 HEAD 200、
/// GET 一律 403（2026-09-02 实测，GET / Range / query-param / 加 UA 都试过；守卫是
/// `crates/biliup` 里那条 ignored 的 `upos_recovery_round_trip_by_line`）。
///
/// 这件事之所以要影响选路：预处理修不好的分段现在是「原片直传 + 告警 + 本地清理」，
/// 而「需要重修时从 B 站把原片取回来」是清掉本地文件的前提。落在没有取回通道的线路上，
/// 那个前提就不成立了。所以 auto 探测**优先只在这些线路里挑**，其余线路（含 `bldsa`）
/// 只有在它们全部不可用时才兜底——宁可失去取回通道，也不能传不上去。
///
/// 这是**实测白名单，不是按厂商推断的**。要加线路，先跑那条 ignored 测试确认它的 GET
/// 逐字节一致再往里加；跑之前挑没有录制的时段，真实上传会触发 601 账号级冷却。
/// 显式配置的线路不受这里影响：主人点名要哪条就用哪条。
const RECOVERABLE_LINES: [&str; 3] = ["bda2", "tx", "alia"];
pub const AUTO: &str = "auto";

/// Why the attempt ended up on this line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSource {
    /// The operator's `config.lines` value, used as-is.
    Configured,
    /// A line the operator picked for this one task on the recovery page.
    Manual,
    /// The configured/manual line was cooling, so a candidate further down was used.
    Fallback,
    /// `auto`: pick the fastest line that is not cooling by probing.
    AutoProbe,
}

impl LineSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Manual => "manual",
            Self::Fallback => "fallback",
            Self::AutoProbe => "auto_probe",
        }
    }
}

/// One cooling line as seen by the decision.
#[derive(Debug, Clone)]
pub struct CoolingLine {
    pub until: DateTime<Utc>,
    pub reason: Option<String>,
}

/// A decision that has not touched the network yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePlan {
    /// The line key that will actually be used (`auto` means "probe at resolve time").
    pub chosen: String,
    pub source: LineSource,
    /// The full ordered candidate sequence this plan was drawn from.
    pub candidates: Vec<String>,
    /// Human-readable reason the preferred line was not used, e.g.
    /// `alia: certificate_expired（剩余 3600 秒）`. Empty when nothing was skipped.
    pub skipped: Vec<String>,
}

impl LinePlan {
    /// Rendered reason for the page's "next line" column; `None` when nothing was skipped.
    pub fn skip_reason(&self) -> Option<String> {
        (!self.skipped.is_empty()).then(|| self.skipped.join("; "))
    }
}

/// Every line that can be named explicitly, by its `upcdn` key. `auto` is not here: it is a
/// decision, not a line.
pub fn explicit_upload_line(key: &str) -> Option<Line> {
    match key {
        "bldsa" => Some(line::bldsa()),
        "cnbldsa" => Some(line::cnbldsa()),
        "andsa" => Some(line::andsa()),
        "atdsa" => Some(line::atdsa()),
        "bda2" => Some(line::bda2()),
        "cnbd" => Some(line::cnbd()),
        "anbd" => Some(line::anbd()),
        "atbd" => Some(line::atbd()),
        "tx" => Some(line::tx()),
        "cntx" => Some(line::cntx()),
        "antx" => Some(line::antx()),
        "attx" => Some(line::attx()),
        "bda" => Some(line::bda()),
        "txa" => Some(line::txa()),
        "alia" => Some(line::alia()),
        _ => None,
    }
}

/// Build the candidate sequence: the operator's choice first, then the implicit fallbacks, with
/// `auto` last. The preferred line is never demoted by retry count — repeated failure is expressed
/// through the line's own cooldown, not by rotating away from a line the operator asked for.
///
/// With no preferred line the sequence is just `auto`: probing already skips cooling lines and
/// picks the fastest survivor, so prepending fixed guesses would only reintroduce the hardcoded
/// `bda2` head this module exists to remove.
fn candidate_sequence(preferred: Option<&str>) -> Vec<String> {
    let Some(preferred) = preferred else {
        return vec![AUTO.to_string()];
    };
    let mut candidates = Vec::with_capacity(IMPLICIT_FALLBACKS.len() + 2);
    candidates.push(preferred.to_string());
    for fallback in IMPLICIT_FALLBACKS {
        if preferred != fallback {
            candidates.push(fallback.to_string());
        }
    }
    candidates.push(AUTO.to_string());
    candidates
}

/// Normalize whatever is in `config.lines` into a candidate key, or `None` for auto/blank/unknown.
fn preferred_key(configured: &str) -> Option<&str> {
    let configured = configured.trim();
    if configured.is_empty() || configured.eq_ignore_ascii_case(AUTO) {
        return None;
    }
    explicit_upload_line(configured)
        .is_some()
        .then_some(configured)
}

/// Decide which line to use, without any network I/O.
///
/// `forced` is a per-task override from the recovery page; it takes the same "must be used unless
/// cooling" precedence as the configured line.
pub fn plan_upload_line(
    configured: &str,
    forced: Option<&str>,
    cooling: &HashMap<String, CoolingLine>,
    now: DateTime<Utc>,
) -> LinePlan {
    let forced = forced.map(str::trim).filter(|value| !value.is_empty());
    let manual = forced
        .filter(|value| explicit_upload_line(value).is_some() || value.eq_ignore_ascii_case(AUTO));
    let preferred = match manual {
        Some(value) if value.eq_ignore_ascii_case(AUTO) => None,
        Some(value) => Some(value),
        None => preferred_key(configured),
    };
    let candidates = candidate_sequence(preferred);
    let preferred_source = if manual.is_some() {
        LineSource::Manual
    } else if preferred.is_some() {
        LineSource::Configured
    } else {
        LineSource::AutoProbe
    };

    let mut skipped = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate == AUTO {
            let source = if index == 0 {
                LineSource::AutoProbe
            } else {
                LineSource::Fallback
            };
            return LinePlan {
                chosen: AUTO.to_string(),
                source,
                candidates,
                skipped,
            };
        }
        match cooling.get(candidate) {
            None => {
                let source = if index == 0 {
                    preferred_source
                } else {
                    LineSource::Fallback
                };
                return LinePlan {
                    chosen: candidate.clone(),
                    source,
                    candidates,
                    skipped,
                };
            }
            Some(cool) => {
                let remaining = (cool.until - now).num_seconds().max(0);
                let reason = cool.reason.as_deref().unwrap_or("cooldown");
                skipped.push(format!("{candidate}: {reason}（剩余 {remaining} 秒）"));
            }
        }
    }
    // `auto` is always the last candidate, so the loop above always returns.
    unreachable!("candidate sequence always ends with auto")
}

/// The concrete line an attempt will use, plus the decision that produced it.
#[derive(Clone)]
pub struct SelectedLine {
    pub line: Line,
    pub key: String,
    pub source: LineSource,
    pub plan: LinePlan,
}

impl SelectedLine {
    pub fn skip_reason(&self) -> Option<String> {
        self.plan.skip_reason()
    }
}

/// Snapshot the persistent cooldown state in the shape the planner wants.
pub async fn cooling_lines(
    pool: &ConnectionPool,
    now: DateTime<Utc>,
) -> AppResult<HashMap<String, CoolingLine>> {
    Ok(upload_line_health::active_cooldowns(pool, now)
        .await?
        .into_iter()
        .filter_map(|row| {
            row.cooldown_until.map(|until| {
                (
                    row.line_key,
                    CoolingLine {
                        until,
                        reason: row.last_failure_kind,
                    },
                )
            })
        })
        .collect())
}

/// Turn a plan into a usable line. `auto` probes, excluding cooling lines; everything else is a
/// direct construction guarded by one last `acquire_line` so a cooldown that started between the
/// plan and here still takes effect.
pub async fn resolve_planned_line(
    pool: &ConnectionPool,
    client: &reqwest::Client,
    plan: LinePlan,
) -> AppResult<(SelectedLine, Vec<ProbeFailure>)> {
    if plan.chosen != AUTO
        && let Some(line) = explicit_upload_line(&plan.chosen)
        && matches!(
            upload_line_health::acquire_line(pool, &plan.chosen, Utc::now()).await?,
            LineAvailability::Available
        )
    {
        return Ok((
            SelectedLine {
                line,
                key: plan.chosen.clone(),
                source: plan.source,
                plan,
            },
            Vec::new(),
        ));
    }
    let excluded = upload_line_health::active_cooldowns(pool, Utc::now())
        .await?
        .into_iter()
        .map(|row| row.line_key)
        .collect::<Vec<_>>();
    // 先只在有灾后取回通道的线路里探测，见 `RECOVERABLE_LINES`。
    let recoverable: Vec<String> = RECOVERABLE_LINES.iter().map(|key| key.to_string()).collect();
    let probed = match Probe::probe_filtered_with_failures(client, &recoverable, &excluded).await {
        Ok(probed) => probed,
        Err(error) => {
            // 传不上去比失去取回通道更严重，所以这里放开限制而不是失败。但要说清代价：
            // 落在其它线路上的分段，事后拿不回源文件。
            warn!(
                ?error,
                recoverable = ?RECOVERABLE_LINES,
                "可取回线路全部不可用，放开限制重新探测；本次上传的分段将没有灾后取回通道"
            );
            Probe::probe_filtered_with_failures(client, &[], &excluded)
                .await
                .map_err(|error| {
                    error_stack::Report::new(AppError::Custom(format!(
                        "no healthy upload line is currently available: {error}"
                    )))
                })?
        }
    };
    let (line, failures) = probed;
    let probe_failures = failures
        .into_iter()
        .map(|failure| ProbeFailure {
            line_key: failure.line_key,
            error: format!("{:?}", failure.error),
        })
        .collect();
    let key = line.key().to_string();
    let source = if plan.chosen == AUTO {
        plan.source
    } else {
        LineSource::Fallback
    };
    Ok((
        SelectedLine {
            line,
            key,
            source,
            plan,
        },
        probe_failures,
    ))
}

/// A line that failed during probing. Reported back to the caller instead of being recorded here,
/// so the breaker update stays in one place next to the other failure paths.
pub struct ProbeFailure {
    pub line_key: String,
    pub error: String,
}

/// Log the decision in one structured line so "which line did this attempt use, and why" is
/// answerable from the log alone.
pub fn log_line_decision(context: &str, selected: &SelectedLine, configured: &str) {
    let skipped = selected.skip_reason();
    if skipped.is_some() {
        warn!(
            context,
            configured,
            chosen = %selected.key,
            source = selected.source.as_str(),
            candidates = %selected.plan.candidates.join(","),
            skipped = ?skipped,
            "upload line fell back from the preferred line"
        );
    } else {
        info!(
            context,
            configured,
            chosen = %selected.key,
            source = selected.source.as_str(),
            candidates = %selected.plan.candidates.join(","),
            "upload line selected"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap()
    }

    fn cooling(keys: &[(&str, &str)]) -> HashMap<String, CoolingLine> {
        keys.iter()
            .map(|(key, reason)| {
                (
                    (*key).to_string(),
                    CoolingLine {
                        until: now() + Duration::hours(1),
                        reason: Some((*reason).to_string()),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_healthy_configured_line_is_always_used() {
        let plan = plan_upload_line("alia", None, &HashMap::new(), now());

        assert_eq!(plan.chosen, "alia");
        assert_eq!(plan.source, LineSource::Configured);
        assert_eq!(plan.candidates, ["alia", "bda2", "tx", "auto"]);
        assert_eq!(plan.skip_reason(), None);
    }

    #[test]
    fn bda2_is_no_longer_the_head_of_any_candidate_sequence() {
        let plan = plan_upload_line(
            "alia",
            None,
            &cooling(&[("alia", "certificate_expired")]),
            now(),
        );

        assert_eq!(plan.chosen, "bda2");
        assert_eq!(plan.source, LineSource::Fallback);
        assert_eq!(plan.candidates[0], "alia");
        assert!(
            plan.skip_reason()
                .is_some_and(|reason| reason.contains("alia: certificate_expired")),
            "a fallback must say which line it skipped and why"
        );
    }

    #[test]
    fn a_manual_line_overrides_configuration_but_still_yields_to_cooldown() {
        let honored = plan_upload_line("bda2", Some("tx"), &HashMap::new(), now());
        assert_eq!(honored.chosen, "tx");
        assert_eq!(honored.source, LineSource::Manual);

        let cooled = plan_upload_line("bda2", Some("tx"), &cooling(&[("tx", "transport")]), now());
        assert_eq!(cooled.chosen, "bda2");
        assert_eq!(cooled.source, LineSource::Fallback);
    }

    #[test]
    fn bldsa_is_never_an_implicit_candidate_but_may_be_configured() {
        let implicit = plan_upload_line("bda2", None, &HashMap::new(), now());
        assert!(!implicit.candidates.iter().any(|key| key == "bldsa"));
        assert_eq!(implicit.chosen, "bda2");
        assert_eq!(implicit.source, LineSource::Configured);

        let implicit = plan_upload_line("auto", None, &HashMap::new(), now());
        assert_eq!(implicit.chosen, AUTO);
        assert_eq!(implicit.source, LineSource::AutoProbe);

        let explicit = plan_upload_line("bldsa", None, &HashMap::new(), now());
        assert_eq!(explicit.chosen, "bldsa");
        assert_eq!(explicit.source, LineSource::Configured);
    }

    #[test]
    fn everything_cooling_falls_through_to_auto() {
        let plan = plan_upload_line(
            "alia",
            None,
            &cooling(&[
                ("alia", "certificate_expired"),
                ("bda2", "transport"),
                ("tx", "request_timeout"),
            ]),
            now(),
        );

        assert_eq!(plan.chosen, AUTO);
        assert_eq!(plan.source, LineSource::Fallback);
        assert_eq!(plan.skipped.len(), 3);
    }

    #[test]
    fn an_unknown_configured_value_degrades_to_auto_rather_than_bda2() {
        let plan = plan_upload_line("no-such-line", None, &HashMap::new(), now());

        assert_eq!(plan.chosen, AUTO);
        assert_eq!(plan.source, LineSource::AutoProbe);
        assert_eq!(plan.candidates, ["auto"]);
    }
}
