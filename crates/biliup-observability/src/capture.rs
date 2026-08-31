use crate::{CaptureKind, Context, Draft, Emitter, Fields, Level, model, sanitize};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context as LayerContext, registry::LookupSpan};

/// Use as a per-layer filter on every old sink in P2, never as a registry-wide filter.
pub fn legacy_output(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() != "biliup::event"
}
#[derive(Clone)]
pub struct CaptureLayer {
    emitter: Emitter,
}
impl CaptureLayer {
    pub fn new(emitter: Emitter) -> Self {
        Self { emitter }
    }
    pub(crate) fn emitter(&self) -> Emitter {
        self.emitter.clone()
    }
    /// Independent dynamic per-layer filtering. Disabled capture does not visit/format fields.
    pub fn filtered<S>(self) -> impl Layer<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let emitter = self.emitter.clone();
        self.with_filter(tracing_subscriber::filter::dynamic_filter_fn(
            move |meta, _| {
                if meta.is_span() {
                    return emitter.enabled(Level::Error);
                }
                emitter.enabled(Level::from_tracing(meta.level()))
                    && !meta.target().starts_with("sqlx")
                    && !meta.target().starts_with("biliup_observability")
                    && (meta.target() == "biliup::event" || emitter.bridge_enabled())
            },
        ))
    }
}
#[derive(Default)]
struct SpanFields(Fields);
struct Visitor<'a>(&'a mut Fields);
impl Visit for Visitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Bound before allocating an owned value. Files are handled by Fields after sanitizing.
        let prefix = sanitize::prefix(value, 4096);
        self.0.insert(field.name(), prefix.into());
        if prefix.len() < value.len() {
            self.0.quality.truncated += 1;
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if model::field_kind(field.name()).is_none() {
            self.0.quality.rejected += 1;
            return;
        }
        let result = sanitize::debug(value);
        self.0.quality.truncated += u64::from(result.truncated);
        self.0.insert(field.name(), result.text.into());
    }
}
impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Fields::new();
            attrs.record(&mut Visitor(&mut fields));
            span.extensions_mut().insert(SpanFields(fields));
        }
    }
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: LayerContext<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<SpanFields>()
        {
            values.record(&mut Visitor(&mut fields.0));
        }
    }
    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let mut fields = Fields::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(s) = span.extensions().get::<SpanFields>() {
                    fields.merge(&s.0);
                }
            }
        }
        capture_event(&self.emitter, event, fields);
    }
}

pub(crate) fn capture_event(emitter: &Emitter, event: &Event<'_>, mut fields: Fields) {
    let meta = event.metadata();
    let level = Level::from_tracing(meta.level());
    if !emitter.enabled(level)
        || meta.target().starts_with("sqlx")
        || meta.target().starts_with("biliup_observability")
    {
        return;
    }
    let native = meta.target() == "biliup::event";
    if !native && !emitter.bridge_enabled() {
        return;
    }
    event.record(&mut Visitor(&mut fields));
    let name = if native {
        fields.text("event_name").unwrap_or("")
    } else {
        "system.legacy"
    };
    let draft = Draft {
        name: name.into(),
        message: fields.text("message").unwrap_or("事件诊断").into(),
        context: Context::default(),
        fields,
        diagnostic: None,
    };
    match emitter.create_inner(
        level,
        draft,
        if native {
            CaptureKind::Native
        } else {
            CaptureKind::LegacyBridge
        },
        meta.target(),
        None,
        None,
    ) {
        Ok(event) => {
            emitter.submit(event);
        }
        Err(_) => emitter.reject(level),
    }
}
