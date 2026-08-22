//! OpenFang daemon server — boots the kernel and serves the HTTP API.

use crate::channel_bridge;
use crate::middleware;
use crate::rate_limiter;
use crate::routes::{self, AppState};
use crate::webchat;
use crate::ws;
use axum::Router;
use openfang_kernel::OpenFangKernel;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

/// How long the daemon lets in-flight HTTP requests finish after a shutdown
/// signal before it stops waiting for them and proceeds with its own exit path.
/// It does not close them — see the WARN on the timeout branch.
///
/// This is a budget, not a preference. Everything the daemon does on the way
/// out happens *after* `axum::serve(..).with_graceful_shutdown(..).await`
/// returns, and that await has no bound of its own: it waits for every
/// outstanding request. Two of this server's endpoints can outlast any
/// supervisor's patience — `POST /api/channels/reload` blocks while the channel
/// bridge drains an in-flight Telegram `getUpdates` (up to ~30 s), and the agent
/// WebSocket lives as long as its client. Waiting for those means the exit path
/// never runs and the process is killed instead of stopping.
///
/// 3 s leaves room inside the 10 s that `docker stop` and systemd both give by
/// default for the work that follows: `BridgeManager::stop_fast`
/// (`ADAPTER_STOP_TIMEOUT_FAST`, 250 ms per adapter) and `kernel.shutdown()`.
/// Override with `OPENFANG_SHUTDOWN_DRAIN_SECS` when a deployment gives the
/// process a different grace period.
const SHUTDOWN_DRAIN_TIMEOUT_DEFAULT: std::time::Duration = std::time::Duration::from_secs(3);

/// Daemon info written to `~/.openfang/daemon.json` so the CLI can find us.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub listen_addr: String,
    pub started_at: String,
    pub version: String,
    pub platform: String,
}

