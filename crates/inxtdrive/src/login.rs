//! Browser sign-in through a one-shot loopback web page.
//!
//! The page offers the official web sign-in (which redirects back here with
//! the session, exactly like the official desktop app) and an email/password
//! form as a fallback. Both paths carry a random nonce so other web pages
//! cannot inject a session.

use std::sync::Arc;
use std::time::Duration;

use inxt_api::auth::{parse_sso_callback, sso_login_url};
use inxt_api::{Client, Credentials};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(15 * 60);

struct Request {
    method: String,
    path: String,
    query: String,
    body: String,
}

async fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let len: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0)
        .min(64 * 1024);
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Request { method, path, query, body: String::from_utf8_lossy(&body).into_owned() })
}

async fn respond(s: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(resp.as_bytes()).await;
    let _ = s.shutdown().await;
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const STYLE: &str = r#"<style>
:root{color-scheme:light dark;--bg:#f5f7fb;--card:#fff;--fg:#14181f;--muted:#5b6472;--accent:#2563eb;--line:#d9dee7;--err:#b42318}
@media (prefers-color-scheme:dark){:root{--bg:#0f1218;--card:#171b23;--fg:#e8ebf0;--muted:#9aa3b2;--accent:#5b8cff;--line:#2a3140;--err:#ff8a7a}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:15px/1.5 "Segoe UI",system-ui,sans-serif;padding:16px}
.card{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:32px;width:100%;max-width:400px}
h1{font-size:20px;margin:0 0 6px}p{color:var(--muted);margin:0 0 22px}
a.btn,button{display:block;width:100%;text-align:center;border:0;border-radius:9px;padding:11px 14px;font:inherit;font-weight:600;cursor:pointer;text-decoration:none}
a.btn{background:var(--accent);color:#fff}button{background:transparent;color:var(--accent);border:1px solid var(--line);margin-top:10px}
details{margin-top:22px;border-top:1px solid var(--line);padding-top:16px}summary{cursor:pointer;color:var(--muted)}
label{display:block;margin:12px 0 4px;font-size:13px;color:var(--muted)}input{width:100%;padding:9px 11px;border-radius:8px;border:1px solid var(--line);background:transparent;color:var(--fg);font:inherit}
.err{color:var(--err);margin:0 0 14px}.small{font-size:12px;color:var(--muted);margin-top:18px}
</style>"#;

fn page(title: &str, inner: &str) -> String {
    format!("<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title}</title>{STYLE}</head><body><div class=card>{inner}</div></body></html>")
}

fn start_page(sso_url: &str, nonce: &str, error: Option<&str>, email: &str, need_tfa: bool) -> String {
    let err = error.map(|e| format!("<p class=err>{}</p>", esc(e))).unwrap_or_default();
    let tfa = if need_tfa {
        "<label for=tfa>Two-factor code</label><input id=tfa name=tfa inputmode=numeric autocomplete=one-time-code autofocus>"
    } else {
        "<label for=tfa>Two-factor code (if enabled)</label><input id=tfa name=tfa inputmode=numeric autocomplete=one-time-code>"
    };
    let open = if error.is_some() { " open" } else { "" };
    page(
        "Sign in to InxtDrive",
        &format!(
            "<h1>Sign in to InxtDrive</h1><p>Connect your Internxt account to use it as a drive on this PC.</p>{err}\
             <a class=btn href=\"{sso}\">Continue with Internxt sign-in</a>\
             <details{open}><summary>Use email and password instead</summary>\
             <form method=post action=\"/password/{nonce}\">\
             <label for=email>Email</label><input id=email name=email type=email autocomplete=username value=\"{email}\" required>\
             <label for=password>Password</label><input id=password name=password type=password autocomplete=current-password required>\
             {tfa}<button type=submit>Sign in</button></form></details>\
             <p class=small>InxtDrive is an unofficial client. Your password and recovery words never leave this computer.</p>",
            sso = esc(sso_url),
            email = esc(email),
        ),
    )
}

fn done_page() -> String {
    page("Signed in", "<h1>You're signed in</h1><p>InxtDrive is setting up your drive. You can close this tab.</p>")
}

/// Runs the sign-in page until the user completes it. `open` launches the
/// browser at the given local URL.
pub async fn browser_login(client: Arc<Client>, open: impl Fn(&str)) -> anyhow::Result<Credentials> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let nonce = hex::encode(rand::random::<[u8; 16]>());
    let callback = format!("http://127.0.0.1:{port}/callback/{nonce}");
    let sso = sso_login_url(&client.config().web_url, &callback);
    open(&format!("http://127.0.0.1:{port}/{nonce}"));

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (mut s, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| anyhow::anyhow!("sign-in timed out"))??;
        let Some(req) = read_request(&mut s).await else { continue };
        let path = req.path.trim_end_matches('/');

        if req.method == "GET" && path == format!("/{nonce}") {
            respond(&mut s, "200 OK", "", &start_page(&sso, &nonce, None, "", false)).await;
        } else if req.method == "GET" && path == format!("/callback/{nonce}") {
            let result = match parse_sso_callback(&req.query) {
                Ok(sso_result) => client.complete_sso(sso_result).await.map_err(anyhow::Error::from),
                Err(e) => Err(e.into()),
            };
            match result {
                Ok(creds) => {
                    respond(&mut s, "200 OK", "", &done_page()).await;
                    return Ok(creds);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "browser sign-in failed");
                    let body = start_page(&sso, &nonce, Some(&format!("Sign-in did not complete: {e}")), "", false);
                    respond(&mut s, "200 OK", "", &body).await;
                }
            }
        } else if req.method == "POST" && path == format!("/password/{nonce}") {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(req.body.as_bytes()).into_owned().collect();
            let email = form.get("email").cloned().unwrap_or_default();
            let password = form.get("password").cloned().unwrap_or_default();
            let tfa = form.get("tfa").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            match client.login_password(email.trim(), &password, tfa.as_deref()).await {
                Ok(creds) => {
                    respond(&mut s, "200 OK", "", &done_page()).await;
                    return Ok(creds);
                }
                Err(e) => {
                    let msg = e.to_string();
                    let need_tfa = msg.contains("two-factor");
                    let shown = if need_tfa {
                        "Enter the code from your authenticator app.".to_string()
                    } else if e.status() == Some(401) || e.status() == Some(400) || msg.contains("bad padding") {
                        "Wrong email, password or code.".to_string()
                    } else {
                        format!("Sign-in failed: {msg}")
                    };
                    respond(&mut s, "200 OK", "", &start_page(&sso, &nonce, Some(&shown), &email, need_tfa)).await;
                }
            }
        } else {
            respond(&mut s, "404 Not Found", "", &page("Not found", "<h1>Not found</h1>")).await;
        }
    }
}
