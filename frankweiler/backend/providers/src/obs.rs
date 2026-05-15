//! Observability wiring for provider CLIs.
//!
//! One entry point — [`init`] — that builds a `tracing` subscriber with
//! the rendering layer chosen by environment, plus an optional OTLP
//! exporter. The intent is that every CLI in this crate writes the same
//! structured events and lets the renderer be one of N consumers:
//!
//!   * TTY on stderr → [`tracing_indicatif`] progress bars (one per span
//!     that opts in via `indicatif.pb_show`), plus pretty-printed events.
//!   * Non-TTY (pipes, CI, container logs) → newline-delimited JSON on
//!     stderr. The pipeline orchestrator scrapes this stream.
//!   * `--otlp-endpoint <url>` (or `$OTLP_ENDPOINT`) → also export spans
//!     to an OTLP/gRPC collector. Cheap to leave off; pays for itself
//!     the first time you want a dashboard that isn't `grep`.
//!
//! See [`events`] for the shared event vocabulary — emit these via the
//! provided helpers so the field set stays consistent across renderers.
//!
//! Drop-in usage from a CLI:
//!
//! ```ignore
//! #[derive(clap::Parser)]
//! struct Args {
//!     #[command(flatten)]
//!     obs: frankweiler_providers::obs::ObsArgs,
//! }
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let args = <Args as clap::Parser>::parse();
//!     let _guard = frankweiler_providers::obs::init(&args.obs, "slack-download")?;
//!     // ... work ...
//!     Ok(())
//! }
//! ```

use std::io::IsTerminal;

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// `--log-format` selector. `Auto` (the default) emits pretty + progress
/// bars on a TTY, JSON otherwise — exactly what you'd want from a CLI
/// that doubles as a pipeline step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    #[default]
    Auto,
    Json,
    Pretty,
}

/// Observability flags. Flatten this into your `clap::Parser` with
/// `#[command(flatten)]`.
#[derive(Debug, Clone, clap::Args)]
pub struct ObsArgs {
    /// Renderer for the local stderr stream. `auto` picks pretty +
    /// `tracing_indicatif` progress bars on a TTY, JSON otherwise.
    #[arg(long, value_enum, default_value_t = LogFormat::Auto, env = "FW_LOG_FORMAT")]
    pub log_format: LogFormat,

    /// `tracing-subscriber` env filter directive. Same grammar as
    /// `$RUST_LOG`, which is also honored if this flag isn't set.
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// OTLP/gRPC endpoint (e.g. `http://localhost:4317`). When set,
    /// spans are exported to the collector in addition to the stderr
    /// renderer. Leave empty to keep observability local.
    #[arg(long, env = "OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
}

/// Returned from [`init`]. Drop on shutdown so the OTLP batch exporter
/// gets a chance to flush before the process exits. Doing this through
/// a guard rather than asking callers to remember a `shutdown()` call
/// is the convention `tracing-opentelemetry`'s docs recommend.
pub struct TracingGuard {
    provider: Option<TracerProvider>,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            // shutdown() returns Result; we're in Drop so we can't
            // propagate. Print on failure so a dropped span isn't
            // silently lost.
            if let Err(e) = p.shutdown() {
                eprintln!("otlp shutdown: {e}");
            }
        }
    }
}

/// Initialize the global tracing subscriber. Call exactly once near the
/// top of `main`. `service_name` becomes the OTLP service.name resource
/// attribute and shows up as the span scope on traces.
pub fn init(args: &ObsArgs, service_name: &'static str) -> Result<TracingGuard> {
    let filter = EnvFilter::try_new(&args.log_level)
        .with_context(|| format!("parse log-level filter {:?}", args.log_level))?;

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

    // Local renderer. `indicatif` only makes sense on a TTY — on JSON
    // mode we just emit events, no bars.
    let registry = tracing_subscriber::registry().with(filter).with(otel_layer);

    if use_json {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr),
            )
            .try_init()
            .context("install tracing subscriber")?;
    } else {
        let indicatif = IndicatifLayer::new();
        let writer = indicatif.get_stderr_writer();
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_target(false),
            )
            .with(indicatif)
            .try_init()
            .context("install tracing subscriber")?;
    }

    Ok(TracingGuard { provider })
}

/// Event vocabulary shared across provider CLIs. Helpers emit
/// `tracing::info!` with a stable field set so JSON consumers and OTLP
/// dashboards see the same shape. Keep this list short — every event
/// added here becomes a long-term commitment to downstream consumers.
pub mod events {
    /// One HTTP-ish call to the upstream provider completed. Emitted at
    /// `debug` because progress bars carry the cumulative counters; the
    /// per-call detail is only interesting when something looks wrong.
    pub fn item_fetched(url: &str, bytes: u64, duration_ms: u64) {
        tracing::debug!(
            event = "item_fetched",
            url = url,
            bytes = bytes,
            duration_ms = duration_ms,
        );
    }

    /// A batch of records was diffed against prior state and persisted.
    pub fn indexed_batch(entity: &str, count: usize, duration_ms: u64) {
        tracing::info!(
            event = "indexed_batch",
            entity = entity,
            count = count,
            duration_ms = duration_ms,
        );
    }
}
