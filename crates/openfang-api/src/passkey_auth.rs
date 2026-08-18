//! Passkey-only dashboard authentication backed by persistent SQLite sessions.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use openfang_memory::dashboard_auth::{DashboardAuthStore, DashboardUser, SessionPrincipal};
use openfang_types::config::AuthConfig;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Webauthn, WebauthnBuilder,
};

use crate::routes::AppState;

pub const SESSION_COOKIE: &str = "__Host-openfang_session";
const CEREMONY_TTL_SECONDS: i64 = 5 * 60;
const MAX_PENDING_CEREMONIES: usize = 256;

struct RegistrationCeremony {
    state: PasskeyRegistration,
    invite_id: String,
    expires_at: i64,
}

struct AuthenticationCeremony {
    state: PasskeyAuthentication,
    expires_at: i64,
}

pub struct PasskeyAuthService {
    webauthn: Webauthn,
    store: DashboardAuthStore,
    registrations: Mutex<HashMap<String, RegistrationCeremony>>,
    authentications: Mutex<HashMap<String, AuthenticationCeremony>>,
    rp_origin: String,
    session_ttl_hours: u64,
}

impl PasskeyAuthService {
    /// Construct the WebAuthn service. Invalid public configuration is fatal at startup.
    pub fn new(config: &AuthConfig, store: DashboardAuthStore) -> Result<Self, String> {
        if !config.enabled {
            return Err(
                "passkey auth service cannot be created while auth.enabled is false".into(),
            );
        }
        let rp_id = config.rp_id.trim();
        let rp_origin = config.rp_origin.trim();
        let rp_name = config.rp_name.trim();
        if rp_id.is_empty() || rp_origin.is_empty() || rp_name.is_empty() {
            return Err("auth.rp_id, auth.rp_origin, and auth.rp_name are required".into());
        }
        if config.session_ttl_hours == 0 || config.session_ttl_hours > 24 * 30 {
            return Err("auth.session_ttl_hours must be between 1 and 720".into());
        }

        let origin = Url::parse(rp_origin).map_err(|e| format!("invalid auth.rp_origin: {e}"))?;
        if origin.scheme() != "https"
            || origin.host_str() != Some(rp_id)
            || origin.port().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            return Err(
                "auth.rp_origin must be an exact HTTPS origin whose host equals auth.rp_id".into(),
            );
        }

        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| format!("invalid WebAuthn relying-party configuration: {e}"))?
            .rp_name(rp_name)
            .build()
            .map_err(|e| format!("failed to build WebAuthn service: {e}"))?;

        Ok(Self {
            webauthn,
            store,
            registrations: Mutex::new(HashMap::new()),
            authentications: Mutex::new(HashMap::new()),
            rp_origin: rp_origin.to_string(),
            session_ttl_hours: config.session_ttl_hours,
        })
    }

    pub fn rp_origin(&self) -> &str {
        &self.rp_origin
    }

    pub fn validate_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == self.rp_origin)
            .unwrap_or(false)
    }

    /// Выпустить приглашение и вернуть ссылку, по которой заводится пасскей.
    ///
    /// Токен живёт во **фрагменте** ссылки (`/register#<токен>`), поэтому он не
    /// уходит на сервер и не попадает ни в его логи, ни в логи обратного прокси.
    /// В базе лежит только SHA-256 от него.
    ///
    /// Слот заводится по требованию: `ensure_slot` создаёт его, если такого нет.
    /// Жёсткого списка людей здесь нет и быть не должно — кому выдавать доступ,
    /// решает тот, кто зовёт этот метод, а не список в коде.
    pub fn issue_invite(
        &self,
        slug: &str,
        display_name: &str,
        ttl_hours: u64,
    ) -> Result<(String, i64), String> {
        if ttl_hours == 0 || ttl_hours > 24 * 14 {
            return Err("ttl_hours must be between 1 and 336 (14 days)".into());
        }
        let now = unix_timestamp();
        let expires_at = now + (ttl_hours as i64) * 3600;
        let token = generate_secret_token();
        let hash = hash_secret(token.as_bytes());
        self.store
            .create_invite(slug, display_name, &hash, now, expires_at)
            .map_err(|e| e.to_string())?;
        Ok((registration_url(&self.rp_origin, &token), expires_at))
    }

    /// Кто заведён и в каком состоянии. Ни токенов, ни ключей не раскрывает.
    pub fn list_slots(&self) -> Result<Vec<openfang_memory::dashboard_auth::SlotStatus>, String> {
        self.store
            .list_slots(unix_timestamp())
            .map_err(|e| e.to_string())
    }

    /// Отозвать у слота ключ, сессии и неиспользованные приглашения.
    pub fn revoke_slot(&self, slug: &str) -> Result<bool, String> {
        self.store
            .revoke_slot(slug, unix_timestamp())
            .map_err(|e| e.to_string())
    }

    pub fn validate_request_session(&self, headers: &HeaderMap) -> Option<SessionPrincipal> {
        let token = extract_session_cookie(headers)?;
        self.store
            .validate_session(&hash_secret(token.as_bytes()), unix_timestamp())
            .ok()
            .flatten()
    }

    fn issue_session(
        &self,
        user: &DashboardUser,
        credential_id: &[u8],
    ) -> Result<(String, i64), String> {
        let token = generate_secret_token();
        let now = unix_timestamp();
        let expires_at = now
            .checked_add((self.session_ttl_hours as i64) * 3600)
            .ok_or_else(|| "session expiry overflow".to_string())?;
        self.store
            .create_session(
                &hash_secret(token.as_bytes()),
                &user.id,
                credential_id,
                now,
                expires_at,
            )
            .map_err(|e| e.to_string())?;
        Ok((token, expires_at))
    }

    fn store_registration(&self, ceremony: RegistrationCeremony) -> Result<String, String> {
        let now = unix_timestamp();
        let mut pending = self
            .registrations
            .lock()
            .map_err(|_| "registration ceremony lock poisoned".to_string())?;
        pending.retain(|_, item| item.expires_at > now);
        if pending.len() >= MAX_PENDING_CEREMONIES {
            return Err("too many pending registration ceremonies".into());
        }
        let id = generate_secret_token();
        pending.insert(id.clone(), ceremony);
        Ok(id)
    }

    fn take_registration(&self, id: &str) -> Result<RegistrationCeremony, String> {
        let item = self
            .registrations
            .lock()
            .map_err(|_| "registration ceremony lock poisoned".to_string())?
            .remove(id)
            .ok_or_else(|| "registration ceremony is missing or already used".to_string())?;
        if item.expires_at <= unix_timestamp() {
            return Err("registration ceremony has expired".into());
        }
        Ok(item)
    }

    fn store_authentication(&self, ceremony: AuthenticationCeremony) -> Result<String, String> {
        let now = unix_timestamp();
        let mut pending = self
            .authentications
            .lock()
            .map_err(|_| "authentication ceremony lock poisoned".to_string())?;
        pending.retain(|_, item| item.expires_at > now);
        if pending.len() >= MAX_PENDING_CEREMONIES {
            return Err("too many pending authentication ceremonies".into());
        }
        let id = generate_secret_token();
        pending.insert(id.clone(), ceremony);
        Ok(id)
    }

    fn take_authentication(&self, id: &str) -> Result<AuthenticationCeremony, String> {
        let item = self
            .authentications
            .lock()
            .map_err(|_| "authentication ceremony lock poisoned".to_string())?
            .remove(id)
            .ok_or_else(|| "authentication ceremony is missing or already used".to_string())?;
        if item.expires_at <= unix_timestamp() {
            return Err("authentication ceremony has expired".into());
        }
        Ok(item)
    }
}

