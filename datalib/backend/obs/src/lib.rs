//! Observability wiring for every datalib Rust binary.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use indicatif::MultiProgress;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;
use tracing_indicatif::writer::IndicatifWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub use datalib_log_filter::default_filter;
pub use datalib_status_line::{shared_multi, status_line};

pub mod diagnostics;

/// `--log-format` selector. `Auto` (the default) emits pretty on a TTY,
/// JSON otherwise — exactly what you'd want from a CLI that doubles as
/// a pipeline step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    #[default]
    Auto,
    Json,
    Pretty,
}

/// Observability flags. Flatten this into your `clap::Parser` with
/// `#[command(flatten)]`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ObsArgs {
    /// Renderer for the local stderr stream. `auto` picks pretty on a
    /// TTY, JSON otherwise.
    #[arg(long, value_enum, default_value_t = LogFormat::Auto, env = "DATALIB_LOG_FORMAT")]
    pub log_format: LogFormat,

    /// `tracing-subscriber` env filter directive. Same grammar as
    /// `$RUST_LOG`, which is also honored if this flag isn't set — and
    /// which the runner sets for a step to the level its config names.
    /// Neither: `datalib_log_filter::default_filter()`.
    #[arg(long, env = "RUST_LOG")]
    pub log_level: Option<String>,

    /// OTLP/gRPC endpoint (e.g. `http://localhost:4317`). When set,
    /// spans are exported to the collector in addition to the stderr
    /// renderer. Leave empty to keep observability local.
    #[arg(long, env = "OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
}

/// Returned from [`init`]. Drop on shutdown so the OTLP batch exporter
/// gets a chance to flush before the process exits.
pub struct TracingGuard {
    provider: Option<TracerProvider>,
    multi: Arc<MultiProgress>,
}

impl TracingGuard {
    pub fn multi(&self) -> &Arc<MultiProgress> {
        &self.multi
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            if let Err(e) = p.shutdown() {
                // Process-teardown fallback: the tracing subscriber may
                // already be torn down by the time this fires, and the
                // MultiProgress is being dropped alongside us — raw
                // stderr is the only sink left.
                #[allow(clippy::disallowed_macros)]
                {
                    eprintln!("otlp shutdown: {e}");
                }
            }
        }
    }
}

pub fn init(args: &ObsArgs, service_name: &'static str) -> Result<TracingGuard> {
    let directives = args.log_level.clone().unwrap_or_else(default_filter);
    let filter = EnvFilter::try_new(&directives)
        .with_context(|| format!("parse log-level filter {directives:?}"))?;

    let use_json = match args.log_format {
        LogFormat::Json => true,
        LogFormat::Pretty => false,
        LogFormat::Auto => !std::io::stderr().is_terminal(),
    };

    // OTLP layer is optional. Build it first so the lifetime of the
    // TracerProvider lives in the guard rather than in the subscriber.
    let (otel_layer, provider) = match &args.otlp_endpoint {
        Some(endpoint) => {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .with_context(|| format!("build otlp exporter for {endpoint}"))?;
            let provider = TracerProvider::builder()
                .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                .with_resource(Resource::new(vec![KeyValue::new(
                    "service.name",
                    service_name,
                )]))
                .build();
            let tracer = provider.tracer(service_name);
            let layer = tracing_opentelemetry::layer().with_tracer(tracer);
            (Some(layer), Some(provider))
        }
        None => (None, None),
    };

    // Single `MultiProgress` shared between tracing's writer and any
    // caller-attached bars. The tracing fmt layer writes through an
    // `IndicatifWriter` that suspends this MP before each line, so log
    // emissions can't stomp on bars in either format.
    #[allow(clippy::disallowed_methods)]
    let multi = Arc::new(MultiProgress::new());
    let writer: IndicatifWriter<tracing_indicatif::writer::Stderr> =
        IndicatifWriter::new((*multi).clone());

    // Pretty vs JSON differ only in the `.json()` toggle. Each branch
    // builds its own fmt layer because the two builder chains end in
    // different concrete types that can't share a variable.
    // The diagnostics layer captures every WARN/ERROR event into the
    // ambient per-source buffer (when one is installed via
    // `diagnostics::scope`) so the sync orchestrator can fold them into
    // the per-source summary. It's a no-op on tasks without a buffer.
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(diagnostics::DiagnosticsLayer);
    if use_json {
        registry
            .with(
                // The current span says what a line is about (which
                // channel, which call); the whole ancestry on every
                // line repeated the store's path a hundred times over.
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_span_list(false)
                    .with_writer(writer)
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_target(true),
            )
            .try_init()
            .context("install tracing subscriber")?;
    } else {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_target(true),
            )
            .try_init()
            .context("install tracing subscriber")?;
    }

    // Publish the shared MultiProgress so call sites in other crates
    // can grab it via `datalib_obs::shared_multi()` without
    // threading the `TracingGuard` through their call chain. Second
    // and later inits are no-ops (the first wins) — relevant only
    // in tests that exercise `init` more than once per process.
    datalib_status_line::set_shared_multi(multi.clone());

    Ok(TracingGuard { provider, multi })
}