/// Build the full API router with all routes, middleware, and state.
///
/// This is extracted from `run_daemon()` so that embedders (e.g. openfang-desktop)
/// can create the router without starting the full daemon lifecycle.
///
/// Returns `(router, shared_state)`. The caller can use `state.bridge_manager`
/// to shut down the bridge on exit.
pub async fn build_router(
    kernel: Arc<OpenFangKernel>,
    listen_addr: SocketAddr,
) -> (Router<()>, Arc<AppState>) {
    // Start channel bridges (Telegram, etc.)
    let bridge = channel_bridge::start_channel_bridge(kernel.clone()).await;

    let passkey_auth = if kernel.config.auth.enabled {
        Some(Arc::new(
            crate::passkey_auth::PasskeyAuthService::new(
                &kernel.config.auth,
                kernel.memory.dashboard_auth().clone(),
            )
            .unwrap_or_else(|error| panic!("Invalid passkey auth configuration: {error}")),
        ))
    } else {
        None
    };

    let channels_config = kernel.config.channels.clone();
    let state = Arc::new(AppState {
        kernel: kernel.clone(),
        passkey_auth: passkey_auth.clone(),
        started_at: Instant::now(),
        peer_registry: kernel.peer_registry.get().map(|r| Arc::new(r.clone())),
        bridge_manager: tokio::sync::Mutex::new(bridge),
        channels_config: tokio::sync::RwLock::new(channels_config),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(kernel.config.budget.clone())),
    });

    // Start WS cron broadcaster — subscribes to kernel event bus and pushes
    // cron job results to all connected WebSocket clients in real-time.
    ws::start_ws_cron_broadcaster(kernel.clone());

    // Passkey mode accepts exactly its configured HTTPS origin. Development
    // mode retains the existing loopback-only CORS convenience.
    let cors = if let Some(service) = passkey_auth.as_ref() {
        let origin: axum::http::HeaderValue = service
            .rp_origin()
            .parse()
            .expect("validated passkey RP origin must be a valid header");
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
            .allow_credentials(true)
    } else if state.kernel.config.api_key.trim().is_empty() {
        // No auth → restrict CORS to localhost origins (include both 127.0.0.1 and localhost)
        let port = listen_addr.port();
        let mut origins: Vec<axum::http::HeaderValue> = vec![
            format!("http://{listen_addr}").parse().unwrap(),
            format!("http://localhost:{port}").parse().unwrap(),
        ];
        // Also allow common dev ports
        for p in [3000u16, 8080] {
            if p != port {
                if let Ok(v) = format!("http://127.0.0.1:{p}").parse() {
                    origins.push(v);
                }
                if let Ok(v) = format!("http://localhost:{p}").parse() {
                    origins.push(v);
                }
            }
        }
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    } else {
        // Auth enabled → restrict CORS to localhost + configured origins.
        // SECURITY: CorsLayer::permissive() is dangerous — any website could
        // make cross-origin requests. Restrict to known origins instead.
        let mut origins: Vec<axum::http::HeaderValue> = vec![
            format!("http://{listen_addr}").parse().unwrap(),
            "http://localhost:4200".parse().unwrap(),
            "http://127.0.0.1:4200".parse().unwrap(),
            "http://localhost:8080".parse().unwrap(),
            "http://127.0.0.1:8080".parse().unwrap(),
        ];
        // Add the actual listen address variants
        if listen_addr.port() != 4200 && listen_addr.port() != 8080 {
            if let Ok(v) = format!("http://localhost:{}", listen_addr.port()).parse() {
                origins.push(v);
            }
            if let Ok(v) = format!("http://127.0.0.1:{}", listen_addr.port()).parse() {
                origins.push(v);
            }
        }
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    };

    // Trim whitespace so `api_key = ""` or `api_key = "  "` both disable auth.
    let api_key = state.kernel.config.api_key.trim().to_string();
    let allow_no_auth = std::env::var("OPENFANG_ALLOW_NO_AUTH")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false);

    // Fail-closed warning: if no api_key and no dashboard auth, and the
    // server is bound to a non-loopback address without an explicit opt-in,
    // shout about it. The middleware will reject non-loopback traffic.
    let bind_is_loopback = listen_addr.ip().is_loopback();
    if api_key.is_empty() && !state.kernel.config.auth.enabled && !bind_is_loopback {
        if allow_no_auth {
            tracing::warn!(
                "OPENFANG_ALLOW_NO_AUTH=1 is set. Running WITHOUT authentication on {}. \
                 Anyone reachable at this address can read/write agents, channels, and keys.",
                listen_addr
            );
        } else {
            tracing::warn!(
                "No api_key configured and server is bound to {} (non-loopback). \
                 Non-loopback requests will be rejected with 401. \
                 Set OPENFANG_API_KEY (or api_key in config.toml), or bind to 127.0.0.1, \
                 or set OPENFANG_ALLOW_NO_AUTH=1 to explicitly run open.",
                listen_addr
            );
        }
    }

    let auth_state = crate::middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        passkey_auth: passkey_auth.clone(),
        allow_no_auth,
    };
    let gcra_limiter = rate_limiter::create_rate_limiter();

    let app = Router::new()
        .route("/", axum::routing::get(webchat::webchat_page))
        .route("/login", axum::routing::get(webchat::login_page))
        .route("/register", axum::routing::get(webchat::register_page))
        .route("/logo.png", axum::routing::get(webchat::logo_png))
        .route("/favicon.ico", axum::routing::get(webchat::favicon_ico))
        .route("/manifest.json", axum::routing::get(webchat::manifest_json))
        .route("/sw.js", axum::routing::get(webchat::sw_js))
        .route(
            "/api/metrics",
            axum::routing::get(routes::prometheus_metrics),
        )
        .route("/api/health", axum::routing::get(routes::health))
        .route(
            "/api/health/detail",
            axum::routing::get(routes::health_detail),
        )
        .route("/api/status", axum::routing::get(routes::status))
        .route("/api/version", axum::routing::get(routes::version))
        .route(
            "/api/agents",
            axum::routing::get(routes::list_agents).post(routes::spawn_agent),
        )
        .route(
            "/api/agents/{id}",
            axum::routing::get(routes::get_agent)
                .delete(routes::kill_agent)
                .patch(routes::patch_agent),
        )
        .route(
            "/api/agents/{id}/uninstall",
            axum::routing::delete(routes::uninstall_agent),
        )
        .route(
            "/api/agents/{id}/mode",
            axum::routing::put(routes::set_agent_mode),
        )
        .route("/api/profiles", axum::routing::get(routes::list_profiles))
        .route(
            "/api/agents/{id}/restart",
            axum::routing::post(routes::restart_agent),
        )
        .route(
            "/api/agents/{id}/start",
            axum::routing::post(routes::restart_agent),
        )
        .route(
            // Issue #890 — alias so dashboards and external orchestrators can
            // wake an inactive agent via a verb that matches the agent_activate tool.
            "/api/agents/{id}/activate",
            axum::routing::post(routes::restart_agent),
        )
        .route(
            "/api/agents/{id}/message",
            axum::routing::post(routes::send_message),
        )
        .route(
            "/api/agents/{id}/message/stream",
            axum::routing::post(routes::send_message_stream),
        )
        .route(
            "/api/agents/{id}/session",
            axum::routing::get(routes::get_agent_session),
        )
        .route(
            "/api/agents/{id}/sessions",
            axum::routing::get(routes::list_agent_sessions).post(routes::create_agent_session),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}/switch",
            axum::routing::post(routes::switch_agent_session),
        )
        .route(
            "/api/agents/{id}/session/reset",
            axum::routing::post(routes::reset_session),
        )
        .route(
            "/api/agents/{id}/history",
            axum::routing::delete(routes::clear_agent_history),
        )
        .route(
            "/api/agents/{id}/session/compact",
            axum::routing::post(routes::compact_session),
        )
        .route(
            "/api/agents/{id}/stop",
            axum::routing::post(routes::stop_agent),
        )
        .route(
            "/api/agents/{id}/model",
            axum::routing::put(routes::set_model),
        )
        .route(
            "/api/agents/{id}/tools",
            axum::routing::get(routes::get_agent_tools).put(routes::set_agent_tools),
        )
        .route(
            "/api/agents/{id}/skills",
            axum::routing::get(routes::get_agent_skills).put(routes::set_agent_skills),
        )
        .route(
            "/api/agents/{id}/mcp_servers",
            axum::routing::get(routes::get_agent_mcp_servers).put(routes::set_agent_mcp_servers),
        )
        .route(
            "/api/agents/{id}/identity",
            axum::routing::patch(routes::update_agent_identity),
        )
        .route(
            "/api/agents/{id}/config",
            axum::routing::patch(routes::patch_agent_config),
        )
        .route(
            "/api/agents/{id}/clone",
            axum::routing::post(routes::clone_agent),
        )
        .route(
            "/api/agents/{id}/files",
            axum::routing::get(routes::list_agent_files),
        )
        .route(
            "/api/agents/{id}/files/{filename}",
            axum::routing::get(routes::get_agent_file).put(routes::set_agent_file),
        )
        .route(
            "/api/agents/{id}/deliveries",
            axum::routing::get(routes::get_agent_deliveries),
        )
        .route(
            "/api/agents/{id}/upload",
            axum::routing::post(routes::upload_file),
        )
        .route("/api/agents/{id}/ws", axum::routing::get(ws::agent_ws))
        // Upload serving
        .route(
            "/api/uploads/{file_id}",
            axum::routing::get(routes::serve_upload),
        )
        // Channel endpoints
        .route("/api/channels", axum::routing::get(routes::list_channels))
        .route(
            "/api/channels/{name}/configure",
            axum::routing::post(routes::configure_channel).delete(routes::remove_channel),
        )
        .route(
            "/api/channels/{name}/test",
            axum::routing::post(routes::test_channel),
        )
        .route(
            "/api/channels/reload",
            axum::routing::post(routes::reload_channels),
        )
        // WhatsApp QR login flow
        .route(
            "/api/channels/whatsapp/qr/start",
            axum::routing::post(routes::whatsapp_qr_start),
        )
        .route(
            "/api/channels/whatsapp/qr/status",
            axum::routing::get(routes::whatsapp_qr_status),
        )
        // Template endpoints
        .route("/api/templates", axum::routing::get(routes::list_templates))
        .route(
            "/api/templates/{name}",
            axum::routing::get(routes::get_template),
        )
        // Memory endpoints
        .route(
            "/api/memory/agents/{id}/kv",
            axum::routing::get(routes::get_agent_kv),
        )
        .route(
            "/api/memory/agents/{id}/kv/{key}",
            axum::routing::get(routes::get_agent_kv_key)
                .put(routes::set_agent_kv_key)
                .delete(routes::delete_agent_kv_key),
        )
        // Trigger endpoints
        .route(
            "/api/triggers",
            axum::routing::get(routes::list_triggers).post(routes::create_trigger),
        )
        .route(
            "/api/triggers/{id}",
            axum::routing::delete(routes::delete_trigger).put(routes::update_trigger),
        )
        // Schedule (cron job) endpoints
        .route(
            "/api/schedules",
            axum::routing::get(routes::list_schedules).post(routes::create_schedule),
        )
        .route(
            "/api/schedules/{id}",
            axum::routing::delete(routes::delete_schedule).put(routes::update_schedule),
        )
        .route(
            "/api/schedules/{id}/run",
            axum::routing::post(routes::run_schedule),
        )
        .route(
            "/api/schedules/{id}/delivery-log",
            axum::routing::get(routes::schedule_delivery_log),
        )
        // Workflow endpoints
        .route(
            "/api/workflows",
            axum::routing::get(routes::list_workflows).post(routes::create_workflow),
        )
        .route(
            "/api/workflows/{id}",
            axum::routing::get(routes::get_workflow)
                .put(routes::update_workflow)
                .delete(routes::delete_workflow),
        )
        .route(
            "/api/workflows/{id}/run",
            axum::routing::post(routes::run_workflow),
        )
        .route(
            "/api/workflows/{id}/runs",
            axum::routing::get(routes::list_workflow_runs),
        )
        // Skills endpoints
        .route("/api/skills", axum::routing::get(routes::list_skills))
        .route(
            "/api/skills/install",
            axum::routing::post(routes::install_skill),
        )
        .route(
            "/api/skills/uninstall",
            axum::routing::post(routes::uninstall_skill),
        )
        .route(
            "/api/skills/reload",
            axum::routing::post(routes::reload_skills),
        )
        // Audit trail (issue #1174 — instance-side wrapper integration)
        .route(
            "/api/audit/append",
            axum::routing::post(routes::audit_append),
        )
        .route(
            "/api/skills/{id}/config",
            axum::routing::get(routes::get_skill_config).put(routes::put_skill_config),
        )
        .route(
            "/api/skills/{id}/config/{var_name}",
            axum::routing::delete(routes::delete_skill_config_var),
        )
        .route(
            "/api/marketplace/search",
            axum::routing::get(routes::marketplace_search),
        )
        // ClawHub (OpenClaw ecosystem) endpoints
        .route(
            "/api/clawhub/search",
            axum::routing::get(routes::clawhub_search),
        )
        .route(
            "/api/clawhub/browse",
            axum::routing::get(routes::clawhub_browse),
        )
        .route(
            "/api/clawhub/skill/{slug}",
            axum::routing::get(routes::clawhub_skill_detail),
        )
        .route(
            "/api/clawhub/skill/{slug}/code",
            axum::routing::get(routes::clawhub_skill_code),
        )
        .route(
            "/api/clawhub/install",
            axum::routing::post(routes::clawhub_install),
        )
        // Hands endpoints
        .route("/api/hands", axum::routing::get(routes::list_hands))
        .route(
            "/api/hands/install",
            axum::routing::post(routes::install_hand),
        )
        .route(
            "/api/hands/upsert",
            axum::routing::post(routes::upsert_hand),
        )
        .route(
            "/api/hands/active",
            axum::routing::get(routes::list_active_hands),
        )
        .route("/api/hands/{hand_id}", axum::routing::get(routes::get_hand))
        .route(
            "/api/hands/{hand_id}/activate",
            axum::routing::post(routes::activate_hand),
        )
        .route(
            "/api/hands/{hand_id}/check-deps",
            axum::routing::post(routes::check_hand_deps),
        )
        .route(
            "/api/hands/{hand_id}/install-deps",
            axum::routing::post(routes::install_hand_deps),
        )
        .route(
            "/api/hands/{hand_id}/settings",
            axum::routing::get(routes::get_hand_settings).put(routes::update_hand_settings),
        )
        .route(
            "/api/hands/instances/{id}/pause",
            axum::routing::post(routes::pause_hand),
        )
        .route(
            "/api/hands/instances/{id}/resume",
            axum::routing::post(routes::resume_hand),
        )
        .route(
            "/api/hands/instances/{id}",
            axum::routing::delete(routes::deactivate_hand),
        )
        .route(
            "/api/hands/instances/{id}/stats",
            axum::routing::get(routes::hand_stats),
        )
        .route(
            "/api/hands/instances/{id}/browser",
            axum::routing::get(routes::hand_instance_browser),
        )
        // MCP server endpoints
        .route(
            "/api/mcp/servers",
            axum::routing::get(routes::list_mcp_servers),
        )
        // Audit endpoints
        .route(
            "/api/audit/recent",
            axum::routing::get(routes::audit_recent),
        )
        .route(
            "/api/audit/verify",
            axum::routing::get(routes::audit_verify),
        )
        // Live log streaming (SSE)
        .route("/api/logs/stream", axum::routing::get(routes::logs_stream))
        // Peer/Network endpoints
        .route("/api/peers", axum::routing::get(routes::list_peers))
        .route(
            "/api/network/status",
            axum::routing::get(routes::network_status),
        )
        // Agent communication (Comms) endpoints
        .route(
            "/api/comms/topology",
            axum::routing::get(routes::comms_topology),
        )
        .route(
            "/api/comms/events",
            axum::routing::get(routes::comms_events),
        )
        .route(
            "/api/comms/events/stream",
            axum::routing::get(routes::comms_events_stream),
        )
        .route("/api/comms/send", axum::routing::post(routes::comms_send))
        .route("/api/comms/task", axum::routing::post(routes::comms_task));

    // Split into a second router chunk to stay within axum's type nesting limit.
    let app = app
        // Tools endpoint
        .route("/api/tools", axum::routing::get(routes::list_tools))
        // Config endpoints
        .route("/api/config", axum::routing::get(routes::get_config))
        .route(
            "/api/config/schema",
            axum::routing::get(routes::config_schema),
        )
        .route("/api/config/set", axum::routing::post(routes::config_set))
        // Approval endpoints
        .route(
            "/api/approvals",
            axum::routing::get(routes::list_approvals).post(routes::create_approval),
        )
        .route(
            "/api/approvals/{id}/approve",
            axum::routing::post(routes::approve_request),
        )
        .route(
            "/api/approvals/{id}/reject",
            axum::routing::post(routes::reject_request),
        )
        // Usage endpoints
        .route("/api/usage", axum::routing::get(routes::usage_stats))
        .route(
            "/api/usage/summary",
            axum::routing::get(routes::usage_summary),
        )
        .route(
            "/api/usage/by-model",
            axum::routing::get(routes::usage_by_model),
        )
        .route("/api/usage/daily", axum::routing::get(routes::usage_daily))
        // Budget endpoints
        .route(
            "/api/budget",
            axum::routing::get(routes::budget_status).put(routes::update_budget),
        )
        .route(
            "/api/budget/agents",
            axum::routing::get(routes::agent_budget_ranking),
        )
        .route(
            "/api/budget/agents/{id}",
            axum::routing::get(routes::agent_budget_status).put(routes::update_agent_budget),
        )
        // Session endpoints
        .route("/api/sessions", axum::routing::get(routes::list_sessions))
        .route(
            "/api/sessions/{id}",
            axum::routing::delete(routes::delete_session),
        )
        .route(
            "/api/sessions/{id}/label",
            axum::routing::put(routes::set_session_label),
        )
        .route(
            "/api/agents/{id}/sessions/by-label/{label}",
            axum::routing::get(routes::find_session_by_label),
        )
        // Agent update
        .route(
            "/api/agents/{id}/update",
            axum::routing::put(routes::update_agent),
        )
        // Security dashboard endpoint
        .route("/api/security", axum::routing::get(routes::security_status))
        // Model catalog endpoints
        .route("/api/models", axum::routing::get(routes::list_models))
        .route(
            "/api/models/aliases",
            axum::routing::get(routes::list_aliases),
        )
        .route(
            "/api/models/custom",
            axum::routing::post(routes::add_custom_model),
        )
        .route(
            "/api/models/custom/{*id}",
            axum::routing::delete(routes::remove_custom_model),
        )
        .route("/api/models/{*id}", axum::routing::get(routes::get_model))
        .route("/api/providers", axum::routing::get(routes::list_providers))
        // Copilot OAuth (must be before parametric {name} routes)
        .route(
            "/api/providers/github-copilot/oauth/start",
            axum::routing::post(routes::copilot_oauth_start),
        )
        .route(
            "/api/providers/github-copilot/oauth/poll/{poll_id}",
            axum::routing::get(routes::copilot_oauth_poll),
        )
        .route(
            "/api/providers/{name}/key",
            axum::routing::post(routes::set_provider_key).delete(routes::delete_provider_key),
        )
        .route(
            "/api/providers/{name}/test",
            axum::routing::post(routes::test_provider),
        )
        .route(
            "/api/providers/{name}/url",
            axum::routing::put(routes::set_provider_url),
        )
        .route(
            "/api/skills/create",
            axum::routing::post(routes::create_skill),
        )
        // Migration endpoints
        .route(
            "/api/migrate/detect",
            axum::routing::get(routes::migrate_detect),
        )
        .route(
            "/api/migrate/scan",
            axum::routing::post(routes::migrate_scan),
        )
        .route("/api/migrate", axum::routing::post(routes::run_migrate))
        // Cron job management endpoints
        .route(
            "/api/cron/jobs",
            axum::routing::get(routes::list_cron_jobs).post(routes::create_cron_job),
        )
        .route(
            "/api/cron/jobs/{id}",
            axum::routing::delete(routes::delete_cron_job),
        )
        .route(
            "/api/cron/jobs/{id}/enable",
            axum::routing::put(routes::toggle_cron_job),
        )
        .route(
            "/api/cron/jobs/{id}/status",
            axum::routing::get(routes::cron_job_status),
        )
        .route(
            "/api/cron/jobs/{id}/run",
            axum::routing::post(routes::run_cron_job),
        )
        // Webhook trigger endpoints (external event injection)
        .route("/hooks/wake", axum::routing::post(routes::webhook_wake))
        .route("/hooks/agent", axum::routing::post(routes::webhook_agent))
        .route("/api/shutdown", axum::routing::post(routes::shutdown))
        // Chat commands endpoint (dynamic slash menu)
        .route("/api/commands", axum::routing::get(routes::list_commands))
        // Config reload endpoint
        .route(
            "/api/config/reload",
            axum::routing::post(routes::config_reload),
        )
        // Agent binding routes
        .route(
            "/api/bindings",
            axum::routing::get(routes::list_bindings).post(routes::add_binding),
        )
        .route(
            "/api/bindings/{index}",
            axum::routing::delete(routes::remove_binding),
        )
        // A2A (Agent-to-Agent) Protocol endpoints
        .route(
            "/.well-known/agent.json",
            axum::routing::get(routes::a2a_agent_card),
        )
        .route("/a2a/agents", axum::routing::get(routes::a2a_list_agents))
        .route(
            "/a2a/tasks/send",
            axum::routing::post(routes::a2a_send_task),
        )
        .route("/a2a/tasks/{id}", axum::routing::get(routes::a2a_get_task))
        .route(
            "/a2a/tasks/{id}/cancel",
            axum::routing::post(routes::a2a_cancel_task),
        )
        // A2A management (outbound) endpoints
        .route(
            "/api/a2a/agents",
            axum::routing::get(routes::a2a_list_external_agents),
        )
        .route(
            "/api/a2a/discover",
            axum::routing::post(routes::a2a_discover_external),
        )
        .route(
            "/api/a2a/send",
            axum::routing::post(routes::a2a_send_external),
        )
        .route(
            "/api/a2a/tasks/{id}/status",
            axum::routing::get(routes::a2a_external_task_status),
        )
        // Integration management endpoints
        .route(
            "/api/integrations",
            axum::routing::get(routes::list_integrations),
        )
        .route(
            "/api/integrations/available",
            axum::routing::get(routes::list_available_integrations),
        )
        .route(
            "/api/integrations/add",
            axum::routing::post(routes::add_integration),
        )
        .route(
            "/api/integrations/{id}",
            axum::routing::delete(routes::remove_integration),
        )
        .route(
            "/api/integrations/{id}/reconnect",
            axum::routing::post(routes::reconnect_integration),
        )
        .route(
            "/api/integrations/health",
            axum::routing::get(routes::integrations_health),
        )
        .route(
            "/api/integrations/reload",
            axum::routing::post(routes::reload_integrations),
        )
        // Device pairing endpoints
        .route(
            "/api/pairing/request",
            axum::routing::post(routes::pairing_request),
        )
        .route(
            "/api/pairing/complete",
            axum::routing::post(routes::pairing_complete),
        )
        .route(
            "/api/pairing/devices",
            axum::routing::get(routes::pairing_devices),
        )
        .route(
            "/api/pairing/devices/{id}",
            axum::routing::delete(routes::pairing_remove_device),
        )
        .route(
            "/api/pairing/notify",
            axum::routing::post(routes::pairing_notify),
        )
        // MCP HTTP endpoint (exposes MCP protocol over HTTP)
        .route("/mcp", axum::routing::post(routes::mcp_http))
        // OpenAI-compatible API
        .route(
            "/v1/chat/completions",
            axum::routing::post(crate::openai_compat::chat_completions),
        )
        .route(
            "/v1/models",
            axum::routing::get(crate::openai_compat::list_models),
        )
        // Passkey-only dashboard authentication endpoints.
        .route(
            "/api/auth/passkey/login/start",
            axum::routing::post(crate::passkey_auth::login_start),
        )
        .route(
            "/api/auth/passkey/login/finish",
            axum::routing::post(crate::passkey_auth::login_finish),
        )
        .route(
            "/api/auth/passkey/register/start",
            axum::routing::post(crate::passkey_auth::register_start),
        )
        .route(
            "/api/auth/passkey/register/finish",
            axum::routing::post(crate::passkey_auth::register_finish),
        )
        // Выдача доступа ссылкой. За общей аутентификацией: позвать может тот,
        // у кого есть ключ демона, или уже открытая сессия по пасскею.
        .route(
            "/api/auth/invites",
            axum::routing::post(crate::passkey_auth::invite_create)
                .get(crate::passkey_auth::invite_list),
        )
        .route(
            "/api/auth/invites/{slug}",
            axum::routing::delete(crate::passkey_auth::invite_revoke),
        )
        .route(
            "/api/auth/logout",
            axum::routing::post(crate::passkey_auth::logout),
        )
        .route(
            "/api/auth/check",
            axum::routing::get(crate::passkey_auth::auth_check),
        )
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            middleware::auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            gcra_limiter,
            rate_limiter::gcra_rate_limit,
        ))
        .layer(axum::middleware::from_fn(middleware::security_headers))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state.clone());

    (app, state)
}