#[derive(Deserialize)]
pub struct RegisterStartRequest {
    token: String,
}

#[derive(Serialize)]
pub struct CeremonyStartResponse<T> {
    ceremony_id: String,
    #[serde(flatten)]
    options: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
pub struct RegisterFinishRequest {
    ceremony_id: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Deserialize)]
pub struct LoginFinishRequest {
    ceremony_id: String,
    credential: PublicKeyCredential,
}

#[derive(Serialize)]
struct AuthUserResponse {
    authenticated: bool,
    user: AuthUser,
    expires_at: i64,
}

#[derive(Serialize)]
struct AuthUser {
    slot: String,
    display_name: String,
}

pub async fn register_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<RegisterStartRequest>,
) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    if !service.validate_origin(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid request origin");
    }
    let token = request.token.trim();
    if token.is_empty() || token.len() > 256 {
        return error_response(StatusCode::BAD_REQUEST, "invalid registration invitation");
    }
    let now = unix_timestamp();
    let invite = match service
        .store
        .find_valid_invite(&hash_secret(token.as_bytes()), now)
    {
        Ok(Some(invite)) => invite,
        Ok(None) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "registration invitation is invalid, expired, or already used",
            )
        }
        Err(error) => {
            tracing::error!(%error, "failed to load passkey invitation");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };

    let user_id = match uuid::Uuid::parse_str(&invite.user.id) {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, "passkey slot has an invalid user id");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let (options, registration) = match service.webauthn.start_passkey_registration(
        user_id,
        &invite.user.slug,
        &invite.user.display_name,
        None,
    ) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "failed to start passkey registration");
            return error_response(
                StatusCode::BAD_REQUEST,
                "could not start passkey registration",
            );
        }
    };
    let ceremony_id = match service.store_registration(RegistrationCeremony {
        state: registration,
        invite_id: invite.id,
        expires_at: now + CEREMONY_TTL_SECONDS,
    }) {
        Ok(id) => id,
        Err(error) => return error_response(StatusCode::TOO_MANY_REQUESTS, &error),
    };

    Json(CeremonyStartResponse {
        ceremony_id,
        options,
        slot: Some(invite.user.slug),
        display_name: Some(invite.user.display_name),
    })
    .into_response()
}

