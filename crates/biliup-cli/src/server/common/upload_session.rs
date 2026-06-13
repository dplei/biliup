use crate::server::infrastructure::models::UploadSession;
use chrono::{DateTime, Utc};

/// 从候选会话中选出可续接的那条：同 room、未 finalize、updated_at 在窗口内、取最新。
/// 返回选中项在 `sessions` 中的下标，便于调用方按需取用（避免借用纠纷）。
pub fn select_recovery_candidate(
    sessions: &[UploadSession],
    room_id: i64,
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Option<usize> {
    let cutoff = now - chrono::Duration::minutes(window_minutes);
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.live_streamer_id == room_id
                && s.status != "finalized"
                && s.updated_at >= cutoff
        })
        .max_by_key(|(_, s)| s.updated_at)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn session(
        id: i64,
        room_id: i64,
        status: &str,
        updated_at: DateTime<Utc>,
    ) -> UploadSession {
        UploadSession {
            id,
            live_streamer_id: room_id,
            streamer_info_id: id,
            aid: Some(100 + id),
            bvid: None,
            videos_json: "[]".to_string(),
            status: status.to_string(),
            created_at: updated_at,
            updated_at,
        }
    }

    fn t(min_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::minutes(min_ago)
    }

    fn now_fixed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 13, 12, 0, 0).unwrap()
    }

    #[test]
    fn picks_most_recent_active_in_window() {
        let now = now_fixed();
        let sessions = vec![
            session(1, 7, "uploading", t(20, now)),
            session(2, 7, "submitted", t(5, now)), // 最新且在窗口内
            session(3, 7, "uploading", t(25, now)),
        ];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), Some(1));
    }

    #[test]
    fn skips_finalized() {
        let now = now_fixed();
        let sessions = vec![session(1, 7, "finalized", t(1, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn skips_outside_window() {
        let now = now_fixed();
        let sessions = vec![session(1, 7, "submitted", t(31, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn skips_other_room() {
        let now = now_fixed();
        let sessions = vec![session(1, 8, "submitted", t(1, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn none_when_empty() {
        let now = now_fixed();
        assert_eq!(select_recovery_candidate(&[], 7, now, 30), None);
    }
}
