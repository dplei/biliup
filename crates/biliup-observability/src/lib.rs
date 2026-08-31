//! Standalone observability with opt-in entry integration.
mod capture;
mod diagnostic;
mod model;
mod runtime;
mod sanitize;
pub mod shadow;
pub mod sqlite;

pub use capture::{CaptureLayer, legacy_output};
pub use diagnostic::{Diagnostic, DiagnosticCapture};
pub use model::{CaptureKind, Context, Draft, Event, EventData, Fields, Level, Quality, now_ms};
pub use runtime::{Commit, Consumer, Emitter, Health, Options, Runtime, StorageError};
