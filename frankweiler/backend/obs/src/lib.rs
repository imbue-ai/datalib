//! Observability wiring for every frankweiler Rust binary.
//!
//! One entry point — [`init`] — that builds a `tracing` subscriber, plus
//! an optional OTLP exporter and a shared `MultiProgress` for progress
//! bars. The fmt layer writes through an [`IndicatifWriter`] tied to
//! that `MultiProgress`, so every log emission suspends bar draws before
//! writing. Callers attach their bars to the same `MultiProgress`
//! (via [`shared_multi`] or [`TracingGuard::multi`]) so logs never stomp
//! a bar.
//!
//! Two log formats:
//!   * `pretty` — human-readable, one line per event. Default on TTY.
//!   * `json` — newline-delimited JSON on stderr. Default off-TTY.
//!
//! And optional OTLP export: `--otlp-endpoint <url>` (or `$OTLP_ENDPOINT`)
//! ships spans to an OTLP/gRPC collector in addition to local rendering.
//!
//! There are no automatic per-span progress bars. Bars are created
//! explicitly by callers (e.g. per-source bars
//! attached to [`shared_multi`]).
//!
//! Drop-in usage from a CLI:
//!
//! ```ignore
//! #[derive(clap::Parser)]
//! struct Args {
//!     #[command(flatten)]
//!     obs: frankweiler_obs::ObsArgs,
//! }
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let args = <Args as clap::Parser>::parse();
//!     let _guard = frankweiler_obs::init(&args.obs, "slack-download")?;
//!     // ... work ...
//!     Ok(())
//! }
//! ```

use std::io::IsTerminal;
use std::sync::{Arc, OnceLock};

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
#[derive(Debug, Clone, clap::Args)]
pub struct ObsArgs {
    /// Renderer for the local stderr stream. `auto` picks pretty on a
    /// TTY, JSON otherwise.
    #[arg(long, value_enum, default_value_t = LogFormat::Auto, env = "FW_LOG_FORMAT")]
    pub log_format: LogFormat,

    /// `tracing-subscriber` env filter directive. Same grammar as
    /// `$RUST_LOG`, which is also honored if this flag isn't set.
    #[arg(
        long,
        env = "RUST_LOG",
        default_value = "info,sqlx=warn,hyper=warn,html5ever=error"
    )]
    pub log_level: String,

    /// OTLP/gRPC endpoint (e.g. `http://localhost:4317`). When set,
    /// spans are exported to the collector in addition to the stderr
    /// renderer. Leave empty to keep observability local.
    #[arg(long, env = "OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
}

impl Default for ObsArgs {
    fn default() -> Self {
        Self {
            log_format: LogFormat::default(),
            log_level: "info,sqlx=warn,hyper=warn,html5ever=error".into(),
            otlp_endpoint: None,
        }
    }
}

/// Returned from [`init`]. Drop on shutdown so the OTLP batch exporter
/// gets a chance to flush before the process exits.
///
/// Holds a clone of the shared `MultiProgress`. Callers that want
/// progress bars coordinated with the tracing writer should pull
/// [`multi`](Self::multi) and attach their bars to it (and route any
/// status `eprintln!`-style output through `multi.println(...)`).
pub struct TracingGuard {
    provider: Option<TracerProvider>,
    multi: Arc<MultiProgress>,
}

impl TracingGuard {
    /// The shared `MultiProgress` whose draws are suspended by every
    /// tracing log emission. Attach all interactive progress bars here
    /// so they don't fight with log lines.
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

    // Single `MultiProgress` shared between tracing's writer and any
    // caller-attached bars. The tracing fmt layer writes through an
    // `IndicatifWriter` that suspends this MP before each line, so log
    // emissions can't stomp on bars in either format.
    //
    // `IndicatifWriter::new` takes a `MultiProgress` by value, but the
    // type is internally a cheap `Arc`-cloneable handle. Cloning here
    // gives the writer its own handle while we keep an outer `Arc` so
    // callers can attach bars via [`shared_multi`] / [`TracingGuard::multi`].
    // The one legitimate construction in the workspace. Everywhere
    // else pulls this same MP via `shared_multi()`; the clippy
    // `disallowed-methods` entry in `clippy.toml` enforces that.
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
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer)
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true)
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
                    .with_target(true),
            )
            .try_init()
            .context("install tracing subscriber")?;
    }

    // Publish the shared MultiProgress so call sites in other crates
    // can grab it via `frankweiler_obs::shared_multi()` without
    // threading the `TracingGuard` through their call chain. Second
    // and later inits are no-ops (the first wins) — relevant only
    // in tests that exercise `init` more than once per process.
    let _ = SHARED_MULTI.set(multi.clone());

    Ok(TracingGuard { provider, multi })
}

