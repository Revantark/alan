//! OAuth 2.0 browser sign-in.
//!
//! This module owns the flow mechanics (PKCE, loopback callback capture,
//! authorization URLs, token exchange and refresh), configured through
//! [`OAuthConfig`]. Providers supply their own endpoints, client id, and fixed
//! redirect port; `chatgpt.rs` is the reference consumer.

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Values tied to a provider's registered client (client id, redirect port,
/// scopes) or to a published endpoint, so each is `'static`.
#[derive(Debug, Clone, Copy)]
pub struct OAuthConfig {
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: &'static str,
    /// Absolute redirect URI registered with the provider; its port is the
    /// loopback port bound by [`start_sign_in`].
    pub redirect_uri: &'static str,
    /// Path of [`OAuthConfig::redirect_uri`], used to recognize callbacks.
    pub redirect_path: &'static str,
    /// Fixed redirect port; the provider's client registration pins it.
    pub redirect_port: u16,
    pub scopes: &'static str,
    /// Provider quirks appended to the authorization URL, e.g. captcha
    /// flags demanded by unpublished backends.
    pub extra_authorize_params: &'static [(&'static str, &'static str)],
}

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("OAuth flow failed: {0}")]
    Flow(String),
    #[error(
        "port {0} is already in use; the sign-in redirect target is fixed, \
         so free the port and try again"
    )]
    PortBusy(u16),
    #[error("browser sign-in timed out; no callback arrived on port {0}")]
    CallbackTimeout(u16),
}

/// PKCE pair: S256 challenge over a URL-safe random verifier.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn create_pkce() -> Pkce {
    // 64 random bytes -> 86 base64url characters, within the 43–128 range.
    let mut bytes = [0u8; 64];
    fill_random(&mut bytes);
    let verifier = base64_url(&bytes);
    Pkce {
        challenge: pkce_challenge(&verifier),
        verifier,
    }
}

/// S256 challenge for a PKCE verifier (RFC 7636, appendix B).
fn pkce_challenge(verifier: &str) -> String {
    base64_url(&Sha256::digest(verifier.as_bytes()))
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn fill_random(buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(16) {
        chunk.copy_from_slice(&uuid::Uuid::new_v4().into_bytes()[..chunk.len()]);
    }
}

fn create_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_in: u64,
}

pub async fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
) -> Result<Tokens, OAuthError> {
    token_request(
        config,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", config.redirect_uri),
            ("client_id", config.client_id),
            ("code_verifier", verifier),
        ],
        "code exchange",
    )
    .await
}

pub async fn refresh_tokens(
    config: &OAuthConfig,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    token_request(
        config,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", config.client_id),
        ],
        "token refresh",
    )
    .await
}

/// A hung token endpoint must not block sign-in forever.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Client reused by every token grant, so the TLS session and connection
/// pool are shared instead of rebuilt for each exchange or refresh.
fn token_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TOKEN_REQUEST_TIMEOUT)
            .build()
            .expect("build the token client (fails only on invalid proxy configuration)")
    })
}