pub async fn register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<RegisterFinishRequest>,
) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    if !service.validate_origin(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid request origin");
    }
    // Remove the challenge before verification. Any finish attempt burns it, including failures.
    let ceremony = match service.take_registration(&request.ceremony_id) {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };
    let passkey = match service
        .webauthn
        .finish_passkey_registration(&request.credential, &ceremony.state)
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "passkey registration verification failed");
            return error_response(StatusCode::UNAUTHORIZED, "passkey registration failed");
        }
    };
    let credential_id = passkey.cred_id().as_ref().to_vec();
    let serialized = match serde_json::to_string(&passkey) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to serialize passkey");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let user = match service.store.consume_invite_and_store_passkey(
        &ceremony.invite_id,
        &credential_id,
        &serialized,
        unix_timestamp(),
    ) {
        Ok(user) => user,
        Err(error) => {
            tracing::warn!(%error, "failed to consume passkey invitation");
            return error_response(StatusCode::CONFLICT, "invitation is no longer valid");
        }
    };
    session_response(service, &user, &credential_id)
}

pub async fn login_start(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    if !service.validate_origin(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid request origin");
    }
    let stored = match service.store.active_passkeys() {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => return error_response(StatusCode::UNAUTHORIZED, "no passkeys are registered"),
        Err(error) => {
            tracing::error!(%error, "failed to list active passkeys");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let passkeys = match stored
        .iter()
        .map(|item| serde_json::from_str::<Passkey>(&item.passkey_json))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to deserialize stored passkey");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let (options, authentication) = match service.webauthn.start_passkey_authentication(&passkeys) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "failed to start passkey authentication");
            return error_response(StatusCode::BAD_REQUEST, "could not start passkey login");
        }
    };
    let ceremony_id = match service.store_authentication(AuthenticationCeremony {
        state: authentication,
        expires_at: unix_timestamp() + CEREMONY_TTL_SECONDS,
    }) {
        Ok(id) => id,
        Err(error) => return error_response(StatusCode::TOO_MANY_REQUESTS, &error),
    };
    Json(CeremonyStartResponse {
        ceremony_id,
        options,
        slot: None,
        display_name: None,
    })
    .into_response()
}

