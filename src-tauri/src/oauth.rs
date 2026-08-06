//! System-browser Google sign-in: standard installed-app OAuth 2.0
//! authorization-code + PKCE loopback flow (RFC 8252 / RFC 7636).
//!
//! WHY a loopback flow instead of the embedded-webview popup/redirect the
//! web app's own `signInWithGoogle` otherwise uses: Google's policy blocks
//! OAuth inside embedded webviews (`disallowed_useragent`), which the Tauri
//! shell's WebView2 instance can trip — and even where it doesn't, the user
//! is forced to re-type their Google password despite their actual default
//! browser already holding a live Google session. This flow instead:
//!   1. opens the user's REAL default browser at Google's consent screen
//!      (`google_sign_in`, called from `commands::google_sign_in`)
//!   2. binds an ephemeral localhost TCP listener as the redirect target
//!   3. accepts the one browser-initiated callback request, verifies
//!      `state` (CSRF), and exchanges the authorization code for tokens
//!   4. hands `{ idToken, email }` back across IPC; the web side turns that
//!      into a Firebase credential via `GoogleAuthProvider.credential`.
//!
//! `client_id`/`client_secret` never appear in source: they're read via
//! `option_env!` at compile time, populated by `build.rs` from
//! `desktop/.env.oauth` locally or the real process env in CI — see that
//! file's doc comment for the full injection story.
//!
//! Split as pure (testable without an app/network) vs. impure:
//!   - pure: PKCE derivation/generation, the auth-URL builder, the HTTP
//!     request-line parser, and the id_token email-claim decoder.
//!   - impure: the loopback TCP accept loop and the token-exchange POST —
//!     exercised end-to-end by `google_sign_in` itself, not unit-testable
//!     without a live network/OS socket, so left uncovered here (every pure
//!     piece feeding into them is).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// The whole flow gets ~3 minutes before it gives up on the browser tab —
/// long enough for a real human to pick an account and click through
/// consent, short enough that an abandoned tab doesn't leak the listener
/// task (and its bound port) forever.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// RFC 3986 "unreserved" characters (`ALPHA / DIGIT / "-" / "." / "_" /
/// "~"`) left unescaped; everything else — including the space in `scope`
/// — is percent-encoded as `%20` (this is a query string on a GET URL the
/// browser navigates to, not an `application/x-www-form-urlencoded` POST
/// body, so `+` would be the wrong encoding for space here).
const QUERY_UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

fn percent_encode_component(value: &str) -> String {
    utf8_percent_encode(value, QUERY_UNRESERVED).to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("desktop Google sign-in isn't configured in this build")]
    NotConfigured,
    #[error("couldn't start the local sign-in listener: {0}")]
    Io(#[from] std::io::Error),
    #[error("couldn't open your browser: {0}")]
    OpenBrowser(String),
    #[error("sign-in timed out — please try again")]
    TimedOut,
    #[error("sign-in was cancelled")]
    Denied,
    #[error("sign-in failed: {0}")]
    CallbackError(String),
    #[error("sign-in response didn't match the request — please try again")]
    StateMismatch,
    #[error("sign-in response was missing an authorization code")]
    MissingCode,
    #[error("couldn't complete sign-in with Google: {0}")]
    TokenExchange(String),
}

/// Wire shape returned to the web side — the Firebase credential (built
/// JS-side via `GoogleAuthProvider.credential(idToken)`) is all the app
/// needs; `email` is display-only, decoded from the ID token's own claims
/// without verifying its signature — Firebase re-verifies the token itself
/// (signature + audience) when `signInWithCredential` runs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleSignInResult {
    pub id_token: String,
    pub email: Option<String>,
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636) — pure.
// ---------------------------------------------------------------------------

/// A fresh `code_verifier`: 32 random bytes, base64url-no-pad encoded (43
/// chars) — within RFC 7636's required 43-128 char range.
fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh CSRF `state` token: 24 random bytes, base64url-no-pad encoded.
fn generate_state() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `code_challenge = BASE64URL-ENCODE(SHA256(code_verifier))`, no padding —
/// the S256 method RFC 7636 §4.2 specifies.
fn derive_pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

// ---------------------------------------------------------------------------
// Auth URL — pure.
// ---------------------------------------------------------------------------

fn build_auth_url(client_id: &str, redirect_uri: &str, code_challenge: &str, state: &str) -> String {
    let params: [(&str, &str); 8] = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", "openid email profile"),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("prompt", "select_account"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{GOOGLE_AUTH_ENDPOINT}?{query}")
}

