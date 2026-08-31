use crate::sanitize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub(crate) exit_code: Option<i32>,
    pub(crate) first_fatal: Option<String>,
    pub(crate) tail: String,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
    pub(crate) redacted: bool,
}
impl Diagnostic {
    pub fn tail(&self) -> &str {
        &self.tail
    }
    pub fn first_fatal(&self) -> Option<&str> {
        self.first_fatal.as_deref()
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Streaming line capture: memory is bounded even for stderr without a newline.
pub struct DiagnosticCapture {
    result: Diagnostic,
    line: Vec<u8>,
    overflow: bool,
}
impl Default for DiagnosticCapture {
    fn default() -> Self {
        Self::new()
    }
}
impl DiagnosticCapture {
    pub fn new() -> Self {
        Self {
            result: Diagnostic {
                exit_code: None,
                first_fatal: None,
                tail: String::new(),
                total_bytes: 0,
                truncated: false,
                redacted: false,
            },
            line: Vec::new(),
            overflow: false,
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.result.total_bytes = self.result.total_bytes.saturating_add(bytes.len() as u64);
        for &byte in bytes {
            if byte == b'\n' {
                self.finish_line();
            } else if self.line.len() < 1024 {
                self.line.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }
    fn finish_line(&mut self) {
        let raw_lower = String::from_utf8_lossy(&self.line).to_ascii_lowercase();
        let fatal = raw_lower.contains("fatal") || raw_lower.contains("error");
        let (text, redacted, truncated) = if self.overflow {
            ("[OVERSIZE LINE OMITTED]".to_owned(), false, true)
        } else {
            sanitize::clean(&String::from_utf8_lossy(&self.line), 1024)
        };
        self.result.redacted |= redacted;
        self.result.truncated |= truncated;
        if self.result.first_fatal.is_none() && fatal {
            self.result.first_fatal = Some(text.clone());
        }
        self.result.tail.push_str(&text);
        self.result.tail.push('\n');
        if self.result.tail.len() > 8192 {
            let mut cut = self.result.tail.len() - 8192;
            while !self.result.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.result.tail.drain(..cut);
            self.result.truncated = true;
        }
        self.line.clear();
        self.overflow = false;
    }
    pub fn finish(mut self, exit_code: Option<i32>) -> Diagnostic {
        if !self.line.is_empty() || self.overflow {
            self.finish_line();
        }
        self.result.exit_code = exit_code;
        self.result
    }
}