pub async fn login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<LoginFinishRequest>,
) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    if !service.validate_origin(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid request origin");
    }
    // Remove before verification to make the challenge single-attempt and replay-safe.
    let ceremony = match service.take_authentication(&request.ceremony_id) {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };
    let result = match service
        .webauthn
        .finish_passkey_authentication(&request.credential, &ceremony.state)
    {
        Ok(value) if value.user_verified() => value,
        Ok(_) => return error_response(StatusCode::UNAUTHORIZED, "user verification is required"),
        Err(error) => {
            tracing::warn!(%error, "passkey authentication verification failed");
            return error_response(StatusCode::UNAUTHORIZED, "passkey login failed");
        }
    };
    let credential_id = result.cred_id().as_ref().to_vec();
    let stored = match service.store.active_passkey_by_credential(&credential_id) {
        Ok(Some(value)) => value,
        Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "passkey has been revoked"),
        Err(error) => {
            tracing::error!(%error, "failed to load authenticated passkey");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let mut passkey: Passkey = match serde_json::from_str(&stored.passkey_json) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to deserialize authenticated passkey");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    if passkey.update_credential(&result).is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "passkey credential mismatch");
    }
    let updated = match serde_json::to_string(&passkey) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to serialize authenticated passkey");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    if let Err(error) = service
        .store
        .update_passkey(&credential_id, &updated, unix_timestamp())
    {
        tracing::error!(%error, "failed to update authenticated passkey");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication store failure",
        );
    }
    session_response(service, &stored.user, &credential_id)
}

pub async fn auth_check(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return Json(serde_json::json!({"enabled": false, "authenticated": false})).into_response();
    };
    match service.validate_request_session(&headers) {
        Some(principal) => Json(serde_json::json!({
            "enabled": true,
            "authenticated": true,
            "user": {
                "slot": principal.user.slug,
                "display_name": principal.user.display_name,
            },
            "expires_at": principal.expires_at,
        }))
        .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"enabled": true, "authenticated": false})),
        )
            .into_response(),
    }
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    if !service.validate_origin(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid request origin");
    }
    if let Some(token) = extract_session_cookie(&headers) {
        if let Err(error) = service
            .store
            .revoke_session(&hash_secret(token.as_bytes()), unix_timestamp())
        {
            tracing::error!(%error, "failed to revoke dashboard session");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    }
    let mut response = Json(serde_json::json!({"ok": true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-openfang_session=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict",
        ),
    );
    response
}

fn session_response(
    service: &PasskeyAuthService,
    user: &DashboardUser,
    credential_id: &[u8],
) -> Response<Body> {
    let (token, expires_at) = match service.issue_session(user, credential_id) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to issue dashboard session");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication store failure",
            );
        }
    };
    let max_age = service.session_ttl_hours * 3600;
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Max-Age={max_age}; Path=/; Secure; HttpOnly; SameSite=Strict"
    );
    let mut response = Json(AuthUserResponse {
        authenticated: true,
        user: AuthUser {
            slot: user.slug.clone(),
            display_name: user.display_name.clone(),
        },
        expires_at,
    })
    .into_response();
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().insert(header::SET_COOKIE, value);
            response
        }
        Err(error) => {
            tracing::error!(%error, "failed to build dashboard session cookie");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "authentication failure")
        }
    }
}

pub fn extract_session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name == SESSION_COOKIE && !value.is_empty()).then_some(value))
}

