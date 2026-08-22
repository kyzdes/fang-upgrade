//! Production middleware for the OpenFang API server.
//!
//! Provides:
//! - Request ID generation and propagation
//! - Per-endpoint structured request logging
//! - In-memory rate limiting (per IP)

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::middleware::Next;
use std::time::Instant;
use tracing::info;

/// Request ID header name (standard).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Middleware: inject a unique request ID and log the request/response.
pub async fn request_logging(request: Request<Body>, next: Next) -> Response<Body> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = request.method().clone();
    let uri = request.uri().path().to_string();
    let start = Instant::now();

    let mut response = next.run(request).await;

    let elapsed = start.elapsed();
    let status = response.status().as_u16();

    info!(
        request_id = %request_id,
        method = %method,
        path = %uri,
        status = status,
        latency_ms = elapsed.as_millis() as u64,
        "API request"
    );

    // Inject the request ID into the response
    if let Ok(header_val) = request_id.parse() {
        response.headers_mut().insert(REQUEST_ID_HEADER, header_val);
    }

    response
}

/// Authentication state passed to the auth middleware.
#[derive(Clone)]
pub struct AuthState {
    pub api_key: String,
    pub auth_enabled: bool,
    pub passkey_auth: Option<std::sync::Arc<crate::passkey_auth::PasskeyAuthService>>,
    /// Set from `OPENFANG_ALLOW_NO_AUTH=1` to permit running without an api_key
    /// on a non-loopback bind. Off by default so empty keys fail closed.
    pub allow_no_auth: bool,
}

