use std::fmt::{self, Write};

pub(crate) fn prefix(value: &str, bytes: usize) -> &str {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(crate) fn clean(value: &str, limit: usize) -> (String, bool, bool) {
    let text = prefix(value, limit);
    let lower = text.to_ascii_lowercase();
    let redacted = [
        "cookie",
        "authorization",
        "token",
        "secret",
        "password",
        "credential",
        "bearer",
        "signature",
        "sign=",
        "http:",
        "https:",
        "://",
        "access_key",
        "api_key",
        // Legacy upload callers debug-print the entire remote response, including account
        // archive identifiers. Retain neither that body nor its numeric Debug wrappers.
        "responsedata",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let result = if redacted {
        "[REDACTED]".into()
    } else {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    };
    (result, redacted, text.len() < value.len())
}

pub(crate) fn identifier(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

// Returning fmt::Error stops standard Debug/Display implementations at the byte limit.
pub(crate) struct Bounded {
    pub text: String,
    pub truncated: bool,
}
impl Write for Bounded {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let p = prefix(s, 1024usize.saturating_sub(self.text.len()));
        self.text.push_str(p);
        if p.len() < s.len() {
            self.truncated = true;
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}
pub(crate) fn debug(value: &dyn fmt::Debug) -> Bounded {
    let mut out = Bounded {
        text: String::new(),
        truncated: false,
    };
    let _ = write!(&mut out, "{value:?}");
    out
}