/// Start the OpenFang daemon: boot kernel + HTTP API server.
///
/// This function blocks until Ctrl+C or a shutdown request.
pub async fn run_daemon(
    kernel: OpenFangKernel,
    listen_addr: &str,
    daemon_info_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = listen_addr.parse()?;

    let kernel = Arc::new(kernel);
    kernel.set_self_handle();
    kernel.start_background_agents();

    // Config file hot-reload watcher (polls every 30 seconds)
    {
        let k = kernel.clone();
        let config_path = kernel.config.home_dir.join("config.toml");
        tokio::spawn(async move {
            let mut last_modified = std::fs::metadata(&config_path)
                .and_then(|m| m.modified())
                .ok();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let current = std::fs::metadata(&config_path)
                    .and_then(|m| m.modified())
                    .ok();
                if current != last_modified && current.is_some() {
                    last_modified = current;
                    tracing::info!("Config file changed, reloading...");
                    match k.reload_config() {
                        Ok(plan) => {
                            // `reload_config` already logged the honest
                            // applied-vs-deferred split via
                            // `plan.log_apply_outcome()` — do not re-summarize
                            // `plan.hot_actions` here as "applied", since that
                            // list is only the diff, not a receipt (FANG-42).
                            if !plan.has_changes() {
                                tracing::debug!("Config hot-reload: no actionable changes");
                            }
                        }
                        Err(e) => tracing::warn!("Config hot-reload failed: {e}"),
                    }
                }
            }
        });
    }

    let (app, state) = build_router(kernel.clone(), addr).await;

    // Write daemon info file
    if let Some(info_path) = daemon_info_path {
        // Check if another daemon is already running with this PID file
        if info_path.exists() {
            if let Ok(existing) = std::fs::read_to_string(info_path) {
                if let Ok(info) = serde_json::from_str::<DaemonInfo>(&existing) {
                    // PID alive AND the health endpoint responds → truly running
                    if is_process_alive(info.pid) && is_daemon_responding(&info.listen_addr) {
                        return Err(format!(
                            "Another daemon (PID {}) is already running at {}",
                            info.pid, info.listen_addr
                        )
                        .into());
                    }
                }
            }
            // Stale PID file (process dead or different process reused PID), remove it
            info!("Removing stale daemon info file");
            let _ = std::fs::remove_file(info_path);
        }

        let daemon_info = DaemonInfo {
            pid: std::process::id(),
            listen_addr: addr.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&daemon_info) {
            let _ = std::fs::write(info_path, json);
            // SECURITY: Restrict daemon info file permissions (contains PID and port).
            restrict_permissions(info_path);
        }
    }

    info!("OpenFang API server listening on http://{addr}");
    info!("WebChat UI available at http://{addr}/",);
    info!("WebSocket endpoint: ws://{addr}/api/agents/{{id}}/ws",);

    // Use SO_REUSEADDR to allow binding immediately after reboot (avoids TIME_WAIT).
    let socket = socket2::Socket::new(
        if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        },
        socket2::Type::STREAM,
        None,
    )?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    let listener = tokio::net::TcpListener::from_std(std::net::TcpListener::from(socket))?;

    // Run server with graceful shutdown.
    // SECURITY: `into_make_service_with_connect_info` injects the peer
    // SocketAddr so the auth middleware can check for loopback connections.
    let api_shutdown = state.shutdown_notify.clone();
    let drain_timeout = shutdown_drain_timeout();
    let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal(api_shutdown).await;
        // Tells the arm below that the signal has landed, so the drain window
        // starts counting from the signal and not from process start.
        let _ = drain_started_tx.send(());
    })
    // `Serve`/`WithGracefulShutdown` are `IntoFuture`, not `Future`; `.await`
    // hides that, but polling it twice (once in the `select!`, once under the
    // timeout) needs the real future.
    .into_future();
    tokio::pin!(serve);

    let serve_result = tokio::select! {
        result = &mut serve => result,
        _ = drain_started_rx => {
            // `with_graceful_shutdown` waits for every in-flight request, with
            // no upper bound. Some of this server's requests are long:
            // `POST /api/channels/reload` blocks for the remainder of an
            // in-flight Telegram `getUpdates` (up to ~30 s, see
            // `BridgeManager::stop`), and a WebSocket at
            // /api/agents/{id}/ws stays open as long as its client does.
            // Everything below — daemon.json removal, stopping the bridges,
            // `kernel.shutdown()` — runs only after that await returns, so an
            // unbounded wait means none of it runs at all: the supervisor's
            // grace period expires and the process is SIGKILLed — measured
            // before this bound existed: `docker stop -t 10` fired during a
            // reload took 10.24 s and the container exited 137
            // (tests/fang/baseline/FANG-40-sigterm.txt). Bound the wait so the
            // exit path always gets to run.
            match tokio::time::timeout(drain_timeout, &mut serve).await {
                Ok(result) => result,
                Err(_) => {
                    // Not "closing them": dropping the `serve` future stops
                    // accepting and stops waiting, but axum's per-connection
                    // tasks are `tokio::spawn`ed and outlive it. They end when
                    // the process does, a few lines below.
                    warn!(
                        "Requests still in flight after {}s of graceful shutdown; \
                         leaving them and continuing to exit",
                        drain_timeout.as_secs_f32()
                    );
                    Ok(())
                }
            }
        }
    };

    // Clean up daemon info file
    if let Some(info_path) = daemon_info_path {
        let _ = std::fs::remove_file(info_path);
    }

    // Stop channel bridges. `stop_fast` on purpose: the process is exiting, so
    // draining a Telegram long-poll to free its reader slot buys nothing (the
    // socket is about to close regardless). The graceful `stop()` belongs to
    // hot-reload, where a new poller is about to take the slot.
    //
    // Both halves of that are measured by tests/fang/FANG-40-sigterm.sh against
    // a stub whose getUpdates holds for 30 s (output in
    // tests/fang/after-v3/FANG-40-sigterm.txt). `OF40S_PLAIN_STOP_SECS` is a
    // whole SIGTERM-to-exit with such a poll outstanding and stays under a
    // second; `OF40S_DRAIN_SECS` is the same poll drained by hot-reload's
    // `stop()` and runs to the remainder of the poll — the two numbers are
    // seconds apart in the same run, which is the point of using different
    // stops in the two places.
    //
    // `try_lock` rather than `lock`: nothing here may block on a mutex a
    // request handler holds, on a deadline the supervisor is counting down.
    // `reload_channels_from_disk` no longer holds it across its drain — it
    // `take()`s the bridge and drains it unlocked — so contention is a short
    // window around two moves rather than the ~30 s it used to be, and losing
    // the race costs nothing: the reload owns that bridge and is stopping it
    // itself.
    //
    // The empty slot is the more likely of the two: for the length of a reload
    // the mutex holds `None`, so a SIGTERM arriving then finds nothing to stop.
    // Both cases are logged, because in both of them this line stops no adapter
    // and a silent skip here is indistinguishable from a clean stop in the log.
    match state.bridge_manager.try_lock() {
        Ok(mut guard) => match *guard {
            Some(ref mut b) => b.stop_fast().await,
            None => info!(
                "No channel bridge to stop at shutdown \
                 (none configured, or a hot-reload holds it)"
            ),
        },
        Err(_) => {
            warn!("Channel hot-reload in progress at shutdown; leaving its bridge to the exit");
        }
    }

    // Shutdown kernel
    kernel.shutdown();

    // Reported last, not with `?` at the await: a serve error must not skip the
    // cleanup above, which is the whole reason that await is no longer the last
    // statement in this function.
    serve_result?;

    info!("OpenFang daemon stopped");
    Ok(())
}