/// Bearer token authentication middleware.
///
/// When `api_key` is non-empty (after trimming), requests to non-public
/// endpoints must include `Authorization: Bearer <api_key>`.
///
/// When `api_key` is empty (no key configured) the server defaults to
/// fail-closed for any request that does NOT originate from loopback.
/// Loopback traffic (127.0.0.1 / ::1) is always allowed through with no
/// key so single-user local setups keep zero-config UX. To explicitly
/// run a no-auth server on a LAN/WAN address, set
/// `OPENFANG_ALLOW_NO_AUTH=1`; this opts out of fail-closed and is
/// reported loudly at startup.
///
/// When dashboard auth is enabled, session cookies are also accepted.
pub async fn auth(
    axum::extract::State(auth_state): axum::extract::State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    // SECURITY: Capture method early for method-aware public endpoint checks.
    let method = request.method().clone();

    let is_loopback = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false); // SECURITY: default-deny; unknown origin is NOT loopback

    // Shutdown is loopback-only (CLI on same machine). Skip token auth only
    // when the request is from loopback.
    let path = request.uri().path();
    if path == "/api/shutdown" && is_loopback {
        return next.run(request).await;
    }

    // Passkey mode exposes only the ceremony surface, health, and login assets.
    // The old broad read-only allowlist remains only for local/no-passkey mode.
    let is_get = method == axum::http::Method::GET;
    let is_public = if auth_state.auth_enabled {
        (is_get
            && matches!(
                path,
                // Оболочка панели. Публична намеренно: это статическая страница,
                // а каждый запрос данных за ней проверяется отдельно. Без неё
                // браузер не может войти вообще — на первую навигацию заголовок
                // Authorization не поставить, и запасной вход по ключу демона
                // становится недостижим ещё до того, как страница загрузится.
                "/" | "/login"
                    | "/register"
                    | "/logo.png"
                    | "/favicon.ico"
                    | "/api/health"
                    | "/api/auth/check"
            ))
            || (method == axum::http::Method::POST
                && matches!(
                    path,
                    "/api/auth/passkey/login/start"
                        | "/api/auth/passkey/login/finish"
                        | "/api/auth/passkey/register/start"
                        | "/api/auth/passkey/register/finish"
                ))
    } else {
        path == "/"
            || path == "/logo.png"
            || path == "/favicon.ico"
            || (path == "/.well-known/agent.json" && is_get)
            || (path.starts_with("/a2a/") && is_get)
            || path == "/api/health"
            || path == "/api/health/detail"
            || path == "/api/status"
            || path == "/api/version"
            || (path == "/api/agents" && is_get)
            || (path == "/api/profiles" && is_get)
            || (path == "/api/config" && is_get)
            || (path == "/api/config/schema" && is_get)
            || (path.starts_with("/api/uploads/") && is_get)
            || (path == "/api/models" && is_get)
            || (path == "/api/models/aliases" && is_get)
            || (path == "/api/providers" && is_get)
            || (path == "/api/budget" && is_get)
            || (path == "/api/budget/agents" && is_get)
            || (path.starts_with("/api/budget/agents/") && is_get)
            || (path == "/api/network/status" && is_get)
            || (path == "/api/a2a/agents" && is_get)
            || (path == "/api/approvals" && is_get)
            || (path.starts_with("/api/approvals/") && is_get)
            || (path == "/api/channels" && is_get)
            || (path == "/api/hands" && is_get)
            || (path == "/api/hands/active" && is_get)
            || (path.starts_with("/api/hands/") && is_get)
            || (path == "/api/skills" && is_get)
            || (path.starts_with("/api/skills/") && path.ends_with("/config") && is_get)
            || (path == "/api/sessions" && is_get)
            || (path == "/api/integrations" && is_get)
            || (path == "/api/integrations/available" && is_get)
            || (path == "/api/integrations/health" && is_get)
            || (path == "/api/workflows" && is_get)
            || path == "/api/logs/stream"
            || (path.starts_with("/api/cron/") && is_get)
            || path.starts_with("/api/providers/github-copilot/oauth/")
    };

    if is_public {
        return next.run(request).await;
    }

    // If no API key configured and no dashboard login is active, fail closed
    // for anything that did not come from loopback. Opting out of this
    // behavior requires setting `OPENFANG_ALLOW_NO_AUTH=1`, which is logged
    // loudly at startup.
    //
    // See issue #1034 (B1/B2): empty api_key previously bypassed auth for
    // all origins, exposing agent config, channel tokens, and LLM keys on
    // any LAN-reachable bind.
    let api_key_trimmed = auth_state.api_key.trim().to_string();
    if api_key_trimmed.is_empty() && !auth_state.auth_enabled {
        if is_loopback || auth_state.allow_no_auth {
            return next.run(request).await;
        }
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Bearer")
            .body(Body::from(
                serde_json::json!({
                    "error": "API key required for non-loopback requests. Set OPENFANG_API_KEY or bind to 127.0.0.1."
                })
                .to_string(),
            ))
            .unwrap_or_default();
    }
    let api_key = api_key_trimmed.as_str();

    // Check Authorization: Bearer <token> header, then fallback to X-API-Key
    let bearer_token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let api_token = bearer_token.or_else(|| {
        request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
    });

    // SECURITY: Use constant-time comparison to prevent timing attacks.
    let header_auth = api_token.map(|token| {
        use subtle::ConstantTimeEq;
        if api_key.is_empty() || token.len() != api_key.len() {
            return false;
        }
        token.as_bytes().ct_eq(api_key.as_bytes()).into()
    });

    // Also check ?token= query parameter (for EventSource/SSE clients that
    // cannot set custom headers, same approach as WebSocket auth).
    let query_token_decoded = request
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")))
        .map(crate::percent_decode);

    // SECURITY: Use constant-time comparison to prevent timing attacks.
    let query_auth = query_token_decoded.as_deref().map(|token| {
        use subtle::ConstantTimeEq;
        if api_key.is_empty() || token.len() != api_key.len() {
            return false;
        }
        token.as_bytes().ct_eq(api_key.as_bytes()).into()
    });

    // Accept if either auth method matches
    if header_auth == Some(true) || query_auth == Some(true) {
        return next.run(request).await;
    }

    // Check the opaque, server-side dashboard session. Cookie-authenticated
    // mutations must carry the exact configured Origin; API-key clients are exempt.
    if let Some(service) = auth_state.passkey_auth.as_ref() {
        if service
            .validate_request_session(request.headers())
            .is_some()
        {
            let safe_method = matches!(
                method,
                axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
            );
            if safe_method || service.validate_origin(request.headers()) {
                return next.run(request).await;
            }
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"error": "Invalid request origin"}).to_string(),
                ))
                .unwrap_or_default();
        }
    }

    // Determine error message: was a credential provided but wrong, or missing entirely?
    let credential_provided = header_auth.is_some() || query_auth.is_some();
    let error_msg = if credential_provided {
        "Invalid API key"
    } else {
        "Missing Authorization: Bearer <api_key> header"
    };

    let wants_html = path == "/"
        || request
            .headers()
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.contains("text/html"))
            .unwrap_or(false);
    if auth_state.auth_enabled && wants_html {
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(axum::http::header::LOCATION, "/login")
            .body(Body::empty())
            .unwrap_or_default();
    }

    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", "Bearer")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"error": error_msg}).to_string(),
        ))
        .unwrap_or_default()
}

