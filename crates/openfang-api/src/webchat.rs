//! Embedded WebChat UI served as static HTML.
//!
//! The production dashboard is assembled at compile time from separate
//! HTML/CSS/JS files under `static/` using `include_str!()`. This keeps
//! single-binary deployment while allowing organized source files.
//!
//! Features:
//! - Alpine.js SPA with hash-based routing (10 panels)
//! - Dark/light theme toggle with system preference detection
//! - Responsive layout with collapsible sidebar
//! - Markdown rendering + syntax highlighting (bundled locally)
//! - WebSocket real-time chat with HTTP fallback
//! - Agent management, workflows, memory browser, audit log, and more

use axum::http::header;
use axum::response::IntoResponse;

/// Nonce placeholder in compile-time HTML, replaced at request time.
const NONCE_PLACEHOLDER: &str = "__NONCE__";

/// Compile-time ETag based on the crate version.
/// Not used for the dashboard page (nonce prevents caching) but retained
/// for potential future use by static asset handlers.
#[allow(dead_code)]
const ETAG: &str = concat!("\"openfang-", env!("CARGO_PKG_VERSION"), "\"");

/// Embedded logo PNG for single-binary deployment.
const LOGO_PNG: &[u8] = include_bytes!("../static/logo.png");

/// Embedded favicon ICO for browser tabs.
const FAVICON_ICO: &[u8] = include_bytes!("../static/favicon.ico");

/// GET /logo.png — Serve the OpenFang logo.
pub async fn logo_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        LOGO_PNG,
    )
}

/// GET /favicon.ico — Serve the OpenFang favicon.
pub async fn favicon_ico() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        FAVICON_ICO,
    )
}

/// Embedded PWA manifest for installable web app support.
const MANIFEST_JSON: &str = include_str!("../static/manifest.json");

/// Embedded service worker for PWA support.
const SW_JS: &str = include_str!("../static/sw.js");

/// GET /manifest.json — Serve the PWA web app manifest.
pub async fn manifest_json() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        MANIFEST_JSON,
    )
}

/// GET /sw.js — Serve the PWA service worker.
pub async fn sw_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SW_JS,
    )
}

/// GET / — Serve the OpenFang Dashboard single-page application.
///
/// Generates a unique CSP nonce on every request and injects it into both
/// the `<script>` tags and the `Content-Security-Policy` header. This
/// replaces `'unsafe-inline'` so only our own scripts execute.
pub async fn webchat_page() -> impl IntoResponse {
    let nonce = uuid::Uuid::new_v4().to_string();
    let html = WEBCHAT_HTML.replace(NONCE_PLACEHOLDER, &nonce);
    let csp = format!(
        "default-src 'self'; \
         script-src 'self' 'nonce-{nonce}' 'unsafe-eval' https://cdn.jsdelivr.net; \
         style-src 'self' 'unsafe-inline' https://fonts.googleapis.com https://fonts.gstatic.com https://cdn.jsdelivr.net; \
         img-src 'self' data: blob:; \
         connect-src 'self' ws://localhost:* ws://127.0.0.1:* wss://localhost:* wss://127.0.0.1:* https://cdn.jsdelivr.net; \
         font-src 'self' https://fonts.gstatic.com https://cdn.jsdelivr.net; \
         media-src 'self' blob:; \
         frame-src 'self' blob:; \
         object-src 'none'; \
         base-uri 'self'; \
         form-action 'self'"
    );
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::HeaderName::from_static("content-security-policy"),
                csp,
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        html,
    )
}

/// GET /login — standalone passkey login page.
pub async fn login_page() -> impl IntoResponse {
    auth_html_response(login_html())
}

/// GET /register — standalone invitation-based passkey enrollment page.
/// The invitation remains in the URL fragment and is sent only in the POST body.
pub async fn register_page() -> impl IntoResponse {
    auth_html_response(register_html())
}

fn auth_html_response(template: String) -> impl IntoResponse {
    let nonce = uuid::Uuid::new_v4().to_string();
    let html = template.replace(NONCE_PLACEHOLDER, &nonce);
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; \
         img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'; object-src 'none'"
    );
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::HeaderName::from_static("content-security-policy"),
                csp,
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (
                header::HeaderName::from_static("referrer-policy"),
                "no-referrer".to_string(),
            ),
        ],
        html,
    )
}

