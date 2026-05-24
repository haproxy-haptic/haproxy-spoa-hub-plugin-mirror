use std::sync::OnceLock;

use haproxy_spoa_hub_plugin_api::{MetricRecorder, ProcessingResult, define_plugin};
use tokio::runtime::Runtime;

use crate::client::MirrorClient;
use crate::config::PluginConfig;
use crate::request::extract_mirror_request;

/// Shared plugin state, initialised once in `init()` and read from every
/// `process()` call. `OnceLock` matches the fingerprinting plugin's
/// convention.
struct PluginState {
    config: PluginConfig,
    runtime: Runtime,
    client: MirrorClient,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// Plugin-scoped metric recorder. The `define_plugin!` `metrics_static`
/// arm below binds the hub's v2 recorder here once between `create`
/// and `init`. The dispatcher uses this from every code path. With a
/// v1 hub (pre-vtable-v2) the install never fires and emissions are
/// silent no-ops, so the plugin remains backward-compatible.
pub(crate) static METRICS: MetricRecorder = MetricRecorder::new();

#[derive(Debug)]
struct MirrorPlugin;

define_plugin!(MirrorPlugin, {
    fn new() -> Self {
        MirrorPlugin
    }

    fn init(
        &mut self,
        context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config = PluginConfig::from_context(context);

        // Dedicated multi-thread runtime so reqwest's async stack runs
        // off the SPOA worker. Worker count comes from `worker_threads`
        // when the operator sets it, otherwise from
        // `available_parallelism()` floored at 2 (see PluginConfig::
        // resolved_worker_threads for the rationale). This avoids the
        // single-worker self-deadlock on tight-CPU-limited sidecars
        // while scaling up on hosts with real cores to spare.
        let worker_threads = config.resolved_worker_threads();
        log::debug!(
            target: "mirror",
            "tokio runtime: {worker_threads} worker threads, pool_max_idle_per_host={}",
            config.pool_max_idle_per_host
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name("mirror-plugin")
            .enable_all()
            .build()?;

        let client = MirrorClient::new(&config)?;

        // OnceLock::set rejects on second-init; that's harmless here
        // because the hub guarantees init() is called at most once per
        // load. The .ok() drops the returning state on the rare double-
        // init path (e.g. plugin reload), preventing a spurious leak.
        let _ = STATE.set(PluginState {
            config,
            runtime,
            client,
        });
        Ok(())
    }

    fn name(&self) -> &str {
        "mirror"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        let state = STATE
            .get()
            .ok_or("mirror plugin: process() called before init()")?;

        if let Some(req) = extract_mirror_request(message, state.config.max_body_bytes) {
            let client = state.client.clone();
            // Fire-and-forget: dispatch returns immediately on a saturated
            // semaphore, otherwise sends the HTTP request and discards
            // the response. The hub ACKs back to HAProxy without waiting.
            state.runtime.spawn(async move {
                client.dispatch(req).await;
            });
        }
        // Mirror plugin never sets session variables — its effect is
        // the dispatched request, not data flowing back to HAProxy.
        Ok(ProcessingResult::empty())
    }

    fn config_schema(&self) -> Option<&str> {
        Some(PluginConfig::config_schema())
    }

    fn drain(&self, timeout_ms: u64) -> bool {
        // Hub calls this between the registry swap and shutdown_all
        // on a config reload. Wait for the fire-and-forget HTTP
        // dispatches spawned by `process()` to complete before
        // letting the runtime drop — without this, the spawned
        // tasks get cancelled by the runtime's Drop and the mirror
        // requests never land at their destinations. That cancel-
        // mid-flight pattern is what surfaced as
        // HTTPRouteRequestMultipleMirrors flakes in haproxy-haptic
        // CI (haproxy-spoa-hub#3).
        //
        // Spin-wait with a 5ms poll interval. 5ms gives a 1000ms
        // drain budget ~200 polls (plenty of resolution against
        // mirror dispatches that typically complete in 1-50ms),
        // and is short enough not to pin a core while waiting.
        let Some(state) = STATE.get() else {
            // init() never ran or the static never populated —
            // nothing to drain.
            return true;
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if state.client.in_flight() == 0 {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                // Final read in case a dispatch completed
                // concurrently with the deadline check.
                let final_count = state.client.in_flight();
                if final_count == 0 {
                    return true;
                }
                log::warn!(
                    target: "mirror",
                    "drain timeout ({} ms) hit with {} in-flight dispatches still pending; \
                     the hub will force shutdown and those requests will not land",
                    timeout_ms, final_count
                );
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    metrics_static = METRICS;
});