static SHARED_MULTI: OnceLock<Arc<MultiProgress>> = OnceLock::new();

/// The process-wide [`MultiProgress`] published by [`init`]. Returns
/// `None` before `init` runs (e.g. in unit tests that don't initialize
/// observability) — callers should fall back to a local `MultiProgress`
/// or simply skip rendering bars in that case.
pub fn shared_multi() -> Option<Arc<MultiProgress>> {
    SHARED_MULTI.get().cloned()
}

/// Write a one-line status message that coexists with the indicatif
/// progress bars. Routes through the shared `MultiProgress::println`
/// when bars can actually draw (stderr is a TTY), which suspends draws
/// across the write so the line lands above the bar block instead of
/// overprinting it. Falls back to `eprintln!` before `init` runs, in
/// tests that skip observability, and — crucially — whenever the draw
/// target is hidden (stderr piped/redirected): `MultiProgress::println`
/// is documented to *do nothing* in that case, which once swallowed
/// every status line (including the sync phase markers the http worker
/// scrapes) when the pipeline ran as a child process. Same
/// `format!` argument grammar as `eprintln!`.
///
/// Use this for any user-facing status line that can fire while bars
/// are on screen (download / render / synth phases, the SIGINT
/// handler, error summaries, end-of-run banners from `qmd_indexer`,
/// etc.). Plain `tracing::info!` / `warn!` / `error!` already go
/// through the `IndicatifWriter` and do not need this macro.
///
/// Enforced by `disallowed-macros` in `frankweiler/backend/clippy.toml`:
/// direct `std::eprintln!` / `std::println!` in production code is
/// banned in favor of this macro (or `tracing::*` for log events).
#[macro_export]
macro_rules! status_line {
    ($($arg:tt)*) => {{
        let __msg = ::std::format!($($arg)*);
        match $crate::shared_multi() {
            // `MultiProgress::println` is documented to do *nothing*
            // when the draw target is hidden — which is exactly the
            // off-TTY case (stderr piped, e.g. spawned by the http
            // worker). Only route through it when it will actually
            // draw; otherwise fall through to raw stderr so status
            // lines (incl. the sync phase markers) reach the pipe.
            Some(mp) if !mp.is_hidden() => { let _ = mp.println(&__msg); }
            // Pre-`init` (tracing not up yet), hidden draw target, and
            // out-of-band fallback. The lint exception is part of the
            // macro's contract.
            _ => {
                #[allow(clippy::disallowed_macros)]
                { ::std::eprintln!("{}", __msg); }
            }
        }
    }}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `status_line!` must not route through `MultiProgress::println`
    /// when the draw target is hidden — indicatif documents that call
    /// as a no-op, so taking that branch silently swallows the line.
    /// Off a TTY (child process with piped stderr, CI, `bazel test`)
    /// a stderr-targeted `MultiProgress` *is* hidden, so the macro's
    /// `!mp.is_hidden()` guard is what keeps the sync phase markers
    /// flowing to the http worker's pipe.
    #[test]
    fn stderr_multi_is_hidden_off_tty() {
        if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            // Interactive run: the premise doesn't hold; nothing to check.
            return;
        }
        // Not the process-wide MP — this test constructs a throwaway
        // one purely to observe indicatif's TTY detection.
        #[allow(clippy::disallowed_methods)]
        let mp = MultiProgress::new();
        assert!(
            mp.is_hidden(),
            "stderr-targeted MultiProgress should be hidden off-TTY; \
             if this changes, revisit the status_line! routing guard"
        );
    }

    /// Explicitly-hidden target, TTY-independent: `println` reports Ok
    /// while drawing nothing, which is exactly why the macro must not
    /// treat `Some(mp)` alone as "safe to route through".
    #[test]
    fn hidden_multi_println_is_a_silent_no_op() {
        let mp = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        assert!(mp.is_hidden());
        assert!(mp.println("dropped on the floor").is_ok());
    }
}
