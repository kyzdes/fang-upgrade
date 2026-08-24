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

use crate::routes::AppState;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use std::sync::Arc;

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
///
/// The name above the heading comes from `auth.rp_name` in the config, not from
/// this source file: the same binary serves every installation.
pub async fn login_page(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    auth_html_response(login_html(&state.kernel.config.auth.rp_name))
}

/// GET /register — standalone invitation-based passkey enrollment page.
/// The invitation remains in the URL fragment and is sent only in the POST body.
///
/// The name above the heading comes from `auth.rp_name`, exactly as on `/login`:
/// the same binary serves every installation, and both entrances say the same
/// thing about which one this is.
pub async fn register_page(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    auth_html_response(register_html(&state.kernel.config.auth.rp_name))
}

/// Словесный знак продукта в шапке карточки входа. Тот же литерал стоит в
/// разметке обеих страниц; что они не разошлись, проверяет
/// `brand_wordmark_matches_the_markup`.
const BRAND_WORDMARK: &str = "OPENFANG";

/// Render the installation name above the heading of an auth page.
///
/// Two cases render nothing at all rather than a box:
/// * empty or whitespace-only `auth.rp_name` — there is nothing to draw;
/// * a name equal to [`BRAND_WORDMARK`] (the default `auth.rp_name` is
///   `OpenFang`) — the wordmark is already on the card, and a second storey
///   repeating it carries no information.
///
/// The value is operator-supplied config, so it is HTML-escaped.
fn eyebrow(rp_name: &str) -> String {
    let name = rp_name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(BRAND_WORDMARK) {
        String::new()
    } else {
        format!(
            r#"<div class="eyebrow">{}</div>"#,
            html_escape::encode_text(name)
        )
    }
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
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#08090d;color:#f5f5f7;font:15px/1.5 Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}.card{width:min(430px,calc(100vw - 32px));padding:36px;border:1px solid #252833;border-radius:20px;background:linear-gradient(145deg,#151721,#0e1017);box-shadow:0 25px 80px #0008}.brand{display:flex;align-items:center;gap:12px;margin-bottom:28px;color:#ff7a1a;font:700 13px/1 monospace;letter-spacing:.18em}.brand img{width:32px;height:32px}.eyebrow{margin:0 0 8px;color:#8f94a3;font-size:12px;text-transform:uppercase;letter-spacing:.12em}h1{margin:0 0 12px;font-size:30px;line-height:1.15}p{margin:0 0 24px;color:#a8adba}.slot{margin:-8px 0 24px;padding:12px 14px;border:1px solid #2c3040;border-radius:11px;background:#0a0c12;color:#e4e6ec}.slot strong{display:block;color:#fff}.button{width:100%;border:0;border-radius:12px;padding:14px 18px;background:#ff6b12;color:white;font:700 15px/1 inherit;cursor:pointer}.button:hover{background:#ff7d2f}.button:disabled{opacity:.55;cursor:wait}.status{min-height:24px;margin:18px 0 0;color:#a8adba}.status.error{color:#ff8d8d}.foot{margin-top:22px;color:#686d7a;font-size:12px}code{color:#c6cad5}@media(max-width:480px){.card{padding:26px}}
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

fn login_html(rp_name: &str) -> String {
    [
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Sign in — OpenFang</title><link rel="icon" href="/favicon.ico"><style>"#,
        AUTH_STYLE,
        r#"</style></head><body><main class="card"><div class="brand"><img src="/logo.png" alt="">OPENFANG</div>"#,
        eyebrow(rp_name).as_str(),
        r#"<h1>Sign in with a passkey</h1><p>Use Face ID, Touch ID, Windows Hello, or your trusted device's PIN.</p><button class="button" id="action">Sign in with passkey</button><div class="status" id="status" role="status"></div><div class="foot">No password or API key is used in the browser.</div></main><script nonce="__NONCE__">"#,
        AUTH_SCRIPT,
        r#"var button=document.getElementById('action'),status=document.getElementById('status');if(!supported()){button.disabled=true;status.className='status error';status.textContent='This browser or context does not support passkeys.'}button.addEventListener('click',async function(){button.disabled=true;status.className='status';status.textContent='Confirm sign-in on your device…';try{var start=await post('/api/auth/passkey/login/start',{});var credential=await navigator.credentials.get({publicKey:requestOptions(start)});await post('/api/auth/passkey/login/finish',{ceremony_id:start.ceremony_id,credential:authenticationCredential(credential)});window.location.replace('/')}catch(e){status.className='status error';status.textContent=e.message||'Sign-in failed';button.disabled=false}});</script></body></html>"#,
    ]
    .concat()
}

fn register_html(rp_name: &str) -> String {
    [
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Register passkey — OpenFang</title><link rel="icon" href="/favicon.ico"><style>"#,
        AUTH_STYLE,
        r#"</style></head><body><main class="card"><div class="brand"><img src="/logo.png" alt="">OPENFANG</div>"#,
        eyebrow(rp_name).as_str(),
        r#"<h1>Link a passkey</h1><p>This link is single-use and valid for 72 hours. You'll land straight in the admin panel after registering.</p><div class="slot" id="slot">We'll check the invitation once you press the button.</div><button class="button" id="action">Create passkey</button><div class="status" id="status" role="status"></div><div class="foot">Confirming requires biometrics or your device's PIN.</div></main><script nonce="__NONCE__">"#,
        AUTH_SCRIPT,
        r#"var token='';try{token=decodeURIComponent(window.location.hash.slice(1))}catch(e){}if(token)history.replaceState(null,'','/register');var button=document.getElementById('action'),status=document.getElementById('status'),slot=document.getElementById('slot');if(!supported()){button.disabled=true;status.className='status error';status.textContent='This browser or context does not support passkeys.'}else if(!token){button.disabled=true;status.className='status error';status.textContent='This link has no invitation token.'}button.addEventListener('click',async function(){button.disabled=true;status.className='status';status.textContent='Checking the invitation…';try{var start=await post('/api/auth/passkey/register/start',{token:token});slot.innerHTML='<strong></strong><span></span>';slot.querySelector('strong').textContent=start.display_name;slot.querySelector('span').textContent='Slot: '+start.slot;status.textContent='Confirm passkey creation on your device…';var credential=await navigator.credentials.create({publicKey:creationOptions(start)});await post('/api/auth/passkey/register/finish',{ceremony_id:start.ceremony_id,credential:registrationCredential(credential)});token='';window.location.replace('/')}catch(e){status.className='status error';status.textContent=e.message||'Could not create passkey';button.disabled=false}});</script></body></html>"#,
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
    use super::*;

    /// Оба пути входа живут в панели одновременно, и это решение, а не недоделка.
    ///
    /// Пасскей — основной путь и единственный снаружи: он ходит сессионной кукой,
    /// которую ставит `login_finish`. Заголовок `Authorization: Bearer <api_key>`
    /// остаётся запасным входом с локального адреса и тайлнета.
    ///
    /// Порт из публичного форка удалял браузерный путь `api_key` безусловно, и в
    /// нашей конфигурации (api_key задан, секции [auth] нет) это замыкало вход:
    /// `/api/auth/check` → 401 → `/login` → 401, а ключ вводить некуда. Тот же
    /// коммит нёс тест, требовавший ОТСУТСТВИЯ `_authToken`; здесь он заменён на
    /// проверку обратного, чтобы следующий перенос не убрал запасной вход молча.
    #[test]
    fn dashboard_keeps_both_login_paths() {
        let api_js = include_str!("../static/js/api.js");

        assert!(
            api_js.contains("_authToken"),
            "запасной вход по api_key убран из панели — при отказе пасскея войти будет нельзя"
        );
        assert!(
            api_js.contains("window.location.replace('/login')"),
            "панель не уводит на вход по пасскею при 401 без ключа"
        );
    }

    /// Страница входа не должна нести имя чьей-то установки: репозиторий
    /// публичный, и форк поднимают чужие люди. Имя приходит из `auth.rp_name`.
    #[test]
    fn login_page_shows_the_configured_rp_name_and_no_hardcoded_brand() {
        let html = login_html("Acme Robotics");
        assert!(
            html.contains(r#"<div class="eyebrow">Acme Robotics</div>"#),
            "rp_name must reach the page: {html}"
        );

        assert_eq!(
            html.matches(r#"<div class="eyebrow">"#).count(),
            1,
            "ровно одна надпись над заголовком, и она из конфига"
        );
    }

    /// `auth.rp_name` — значение из конфига оператора, то есть вход, а не
    /// константа. Без экранирования оно закрывает `<div>` и вносит разметку.
    #[test]
    fn rp_name_is_html_escaped() {
        let html = login_html("</div><script>alert(1)</script>");
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// Умолчание `auth.rp_name` — «OpenFang», и словесный знак в шапке карточки
    /// тоже «OPENFANG». На установке, которая имя не меняла, страница печатала
    /// его дважды: знак и под ним eyebrow. Второй этаж не несёт информации.
    #[test]
    fn the_product_name_is_not_printed_twice_by_default() {
        for value in ["OpenFang", "openfang", "  OPENFANG  "] {
            let html = login_html(value);
            assert!(
                !html.contains(r#"class="eyebrow""#),
                "{value:?} совпадает со словесным знаком — eyebrow лишний"
            );
        }
    }

    /// `/register` — вторая страница входа и такой же публичный экран, как
    /// `/login`. Имя на ней было прибито строкой, то есть `auth.rp_name`
    /// оператора туда не доходил вовсе.
    #[test]
    fn register_page_carries_the_configured_rp_name() {
        let html = register_html("Acme Robotics");
        assert!(
            !html.contains("Доверенное устройство"),
            "имя на /register прибито в исходник, а не взято из auth.rp_name"
        );
        assert!(
            html.contains(r#"<div class="eyebrow">Acme Robotics</div>"#),
            "rp_name must reach /register: {html}"
        );
        assert_eq!(
            html.matches(r#"<div class="eyebrow">"#).count(),
            1,
            "ровно одна надпись над заголовком, и она из конфига"
        );
        // Экранирование и умолчание — те же правила, что на /login.
        assert!(!register_html("</div><script>alert(1)</script>").contains("<script>alert(1)"));
        for value in ["", "   ", "OpenFang"] {
            assert!(
                !register_html(value).contains(r#"class="eyebrow""#),
                "{value:?} should render no eyebrow on /register"
            );
        }
    }

    /// Правило «не печатать имя дважды» сравнивает `rp_name` с константой.
    /// Если разметку поменяют, а константу нет, правило замолчит.
    #[test]
    fn brand_wordmark_matches_the_markup() {
        let needle = format!(r#"alt="">{BRAND_WORDMARK}</div>"#);
        assert!(
            login_html("Acme").contains(&needle),
            "/login разошёлся с BRAND_WORDMARK"
        );
        assert!(
            register_html("Acme").contains(&needle),
            "/register разошёлся с BRAND_WORDMARK"
        );
    }

    /// Пустое имя даёт пустую коробку в вёрстке — рисовать нечего, значит и
    /// элемента быть не должно.
    #[test]
    fn empty_rp_name_renders_no_eyebrow_at_all() {
        for value in ["", "   ", "\t"] {
            let html = login_html(value);
            assert!(
                !html.contains(r#"class="eyebrow""#),
                "{value:?} should render no eyebrow"
            );
        }
    }

    /// До этой правки весь зазор между строкой логотипа и заголовком держался
    /// на `margin-top` у `.eyebrow`. При умолчании `rp_name = "OpenFang"`
    /// `eyebrow()` возвращает пустую строку, и разметка схлопывалась: у
    /// `.brand` не было ни `margin-bottom`, ни `padding-bottom`, у `h1` —
    /// `margin: 0 0 12px`. Зазор обязан идти от `.brand`, а не от элемента,
    /// который на дефолтной установке не рисуется вовсе.
    #[test]
    fn brand_carries_its_own_spacing_so_the_gap_survives_without_an_eyebrow() {
        let brand_rule = AUTH_STYLE
            .split(".brand{")
            .nth(1)
            .expect(".brand rule must exist in AUTH_STYLE")
            .split('}')
            .next()
            .unwrap();
        assert!(
            brand_rule.contains("margin-bottom") || brand_rule.contains("padding-bottom"),
            ".brand must carry its own bottom spacing: {brand_rule}"
        );
    }

    /// Обе страницы входа видит чужой оператор форка, который может не знать
    /// русский: разметка и текст обязаны быть на английском, как остальной
    /// продукт (`static/index_head.html` несёт `lang="en"`).
    #[test]
    fn login_and_register_pages_declare_english() {
        assert!(login_html("Acme").starts_with(r#"<!doctype html><html lang="en">"#));
        assert!(register_html("Acme").starts_with(r#"<!doctype html><html lang="en">"#));
    }
}