/// SECURITY: Restrict file permissions to owner-only (0600) on Unix.
/// On non-Unix platforms this is a no-op.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Read daemon info from the standard location.
pub fn read_daemon_info(home_dir: &Path) -> Option<DaemonInfo> {
    let info_path = home_dir.join("daemon.json");
    let contents = std::fs::read_to_string(info_path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Resolve the graceful-shutdown drain budget — see
/// [`SHUTDOWN_DRAIN_TIMEOUT_DEFAULT`].
///
/// `OPENFANG_SHUTDOWN_DRAIN_SECS` overrides it; `0` disables draining entirely
/// (in-flight requests are cut immediately).
///
/// Resolved once at **startup**, before `axum::serve`, because the value has to
/// be in hand before the signal lands — so a bad value must not be fatal. It
/// used to be: `Duration::from_secs_f64` panics on a value it cannot represent,
/// and `1e30` parses as an `f64` perfectly well, so
/// `OPENFANG_SHUTDOWN_DRAIN_SECS=1e30` aborted the daemon at boot with
/// "cannot convert float seconds to Duration" and exit 101 — a shutdown knob
/// taking the process down on the way up. Every rejected value now falls back to
/// the default with a warning.
///
/// Not clamped at the top end: a representable-but-huge value is a deliberate
/// "wait as long as it takes", which defeats the bound this exists to impose.
/// That is the operator's call to make, and the supervisor's SIGKILL is the
/// backstop.
fn shutdown_drain_timeout() -> std::time::Duration {
    parse_drain_timeout(
        std::env::var("OPENFANG_SHUTDOWN_DRAIN_SECS")
            .ok()
            .as_deref(),
    )
}

/// The body of [`shutdown_drain_timeout`], without the environment.
///
/// Split out so the parsing is testable without `set_var`, which is racy across
/// a test binary's threads and unsafe from Rust 2024 on.
fn parse_drain_timeout(raw: Option<&str>) -> std::time::Duration {
    let Some(raw) = raw else {
        return SHUTDOWN_DRAIN_TIMEOUT_DEFAULT;
    };
    // `try_from_secs_f64` is the whole point: it rejects NaN, infinity,
    // negatives and anything too large for a `Duration` by returning `Err`
    // instead of panicking.
    match raw
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|secs| std::time::Duration::try_from_secs_f64(secs).ok())
    {
        Some(d) => d,
        None => {
            warn!(
                "Ignoring unusable OPENFANG_SHUTDOWN_DRAIN_SECS={raw:?}; \
                 using the {}s default",
                SHUTDOWN_DRAIN_TIMEOUT_DEFAULT.as_secs_f32()
            );
            SHUTDOWN_DRAIN_TIMEOUT_DEFAULT
        }
    }
}

/// Wait for an OS termination signal OR an API shutdown request.
///
/// On Unix: listens for SIGINT, SIGTERM, and API notify.
/// On Windows: listens for Ctrl+C and API notify.
async fn shutdown_signal(api_shutdown: Arc<tokio::sync::Notify>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to listen for SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to listen for SIGTERM");

        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT (Ctrl+C), shutting down...");
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
            }
            _ = api_shutdown.notified() => {
                info!("Shutdown requested via API, shutting down...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down...");
            }
            _ = api_shutdown.notified() => {
                info!("Shutdown requested via API, shutting down...");
            }
        }
    }
}