async fn token_request(
    config: &OAuthConfig,
    form: &[(&str, &str)],
    what: &str,
) -> Result<Tokens, OAuthError> {
    let body = form
        .iter()
        .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = token_client()
        .post(config.token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| OAuthError::Flow(format!("{what} request failed: {error}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| OAuthError::Flow(format!("{what} response failed: {error}")))?;

    if !status.is_success() {
        // Surface status and the OAuth `error` field; never echo token
        // values (the body here is an error payload, not a token).
        let oauth_error = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        return Err(OAuthError::Flow(match oauth_error {
            Some(error) => format!("{what} returned HTTP {status} ({error})"),
            None => format!("{what} returned HTTP {status}"),
        }));
    }

    serde_json::from_str(&body)
        .map_err(|error| OAuthError::Flow(format!("{what} returned invalid JSON: {error}")))
}

/// The program used to open a URL in the system browser, by platform.
fn browser_opener() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        Some("open")
    }
    #[cfg(target_os = "windows")]
    {
        Some("cmd")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Some("xdg-open")
    }
}

/// Open the system browser at `url`. Failure is not fatal: the URL is shown
/// in the UI so the user can open it by hand.
async fn open_browser(url: &str) {
    let Some(program) = browser_opener() else {
        tracing::debug!(%url, "no known browser opener for this platform");
        return;
    };
    let mut command = std::process::Command::new(program);
    // Windows' `cmd` needs `/C start ""` to launch a URL; other shells do not.
    #[cfg(target_os = "windows")]
    command.arg("/C").arg("start").arg("").arg(url);
    #[cfg(not(target_os = "windows"))]
    command.arg(url);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let spawned = tokio::task::spawn_blocking(move || command.spawn()).await;
    if let Ok(Err(error)) = spawned {
        tracing::debug!(%url, %error, "could not open the browser automatically");
    }
}

const CALLBACK_PAGE: &str = "<html><body><p>Sign-in complete. You can close this tab and return to the terminal.</p></body></html>";

/// Parse a redirect target such as `/auth/callback?code=..&state=..`.
/// Returns the code only when the path matches and the state matches.
pub fn parse_callback(config: &OAuthConfig, target: &str, state: &str) -> Option<String> {
    if !target.starts_with(config.redirect_path) {
        return None;
    }
    let query = target.split('?').nth(1)?;
    let mut code = None;
    let mut state_ok = false;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(value.to_string()),
            "state" if value == state => state_ok = true,
            _ => {}
        }
    }
    match (code, state_ok) {
        (Some(code), true) => Some(code),
        _ => None,
    }
}

/// Accept callbacks on an already-bound listener until one carries a valid
/// code and state, or the timeout elapses.
pub async fn await_callback_on(
    listener: TcpListener,
    config: &OAuthConfig,
    state: &str,
    timeout: Duration,
) -> Result<String, OAuthError> {
    let result = tokio::time::timeout(timeout, async {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|error| {
                OAuthError::Flow(format!("callback listener failed: {error}"))
            })?;
            let mut buffer = Vec::new();
            // Read until the header block ends; browsers may keep the
            // connection open, so do not wait for EOF. The check runs after the
            // bytes are appended, or a request arriving in a single packet
            // would never be answered.
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.map_err(|error| {
                    OAuthError::Flow(format!("callback read failed: {error}"))
                })?;
                buffer.extend_from_slice(&chunk[..read]);
                if read == 0
                    || buffer.windows(4).any(|window| window == b"\r\n\r\n")
                    || buffer.len() > 16 * 1024
                {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buffer).into_owned();
            let target = request
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                CALLBACK_PAGE.len(),
                CALLBACK_PAGE
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;

            if let Some(code) = parse_callback(config, &target, state) {
                return Ok::<String, OAuthError>(code);
            }
            // Wrong state or an error response: keep listening for a valid
            // callback (e.g. a retried sign-in) until the timeout.
            continue;
        }
    })
    .await;

    drop(listener);
    result.unwrap_or(Err(OAuthError::CallbackTimeout(config.redirect_port)))
}

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Build the authorization URL for a PKCE pair and state.
pub fn authorize_url(config: &OAuthConfig, pkce: &Pkce, state: &str) -> String {
    use std::fmt::Write;
    let mut url = String::from(config.authorize_url);
    let _ = write!(
        url,
        "?response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&scope={scope}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}",
        client_id = urlencode(config.client_id),
        redirect_uri = urlencode(config.redirect_uri),
        scope = urlencode(config.scopes),
        challenge = urlencode(&pkce.challenge),
        state = urlencode(state),
    );
    for (key, value) in config.extra_authorize_params {
        let _ = write!(url, "&{}={}", urlencode(key), urlencode(value));
    }
    url
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            other => {
                let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("%{other:02X}"));
            }
        }
    }
    encoded
}