// ---------------------------------------------------------------------------
// Loopback callback request-line parsing — pure.
// ---------------------------------------------------------------------------

/// Whatever `code`/`state`/`error` params were present on the callback GET
/// request — absence isn't an error at this layer (the caller, which knows
/// the expected `state`, decides what a missing/mismatched value means).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Parses an HTTP request line — `"GET /?code=..&state=.. HTTP/1.1"` — into
/// its query params. Anything not matching that shape (wrong method, no
/// query string, garbage) yields an all-`None` result rather than an error;
/// the caller treats "no code" uniformly whether that's because the request
/// was malformed or simply didn't carry one.
fn parse_callback_request_line(line: &str) -> CallbackParams {
    let mut params = CallbackParams::default();
    let mut parts = line.split_whitespace();
    let Some("GET") = parts.next() else { return params };
    let Some(target) = parts.next() else { return params };
    let Some((_path, query)) = target.split_once('?') else { return params };

    for pair in query.split('&') {
        let (key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_encoding::percent_decode_str(raw_value).decode_utf8_lossy().into_owned();
        match key {
            "code" => params.code = Some(value),
            "state" => params.state = Some(value),
            "error" => params.error = Some(value),
            _ => {}
        }
    }
    params
}

// ---------------------------------------------------------------------------
// ID token email claim — pure.
// ---------------------------------------------------------------------------

/// Best-effort `email` claim out of an ID token's (unverified here) payload
/// segment — `None` on any shape mismatch rather than an error, since this
/// is display-only. JWT base64url segments are unpadded per spec, but a
/// padded variant is tolerated too rather than trusting spec compliance blindly.
fn decode_email_from_id_token(id_token: &str) -> Option<String> {
    let payload_b64 = id_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("email")?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------------------
// Token exchange — impure (network).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<GoogleSignInResult, OAuthError> {
    let client = reqwest::Client::builder()
        .timeout(TOKEN_EXCHANGE_TIMEOUT)
        .build()
        .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

    let form = [
        ("code", code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
        ("code_verifier", code_verifier),
    ];

    let response = client
        .post(GOOGLE_TOKEN_ENDPOINT)
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| OAuthError::TokenExchange(e.to_string()))?;
    let parsed: TokenResponse =
        serde_json::from_str(&body).map_err(|e| OAuthError::TokenExchange(format!("{e} (body: {body})")))?;

    if !status.is_success() {
        let detail = parsed.error_description.or(parsed.error).unwrap_or_else(|| status.to_string());
        return Err(OAuthError::TokenExchange(detail));
    }
    let id_token = parsed.id_token.ok_or_else(|| OAuthError::TokenExchange("response had no id_token".into()))?;
    let email = decode_email_from_id_token(&id_token);
    Ok(GoogleSignInResult { id_token, email })
}

// ---------------------------------------------------------------------------
// Loopback listener — impure (OS socket + timing).
// ---------------------------------------------------------------------------

const SUCCESS_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>AgileTasker</title>
<style>body{font-family:system-ui,sans-serif;background:#111318;color:#eef0f4;display:flex;
align-items:center;justify-content:center;min-height:100vh;margin:0}
main{text-align:center;padding:2rem}h1{font-size:1.25rem;margin:0 0 .5rem}
p{color:#9aa1ad;margin:0}</style></head>
<body><main><h1>You're signed in</h1><p>You can close this tab and return to AgileTasker.</p></main></body></html>"#;

const FAILURE_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>AgileTasker</title>
<style>body{font-family:system-ui,sans-serif;background:#111318;color:#eef0f4;display:flex;
align-items:center;justify-content:center;min-height:100vh;margin:0}
main{text-align:center;padding:2rem}h1{font-size:1.25rem;margin:0 0 .5rem}
p{color:#9aa1ad;margin:0}</style></head>
<body><main><h1>Sign-in didn't complete</h1><p>You can close this tab and try again from AgileTasker.</p></main></body></html>"#;

/// Accepts exactly one connection on `listener` (or times out), parses its
/// request line, verifies `state`, writes a small static response page, and
/// returns the authorization `code` on success. `listener` is consumed —
/// dropping it (every return path, success or error) closes the bound port
/// rather than leaving it lingering for a stray second request.
async fn accept_callback(listener: TcpListener, expected_state: &str) -> Result<String, OAuthError> {
    let (stream, _addr) = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept()).await.map_err(|_| OAuthError::TimedOut)??;

    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let params = parse_callback_request_line(&request_line);
    let outcome = match params.error.as_deref() {
        Some("access_denied") => Err(OAuthError::Denied),
        Some(other) => Err(OAuthError::CallbackError(other.to_string())),
        None if params.state.as_deref() != Some(expected_state) => Err(OAuthError::StateMismatch),
        None => params.code.clone().ok_or(OAuthError::MissingCode),
    };

    let html = if outcome.is_ok() { SUCCESS_HTML } else { FAILURE_HTML };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let mut stream = reader.into_inner();
    // Best-effort: the OAuth outcome is already decided above and doesn't
    // depend on this write succeeding — the browser tab just might not get
    // a pretty closing page if it fails partway.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    outcome
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Runs the full loopback + PKCE flow: opens the system browser at Google's
/// consent screen and returns once it has redirected back with a code
/// that's been exchanged for tokens (or the flow times out / is cancelled /
/// fails). See the module docs for the step-by-step.
pub async fn google_sign_in(app: &AppHandle) -> Result<GoogleSignInResult, OAuthError> {
    let client_id = option_env!("GOOGLE_OAUTH_CLIENT_ID").ok_or(OAuthError::NotConfigured)?;
    let client_secret = option_env!("GOOGLE_OAUTH_CLIENT_SECRET").ok_or(OAuthError::NotConfigured)?;

    let verifier = generate_code_verifier();
    let challenge = derive_pkce_challenge(&verifier);
    let state = generate_state();

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let auth_url = build_auth_url(client_id, &redirect_uri, &challenge, &state);
    app.opener().open_url(auth_url, None::<&str>).map_err(|e| OAuthError::OpenBrowser(e.to_string()))?;

    let code = accept_callback(listener, &state).await?;
    exchange_code(client_id, client_secret, &code, &verifier, &redirect_uri).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7636 Appendix B known-answer test.
    #[test]
    fn pkce_challenge_matches_rfc7636_known_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(derive_pkce_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_and_state_are_url_safe_and_unpadded() {
        let verifier = generate_code_verifier();
        assert!(verifier.len() >= 43 && verifier.len() <= 128, "len={}", verifier.len());
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        let state = generate_state();
        assert!(!state.is_empty());
        assert!(state.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        // Two calls must not collide (would indicate a broken RNG source).
        assert_ne!(generate_code_verifier(), generate_code_verifier());
        assert_ne!(generate_state(), generate_state());
    }

    #[test]
    fn auth_url_contains_every_required_param_correctly_encoded() {
        let url = build_auth_url(
            "abc123.apps.googleusercontent.com",
            "http://127.0.0.1:54321",
            "challenge-value",
            "state-value",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("client_id=abc123.apps.googleusercontent.com"));
        // `://` in the redirect_uri must be percent-encoded — it's a query
        // *value* here, not a bare URL segment.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321"), "{url}");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope=openid%20email%20profile"), "{url}");
        assert!(url.contains("code_challenge=challenge-value"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-value"));
        assert!(url.contains("prompt=select_account"));
    }

    #[test]
    fn parses_code_and_state_from_request_line() {
        let params = parse_callback_request_line("GET /?code=4%2F0Adeu&state=xyz123 HTTP/1.1\r\n");
        assert_eq!(params.code.as_deref(), Some("4/0Adeu"));
        assert_eq!(params.state.as_deref(), Some("xyz123"));
        assert_eq!(params.error, None);
    }

    #[test]
    fn parses_error_param_on_consent_denial() {
        let params = parse_callback_request_line("GET /?error=access_denied&state=xyz123 HTTP/1.1\r\n");
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.code, None);
    }

    #[test]
    fn missing_query_string_yields_all_none() {
        let params = parse_callback_request_line("GET /favicon.ico HTTP/1.1\r\n");
        assert_eq!(params, CallbackParams::default());
    }

    #[test]
    fn non_get_request_line_yields_all_none() {
        let params = parse_callback_request_line("POST /?code=abc HTTP/1.1\r\n");
        assert_eq!(params, CallbackParams::default());
    }

    #[test]
    fn decodes_email_claim_from_id_token_payload() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"lovro@example.com","sub":"123"}"#);
        let fake_token = format!("header.{payload}.signature");
        assert_eq!(decode_email_from_id_token(&fake_token).as_deref(), Some("lovro@example.com"));
    }

    #[test]
    fn email_decode_is_none_for_malformed_token() {
        assert_eq!(decode_email_from_id_token("not-a-jwt"), None);
        assert_eq!(decode_email_from_id_token(""), None);
    }
}