/// Check if a process with the given PID is still alive.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Use kill -0 to check if process exists without sending a signal
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(windows)]
    {
        // tasklist /FI "PID eq N" returns "INFO: No tasks..." when no match,
        // or a table row with the PID when found. Check exit code and that
        // "INFO:" is NOT in the output to confirm the process exists.
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| {
                o.status.success() && {
                    let out = String::from_utf8_lossy(&o.stdout);
                    !out.contains("INFO:") && out.contains(&pid.to_string())
                }
            })
            .unwrap_or(false)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Check if an OpenFang daemon is actually responding at the given address.
/// This avoids false positives where a different process reused the same PID
/// after a system reboot.
fn is_daemon_responding(addr: &str) -> bool {
    // Quick TCP connect check — don't make a full HTTP request to avoid delays
    let addr_only = addr
        .strip_prefix("http://")
        .or_else(|| addr.strip_prefix("https://"))
        .unwrap_or(addr);
    if let Ok(sock_addr) = addr_only.parse::<std::net::SocketAddr>() {
        std::net::TcpStream::connect_timeout(&sock_addr, std::time::Duration::from_millis(500))
            .is_ok()
    } else {
        // Fallback: try connecting to hostname
        std::net::TcpStream::connect(addr_only)
            .map(|_| true)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_drain_timeout, SHUTDOWN_DRAIN_TIMEOUT_DEFAULT};
    use std::time::Duration;

    /// `OPENFANG_SHUTDOWN_DRAIN_SECS` is read at startup, so no value of it may
    /// stop the daemon from starting.
    ///
    /// Before this was fixed, every input on the second list below reached
    /// `Duration::from_secs_f64` and panicked there — the daemon printed
    /// "OpenFang API server listening on ..." and then died with
    /// "cannot convert float seconds to Duration" and exit 101.
    #[test]
    fn no_value_of_the_drain_override_can_stop_the_daemon_starting() {
        // Honoured.
        assert_eq!(parse_drain_timeout(Some("0")), Duration::ZERO);
        assert_eq!(parse_drain_timeout(Some("7")), Duration::from_secs(7));
        assert_eq!(
            parse_drain_timeout(Some(" 2.5 ")),
            Duration::from_secs_f64(2.5)
        );

        // Rejected, each falling back to the default rather than aborting.
        for raw in [
            "1e30",  // parses as f64, far too large for a Duration
            "1e400", // parses as f64 infinity
            "inf", "-inf", "NaN", "-1", "", "  ", "three", "3s",
        ] {
            assert_eq!(
                parse_drain_timeout(Some(raw)),
                SHUTDOWN_DRAIN_TIMEOUT_DEFAULT,
                "{raw:?} should fall back to the default"
            );
        }

        // Unset.
        assert_eq!(parse_drain_timeout(None), SHUTDOWN_DRAIN_TIMEOUT_DEFAULT);
    }
}