const AUTH_STYLE: &str = r#"
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#08090d;color:#f5f5f7;font:15px/1.5 Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}.card{width:min(430px,calc(100vw - 32px));padding:36px;border:1px solid #252833;border-radius:20px;background:linear-gradient(145deg,#151721,#0e1017);box-shadow:0 25px 80px #0008}.brand{display:flex;align-items:center;gap:12px;color:#ff7a1a;font:700 13px/1 monospace;letter-spacing:.18em}.brand img{width:32px;height:32px}.eyebrow{margin:30px 0 8px;color:#8f94a3;font-size:12px;text-transform:uppercase;letter-spacing:.12em}h1{margin:0 0 12px;font-size:30px;line-height:1.15}p{margin:0 0 24px;color:#a8adba}.slot{margin:-8px 0 24px;padding:12px 14px;border:1px solid #2c3040;border-radius:11px;background:#0a0c12;color:#e4e6ec}.slot strong{display:block;color:#fff}.button{width:100%;border:0;border-radius:12px;padding:14px 18px;background:#ff6b12;color:white;font:700 15px/1 inherit;cursor:pointer}.button:hover{background:#ff7d2f}.button:disabled{opacity:.55;cursor:wait}.status{min-height:24px;margin:18px 0 0;color:#a8adba}.status.error{color:#ff8d8d}.foot{margin-top:22px;color:#686d7a;font-size:12px}code{color:#c6cad5}@media(max-width:480px){.card{padding:26px}}
"#;