/// A started sign-in flow: the authorization URL to show the user plus the
/// state needed to complete the exchange. The loopback listener is already
/// bound, so the first callback cannot be lost between phases.
pub struct SignInStart {
    pub url: String,
    pkce: Pkce,
    state: String,
    listener: TcpListener,
}

/// Bind the redirect port and build the authorization URL. Does not open a
/// browser yet; call `finish_sign_in` with the returned value.
pub async fn start_sign_in(config: &OAuthConfig) -> Result<SignInStart, OAuthError> {
    let listener = TcpListener::bind(("127.0.0.1", config.redirect_port))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                OAuthError::PortBusy(config.redirect_port)
            } else {
                OAuthError::Flow(format!(
                    "could not listen on port {}: {error}",
                    config.redirect_port
                ))
            }
        })?;
    let pkce = create_pkce();
    let state = create_state();
    let url = authorize_url(config, &pkce, &state);
    Ok(SignInStart {
        url,
        pkce,
        state,
        listener,
    })
}

/// Open the browser, wait for the callback, and exchange the code. The
/// caller is responsible for showing the URL; nothing is printed here.
pub async fn finish_sign_in(
    config: &OAuthConfig,
    start: SignInStart,
) -> Result<Tokens, OAuthError> {
    open_browser(&start.url).await;
    let code = await_callback_on(start.listener, config, &start.state, CALLBACK_TIMEOUT).await?;
    exchange_code(config, &code, &start.pkce.verifier).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OAuthConfig {
        OAuthConfig {
            authorize_url: "https://auth.example.com/oauth/authorize",
            token_url: "https://auth.example.com/oauth/token",
            client_id: "test-client-id",
            redirect_uri: "http://localhost:1455/auth/callback",
            redirect_path: "/auth/callback",
            redirect_port: 1455,
            scopes: "openid profile",
            extra_authorize_params: &[("idv_captcha", "1")],
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // Verifier/challenge pair from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_meets_length_rules() {
        let pkce = create_pkce();
        assert!(
            (43..=128).contains(&pkce.verifier.chars().count()),
            "verifier length out of RFC range: {}",
            pkce.verifier.len()
        );
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
        assert!(!pkce.challenge.contains('='));
        assert!(!pkce.challenge.contains('+'));
        assert!(!pkce.challenge.contains('/'));
    }

    #[test]
    fn state_mismatch_is_rejected() {
        let config = test_config();
        assert_eq!(
            parse_callback(
                &config,
                "/auth/callback?code=abc&state=expected",
                "expected"
            ),
            Some("abc".into())
        );
        assert_eq!(
            parse_callback(&config, "/auth/callback?code=abc&state=wrong", "expected"),
            None
        );
        assert_eq!(
            parse_callback(&config, "/auth/callback?code=abc", "expected"),
            None
        );
        assert_eq!(
            parse_callback(&config, "/auth/callback?state=expected", "expected"),
            None
        );
        assert_eq!(
            parse_callback(&config, "/other/path?code=abc&state=expected", "expected"),
            None
        );
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let config = test_config();
        let pkce = create_pkce();
        let state = create_state();
        let url = authorize_url(&config, &pkce, &state);
        assert!(url.starts_with(config.authorize_url));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={}", config.client_id)));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={state}")));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(url.contains("scope=openid%20profile"));
        // The fixed redirect URI must survive URL encoding intact.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        // Provider quirks ride along as extra parameters.
        assert!(url.contains("&idv_captcha=1"));
    }

    #[tokio::test]
    async fn callback_with_keep_alive_connection_is_answered() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = test_config();
        let state = create_state();
        let path = format!("/auth/callback?code=the_code&state={state}");

        let server_config = config;
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            await_callback_on(
                listener,
                &server_config,
                &server_state,
                Duration::from_secs(5),
            )
            .await
        });

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        // One write, then hold the connection open without EOF, like a
        // keep-alive browser.
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("close this tab"));

        // The flow completes instead of waiting for a second packet.
        let code = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server must finish, not hang")
            .unwrap()
            .expect("valid code");
        assert_eq!(code, "the_code");
    }
}