/// Security headers middleware — applied to ALL API responses.
pub async fn security_headers(request: Request<Body>, next: Next) -> Response<Body> {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("x-xss-protection", "1; mode=block".parse().unwrap());
    // The dashboard handler (webchat_page) sets its own nonce-based CSP.
    // For all other responses (API endpoints), apply a strict default.
    if !headers.contains_key("content-security-policy") {
        headers.insert(
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'"
                .parse()
                .unwrap(),
        );
    }
    headers.insert(
        "referrer-policy",
        "strict-origin-when-cross-origin".parse().unwrap(),
    );
    headers.insert(
        "cache-control",
        "no-store, no-cache, must-revalidate".parse().unwrap(),
    );
    headers.insert(
        "strict-transport-security",
        "max-age=63072000; includeSubDomains".parse().unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request};
    use axum::routing::get;
    use axum::Router;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    #[test]
    fn test_request_id_header_constant() {
        assert_eq!(REQUEST_ID_HEADER, "x-request-id");
    }

    fn auth_state_empty() -> AuthState {
        AuthState {
            api_key: String::new(),
            auth_enabled: false,
            passkey_auth: None,
            allow_no_auth: false,
        }
    }

    fn auth_state_with_key(key: &str) -> AuthState {
        AuthState {
            api_key: key.to_string(),
            auth_enabled: false,
            passkey_auth: None,
            allow_no_auth: false,
        }
    }

    fn auth_state_passkey_without_session() -> AuthState {
        AuthState {
            api_key: String::new(),
            auth_enabled: true,
            passkey_auth: None,
            allow_no_auth: false,
        }
    }

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn router(state: AuthState) -> Router {
        Router::new()
            .route("/", get(ok_handler))
            .route("/api/health", get(ok_handler))
            .route("/api/agents/1", get(ok_handler))
            .route_layer(axum::middleware::from_fn_with_state(state, auth))
    }

    fn req_from(ip: &str) -> Request<Body> {
        let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/agents/1")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn empty_key_allows_loopback() {
        let app = router(auth_state_empty());
        let resp = app.oneshot(req_from("127.0.0.1")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_key_blocks_lan_origin() {
        // Issue #1034 B1: previously 192.168/10/... could hit every non-public
        // endpoint when api_key was unset. Must now be 401.
        let app = router(auth_state_empty());
        let resp = app.oneshot(req_from("192.168.1.50")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_key_blocks_public_origin() {
        let app = router(auth_state_empty());
        let resp = app.oneshot(req_from("203.0.113.5")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_key_blocks_unknown_connect_info() {
        // Paranoia: if ConnectInfo is missing for any reason, we must fail
        // closed, not open.
        let app = router(auth_state_empty());
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/agents/1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_key_with_allow_no_auth_opens_everything() {
        let mut s = auth_state_empty();
        s.allow_no_auth = true;
        let app = router(s);
        let resp = app.oneshot(req_from("10.0.0.9")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn configured_key_rejects_missing_token_from_loopback() {
        let app = router(auth_state_with_key("secret"));
        let resp = app.oneshot(req_from("127.0.0.1")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn configured_key_accepts_bearer() {
        let app = router(auth_state_with_key("secret"));
        let addr: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/agents/1")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn passkey_mode_protects_reads_but_keeps_health_public() {
        let app = router(auth_state_passkey_without_session());
        let protected = app.clone().oneshot(req_from("203.0.113.5")).await.unwrap();
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);

        let health = Request::builder()
            .method(Method::GET)
            .uri("/api/health")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(health).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_bearer_never_matches_an_empty_machine_key() {
        let app = router(auth_state_passkey_without_session());
        let mut req = req_from("203.0.113.5");
        req.headers_mut()
            .insert("authorization", "Bearer ".parse().unwrap());
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Оболочка панели публична, данные за ней — нет. Обе половины здесь,
    /// потому что порознь каждая выглядит как ошибка.
    ///
    /// Прежний тест на этом месте требовал от `/` редиректа 303 на `/login`.
    /// Он описывал замысел, от которого автор отказался в том же коммите:
    /// `/login` (`webchat::login_page`) отдаёт ТОЛЬКО пасскей и прямо пишет
    /// «Пароль и API-ключ для браузера не используются», а запасной вход по
    /// ключу демона живёт в оболочке на `/` — его стережёт отдельный тест в
    /// `webchat.rs`. Закрыть `/` значит сделать запасной вход недостижимым
    /// ровно тогда, когда он нужен: при отказе пасскея. Поэтому 200, и это
    /// решение, а не недосмотр.
    #[tokio::test]
    async fn dashboard_shell_is_public_but_the_data_behind_it_is_not() {
        // Оболочка: браузер на первой навигации не может поставить
        // Authorization, поэтому страница обязана открыться.
        let app = router(auth_state_passkey_without_session());
        let shell = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(shell).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "оболочка панели должна открываться без сессии, иначе запасной вход недостижим"
        );

        // Данные за оболочкой: тот же посетитель, тот же роутер, но 401.
        // Без этой половины первая читалась бы как дыра в защите.
        let app = router(auth_state_passkey_without_session());
        let data = Request::builder()
            .method(Method::GET)
            .uri("/api/agents/1")
            .header("accept", "application/json")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(data).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "публичная оболочка не должна открывать доступ к данным"
        );
    }
}