const AUTH_SCRIPT: &str = r#"
function b64url(buffer){var bytes=new Uint8Array(buffer),s='';for(var i=0;i<bytes.length;i++)s+=String.fromCharCode(bytes[i]);return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'')}
function bytes(value){var s=value.replace(/-/g,'+').replace(/_/g,'/');while(s.length%4)s+='=';var raw=atob(s),out=new Uint8Array(raw.length);for(var i=0;i<raw.length;i++)out[i]=raw.charCodeAt(i);return out}
function creationOptions(value){var o=value.publicKey;o.challenge=bytes(o.challenge);o.user.id=bytes(o.user.id);if(o.excludeCredentials)o.excludeCredentials.forEach(function(c){c.id=bytes(c.id)});return o}
function requestOptions(value){var o=value.publicKey;o.challenge=bytes(o.challenge);if(o.allowCredentials)o.allowCredentials.forEach(function(c){c.id=bytes(c.id)});return o}
function registrationCredential(c){return{id:c.id,rawId:b64url(c.rawId),type:c.type,response:{attestationObject:b64url(c.response.attestationObject),clientDataJSON:b64url(c.response.clientDataJSON),transports:c.response.getTransports?c.response.getTransports():[]},clientExtensionResults:c.getClientExtensionResults()}}
function authenticationCredential(c){return{id:c.id,rawId:b64url(c.rawId),type:c.type,response:{authenticatorData:b64url(c.response.authenticatorData),clientDataJSON:b64url(c.response.clientDataJSON),signature:b64url(c.response.signature),userHandle:c.response.userHandle?b64url(c.response.userHandle):null},clientExtensionResults:c.getClientExtensionResults()}}
async function post(path,body){var r=await fetch(path,{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json'},body:JSON.stringify(body||{})});var data=await r.json().catch(function(){return{}});if(!r.ok)throw new Error(data.error||'Request failed');return data}
function supported(){return window.isSecureContext&&window.PublicKeyCredential&&navigator.credentials}
"#;

fn login_html() -> String {
    [
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Вход — OpenFang</title><link rel="icon" href="/favicon.ico"><style>"#,
        AUTH_STYLE,
        r#"</style></head><body><main class="card"><div class="brand"><img src="/logo.png" alt="">OPENFANG</div><div class="eyebrow">DenisAgency</div><h1>Вход по passkey</h1><p>Используйте Face ID, Touch ID, Windows Hello или PIN доверенного устройства.</p><button class="button" id="action">Войти по passkey</button><div class="status" id="status" role="status"></div><div class="foot">Пароль и API-ключ для браузера не используются.</div></main><script nonce="__NONCE__">"#,
        AUTH_SCRIPT,
        r#"var button=document.getElementById('action'),status=document.getElementById('status');if(!supported()){button.disabled=true;status.className='status error';status.textContent='Этот браузер или контекст не поддерживает passkey.'}button.addEventListener('click',async function(){button.disabled=true;status.className='status';status.textContent='Подтвердите вход на устройстве…';try{var start=await post('/api/auth/passkey/login/start',{});var credential=await navigator.credentials.get({publicKey:requestOptions(start)});await post('/api/auth/passkey/login/finish',{ceremony_id:start.ceremony_id,credential:authenticationCredential(credential)});window.location.replace('/')}catch(e){status.className='status error';status.textContent=e.message||'Не удалось войти';button.disabled=false}});</script></body></html>"#,
    ]
    .concat()
}

fn register_html() -> String {
    [
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Регистрация passkey — OpenFang</title><link rel="icon" href="/favicon.ico"><style>"#,
        AUTH_STYLE,
        r#"</style></head><body><main class="card"><div class="brand"><img src="/logo.png" alt="">OPENFANG</div><div class="eyebrow">Доверенное устройство</div><h1>Привязать passkey</h1><p>Ссылка одноразовая и действует 72 часа. После регистрации вы сразу попадёте в админку.</p><div class="slot" id="slot">Проверим приглашение после нажатия.</div><button class="button" id="action">Создать passkey</button><div class="status" id="status" role="status"></div><div class="foot">Для подтверждения потребуется биометрия или PIN устройства.</div></main><script nonce="__NONCE__">"#,
        AUTH_SCRIPT,
        r#"var token='';try{token=decodeURIComponent(window.location.hash.slice(1))}catch(e){}if(token)history.replaceState(null,'','/register');var button=document.getElementById('action'),status=document.getElementById('status'),slot=document.getElementById('slot');if(!supported()){button.disabled=true;status.className='status error';status.textContent='Этот браузер или контекст не поддерживает passkey.'}else if(!token){button.disabled=true;status.className='status error';status.textContent='В ссылке нет токена приглашения.'}button.addEventListener('click',async function(){button.disabled=true;status.className='status';status.textContent='Проверяем приглашение…';try{var start=await post('/api/auth/passkey/register/start',{token:token});slot.innerHTML='<strong></strong><span></span>';slot.querySelector('strong').textContent=start.display_name;slot.querySelector('span').textContent='Слот: '+start.slot;status.textContent='Подтвердите создание passkey на устройстве…';var credential=await navigator.credentials.create({publicKey:creationOptions(start)});await post('/api/auth/passkey/register/finish',{ceremony_id:start.ceremony_id,credential:registrationCredential(credential)});token='';window.location.replace('/')}catch(e){status.className='status error';status.textContent=e.message||'Не удалось создать passkey';button.disabled=false}});</script></body></html>"#,
    ]
    .concat()
}

/// The embedded HTML/CSS/JS for the OpenFang Dashboard.
///
/// Assembled at compile time from organized static files.
/// All vendor libraries (Alpine.js, marked.js, highlight.js) are bundled
/// locally — no CDN dependency. Alpine.js is included LAST because it
/// immediately processes x-data directives and fires alpine:init on load.
/// KaTeX is loaded dynamically from jsdelivr CDN when needed for LaTeX rendering.
const WEBCHAT_HTML: &str = concat!(
    include_str!("../static/index_head.html"),
    "<style>\n",
    include_str!("../static/css/theme.css"),
    "\n",
    include_str!("../static/css/layout.css"),
    "\n",
    include_str!("../static/css/components.css"),
    "\n",
    include_str!("../static/vendor/github-dark.min.css"),
    "\n</style>\n",
    include_str!("../static/index_body.html"),
    // Vendor libs: marked + highlight first (used by app.js), then Chart.js
    "<script nonce=\"__NONCE__\">\n",
    include_str!("../static/vendor/marked.min.js"),
    "\n</script>\n",
    "<script nonce=\"__NONCE__\">\n",
    include_str!("../static/vendor/highlight.min.js"),
    "\n</script>\n",
    "<script nonce=\"__NONCE__\">\n",
    include_str!("../static/vendor/chart.umd.min.js"),
    "\n</script>\n",
    // App code
    "<script nonce=\"__NONCE__\">\n",
    include_str!("../static/js/api.js"),
    "\n",
    include_str!("../static/js/app.js"),
    "\n",
    include_str!("../static/js/pages/overview.js"),
    "\n",
    include_str!("../static/js/katex.js"),
    "\n",
    include_str!("../static/js/pages/chat.js"),
    "\n",
    include_str!("../static/js/pages/agents.js"),
    "\n",
    include_str!("../static/js/pages/workflows.js"),
    "\n",
    include_str!("../static/js/pages/workflow-builder.js"),
    "\n",
    include_str!("../static/js/pages/channels.js"),
    "\n",
    include_str!("../static/js/pages/skills.js"),
    "\n",
    include_str!("../static/js/pages/hands.js"),
    "\n",
    include_str!("../static/js/pages/scheduler.js"),
    "\n",
    include_str!("../static/js/pages/settings.js"),
    "\n",
    include_str!("../static/js/pages/usage.js"),
    "\n",
    include_str!("../static/js/pages/sessions.js"),
    "\n",
    include_str!("../static/js/pages/logs.js"),
    "\n",
    include_str!("../static/js/pages/wizard.js"),
    "\n",
    include_str!("../static/js/pages/approvals.js"),
    "\n",
    include_str!("../static/js/pages/comms.js"),
    "\n",
    include_str!("../static/js/pages/runtime.js"),
    "\n</script>\n",
    // Alpine.js MUST be last — it processes x-data and fires alpine:init
    "<script nonce=\"__NONCE__\">\n",
    include_str!("../static/vendor/alpine.min.js"),
    "\n</script>\n",
    "</body></html>"
);

#[cfg(test)]
mod tests {
    #[test]
    fn dashboard_does_not_handle_machine_api_keys() {
        let api_js = include_str!("../static/js/api.js");

        assert!(!api_js.contains("X-API-Key"));
        assert!(!api_js.contains("_authToken"));
        assert!(!api_js.contains("openfang-api-key"));
    }
}
