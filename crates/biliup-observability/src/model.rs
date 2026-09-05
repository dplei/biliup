use crate::{Diagnostic, sanitize};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}
impl Level {
    pub(crate) fn index(self) -> usize {
        self as usize
    }
    pub(crate) fn from_tracing(l: &tracing::Level) -> Self {
        match *l {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            _ => Self::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Native,
    LegacyBridge,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Quality {
    pub redacted: u64,
    pub truncated: u64,
    pub rejected: u64,
}
impl Quality {
    pub(crate) fn add(&mut self, q: &Self) {
        self.redacted = self.redacted.saturating_add(q.redacted);
        self.truncated = self.truncated.saturating_add(q.truncated);
        self.rejected = self.rejected.saturating_add(q.rejected);
    }
}

/// A bounded allowlist, not a general JSON object. Unknown fields are rejected before formatting.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Fields {
    pub(crate) values: BTreeMap<String, Value>,
    pub(crate) quality: Quality,
}
impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            values: BTreeMap<String, Value>,
            quality: Quality,
        }
        let stored = Stored::deserialize(d)?;
        let mut fields = Self::new();
        for (key, value) in stored.values {
            fields.insert(&key, value);
        }
        fields.quality.add(&stored.quality);
        Ok(fields)
    }
}
pub(crate) fn field_kind(key: &str) -> Option<&'static str> {
    match key {
        "live_streamer_id"
        | "streamer_info_id"
        | "upload_session_id"
        | "segment_id"
        | "missing_id"
        | "download_attempt_id"
        | "upload_attempt_id"
        | "task_id" => Some("id"),
        "event_name" => Some("name"),
        "outcome" => Some("outcome"),
        "reason_code" => Some("reason"),
        "original_file" | "artifact_file" => Some("file"),
        "message" => Some("message"),
        "error" => Some("error"),
        "streamer_name" | "platform" | "stage" | "phase" | "line" | "command" => Some("text"),
        "previous_ms"
        | "current_ms"
        | "first_ms"
        | "last_ms"
        | "max_backward_ms"
        | "duration_ms"
        | "delay_ms"
        | "silent_ms"
        | "gap_ms"
        | "size_bytes"
        | "threshold_bytes"
        | "confirmed_bytes"
        | "updated_at_ms"
        | "total_bytes"
        | "count"
        | "pending_count"
        | "segment_order"
        | "timeout_secs"
        | "media_sequence"
        | "previous_media_sequence"
        | "missing_segments" => Some("number"),
        "exit_code" => Some("signed"),
        _ => None,
    }
}
impl Fields {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }
    pub fn quality(&self) -> &Quality {
        &self.quality
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.values.iter()
    }
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.insert(key, value.into());
        self
    }
    pub fn insert(&mut self, key: &str, value: Value) {
        let Some(kind) = field_kind(key) else {
            self.quality.rejected += 1;
            return;
        };
        // A call site that has no business id says so with an empty string. That is "unknown",
        // which the contract allows, so it stores nothing and is not counted as a rejection —
        // otherwise every standalone command would look like a stream of dropped fields.
        if kind == "id" && value.as_str().is_some_and(str::is_empty) {
            return;
        }
        let valid = match (&value, kind) {
            (Value::Number(n), "number") => n.as_u64().is_some_and(|n| n <= i64::MAX as u64),
            (Value::Number(n), "signed") => n.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
            (Value::Number(n), "id") => n.as_u64().is_some(),
            (Value::String(s), "id") => sanitize::identifier(s, 128),
            (Value::String(s), "name") => valid_name(s),
            (Value::String(s), "reason") => {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
            }
            (Value::String(s), "outcome") => [
                "executed",
                "skipped",
                "fallback",
                "failed",
                "waiting",
                "succeeded",
                "unknown",
                "recovered",
                "cancelled",
            ]
            .contains(&s.as_str()),
            (Value::String(_), "file" | "message" | "error" | "text") => true,
            (Value::Null, _) => true, // explicit clear overrides a parent, never guesses a value
            _ => false,
        };
        if !valid {
            self.quality.rejected += 1;
            return;
        }
        let value = if let Value::String(s) = value {
            let limit = match kind {
                "message" => 512,
                "error" => 1024,
                "id" => 128,
                _ => 256,
            };
            let (mut s, redacted, truncated) = sanitize::clean(&s, limit);
            self.quality.redacted += u64::from(redacted);
            self.quality.truncated += u64::from(truncated);
            if kind == "file" && !redacted {
                s = s.rsplit(['/', '\\']).next().unwrap_or_default().to_owned();
            }
            Value::String(s)
        } else if kind == "id" && value.is_number() {
            Value::String(value.to_string())
        } else {
            value
        };
        self.values.insert(key.to_owned(), value);
    }
    pub(crate) fn merge(&mut self, other: &Self) {
        self.values.extend(other.values.clone());
        self.quality.add(&other.quality);
    }
    pub(crate) fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
}

pub(crate) fn valid_name(s: &str) -> bool {
    s.len() <= 96
        && s.split_once('.').is_some_and(|(category, name)| {
            [
                "system",
                "recording",
                "processing",
                "upload",
                "submission",
                "auth",
                "audit",
            ]
            .contains(&category)
                && !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// An owned snapshot for channels, blocking workers and callbacks (no ambient thread identity).
#[derive(Debug, Clone, Default)]
pub struct Context(pub Fields);
impl Context {
    pub fn child(&self, fields: Fields) -> Self {
        let mut f = self.0.clone();
        f.merge(&fields);
        Self(f)
    }
}

pub struct Draft {
    pub name: String,
    pub message: String,
    pub context: Context,
    pub fields: Fields,
    pub diagnostic: Option<Diagnostic>,
}
impl Draft {
    pub fn new(name: &str, message: &str) -> Self {
        Self {
            name: name.into(),
            message: sanitize::prefix(message, 4096).into(),
            context: Context::default(),
            fields: Fields::new(),
            diagnostic: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventData {
    pub event_uid: String,
    pub schema_version: u32,
    pub instance_id: String,
    pub process_run_id: String,
    pub app_version: String,
    pub occurred_at_ms: i64,
    pub sequence: u64,
    pub level: Level,
    pub category: String,
    pub event_name: String,
    pub message: String,
    pub target: String,
    pub capture_kind: CaptureKind,
    pub fields: Fields,
}

/// Only the emitter can create a trusted event. Deserialized/unbounded payloads cannot be enqueued.
#[derive(Debug, Clone)]
pub struct Event {
    pub(crate) data: EventData,
    pub(crate) diagnostic: Option<Diagnostic>,
}
impl Event {
    pub fn data(&self) -> &EventData {
        &self.data
    }
    pub fn diagnostic(&self) -> Option<&Diagnostic> {
        self.diagnostic.as_ref()
    }
}