pub fn generate_secret_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_secret(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

pub fn registration_url(origin: &str, token: &str) -> String {
    format!("{}/register#{token}", origin.trim_end_matches('/'))
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn auth_disabled_response() -> Response<Body> {
    error_response(StatusCode::NOT_FOUND, "passkey authentication is disabled")
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_memory::MemorySubstrate;

    fn config() -> AuthConfig {
        AuthConfig {
            enabled: true,
            rp_id: "denis-openfang.moone.dev".into(),
            rp_origin: "https://denis-openfang.moone.dev".into(),
            rp_name: "OpenFang DenisAgency".into(),
            session_ttl_hours: 168,
        }
    }

    #[test]
    fn rejects_non_https_or_non_exact_origins() {
        let memory = MemorySubstrate::open_in_memory(0.1).unwrap();
        let mut invalid = config();
        invalid.rp_origin = "http://denis-openfang.moone.dev".into();
        assert!(PasskeyAuthService::new(&invalid, memory.dashboard_auth().clone()).is_err());

        invalid.rp_origin = "https://extra.denis-openfang.moone.dev".into();
        assert!(PasskeyAuthService::new(&invalid, memory.dashboard_auth().clone()).is_err());

        invalid.rp_origin = "https://denis-openfang.moone.dev/path".into();
        assert!(PasskeyAuthService::new(&invalid, memory.dashboard_auth().clone()).is_err());
    }

    #[test]
    fn tokens_have_256_bits_and_urls_use_fragments() {
        let token = generate_secret_token();
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&token)
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            registration_url("https://denis-openfang.moone.dev", &token),
            format!("https://denis-openfang.moone.dev/register#{token}")
        );
    }

    #[test]
    fn session_cookie_is_opaque_hashed_and_expires_server_side() {
        let memory = MemorySubstrate::open_in_memory(0.1).unwrap();
        let store = memory.dashboard_auth().clone();
        let invite = store
            .create_invite("slava", "Слава", b"invite", 100, 200)
            .unwrap();
        let user = store
            .consume_invite_and_store_passkey(&invite.id, b"credential", "{}", 110)
            .unwrap();
        let token = generate_secret_token();
        store
            .create_session(
                &hash_secret(token.as_bytes()),
                &user.id,
                b"credential",
                120,
                200,
            )
            .unwrap();

        assert!(store
            .validate_session(token.as_bytes(), 150)
            .unwrap()
            .is_none());
        assert!(store
            .validate_session(&hash_secret(token.as_bytes()), 150)
            .unwrap()
            .is_some());
        assert!(store
            .validate_session(&hash_secret(token.as_bytes()), 201)
            .unwrap()
            .is_none());

        let service = PasskeyAuthService::new(&config(), store).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("other=value; {SESSION_COOKIE}={token}")
                .parse()
                .unwrap(),
        );
        // The fixture timestamps are intentionally historical, so service-level
        // validation rejects it even though extraction succeeds.
        assert_eq!(extract_session_cookie(&headers), Some(token.as_str()));
        assert!(service.validate_request_session(&headers).is_none());
    }
}

// ────────────────────────────── приглашения ──────────────────────────────
//
// Ради чего это существует: доступ к панели раздаётся ссылкой, которую можно
// переслать себе или коллеге — в том числе руками агента, у которого есть ключ
// демона. Никаких заранее заведённых людей в коде нет: слот создаётся в момент
// выдачи приглашения.
//
// Маршруты НЕ входят в публичный список middleware, поэтому позвать их можно
// только с ключом демона или из уже открытой сессии по пасскею. Проверять это
// здесь повторно не нужно — и не следует: вторая проверка рядом с первой рано
// или поздно разойдётся с ней.

#[derive(serde::Deserialize)]
pub struct InviteRequest {
    /// Короткое имя слота: [a-z0-9-], оно же ключ, по которому его отзывают.
    pub slug: String,
    /// Как показывать человека. Если не задано — берётся slug.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Сколько часов жива ссылка. По умолчанию сутки.
    #[serde(default)]
    pub ttl_hours: Option<u64>,
}

#[derive(serde::Serialize)]
struct InviteResponse {
    slug: String,
    display_name: String,
    /// Ссылка целиком, вместе с токеном во фрагменте. Показывается один раз:
    /// в базе лежит только её хеш, восстановить ссылку нельзя.
    url: String,
    expires_at: i64,
}

pub async fn invite_create(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<InviteRequest>,
) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    let slug = req.slug.trim().to_string();
    let display_name = req
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&slug)
        .to_string();
    let ttl = req.ttl_hours.unwrap_or(24);

    match service.issue_invite(&slug, &display_name, ttl) {
        Ok((url, expires_at)) => json_response(
            StatusCode::CREATED,
            &InviteResponse {
                slug,
                display_name,
                url,
                expires_at,
            },
        ),
        Err(message) => error_response(StatusCode::BAD_REQUEST, &message),
    }
}

pub async fn invite_list(State(state): State<Arc<AppState>>) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    match service.list_slots() {
        Ok(slots) => json_response(StatusCode::OK, &slots),
        Err(message) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

pub async fn invite_revoke(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response<Body> {
    let Some(service) = state.passkey_auth.as_ref() else {
        return auth_disabled_response();
    };
    match service.revoke_slot(slug.trim()) {
        Ok(true) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "revoked": true, "slug": slug.trim() }),
        ),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "no such slot"),
        Err(message) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

fn json_response<T: serde::Serialize>(status: StatusCode, body: &T) -> Response<Body> {
    match serde_json::to_string(body) {
        Ok(text) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(text))
            .unwrap_or_default(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not serialise response: {error}"),
        ),
    }
}
