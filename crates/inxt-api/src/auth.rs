use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;
use serde_json::json;

use crate::client::{Auth, Client, Credentials, Retry};
use crate::crypto;
use crate::error::{ApiError, Result};
use crate::types::{LoginAccessResponse, RefreshResponse, SecurityDetails, UserDto};

/// What the web app hands back to a desktop client after browser sign-in.
pub struct SsoResult {
    pub token: String,
    pub mnemonic: String,
}

impl std::fmt::Debug for SsoResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SsoResult(..)")
    }
}

/// URL that signs the user in on the web app and redirects back to
/// `redirect_uri` (a loopback address) with the session.
pub fn sso_login_url(web_url: &str, redirect_uri: &str) -> String {
    let encoded = STANDARD.encode(redirect_uri);
    let mut url = url::Url::parse(&format!("{}/login", web_url.trim_end_matches('/'))).expect("static url");
    url.query_pairs_mut().append_pair("universalLink", "true").append_pair("redirectUri", &encoded);
    url.into()
}

fn b64_param(v: &str) -> Option<String> {
    let v = v.trim();
    STANDARD
        .decode(v)
        .or_else(|_| URL_SAFE.decode(v))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Parses the callback query string (`?newToken=..&mnemonic=..&privateKey=..`).
pub fn parse_sso_callback(query: &str) -> Result<SsoResult> {
    let q = query.trim_start_matches('?');
    let mut token = None;
    let mut mnemonic = None;
    for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
        match k.as_ref() {
            "newToken" => token = b64_param(&v),
            "mnemonic" => mnemonic = b64_param(&v),
            _ => {}
        }
    }
    let (Some(token), Some(mnemonic)) = (token, mnemonic) else {
        return Err(ApiError::Other("sign-in callback is missing the session".into()));
    };
    if !crypto::validate_mnemonic(&mnemonic) {
        return Err(ApiError::Crypto("sign-in callback carried an invalid mnemonic".into()));
    }
    Ok(SsoResult { token, mnemonic })
}

fn credentials_from(user: UserDto, token: String, mnemonic: String) -> Result<Credentials> {
    if user.bucket.is_empty() || user.root_folder_id.is_empty() || user.bridge_user.is_empty() {
        return Err(ApiError::Decode("user", "account data is incomplete".into()));
    }
    Ok(Credentials {
        token,
        mnemonic,
        email: user.email,
        user_uuid: user.uuid,
        user_id: user.user_id,
        bridge_user: user.bridge_user,
        bucket: user.bucket,
        root_folder_uuid: user.root_folder_id,
    })
}

impl Client {
    /// Exchanges a browser sign-in result for full credentials and activates them.
    pub async fn complete_sso(&self, sso: SsoResult) -> Result<Credentials> {
        let url = self.drive_url("/users/refresh");
        let token = sso.token.clone();
        let resp = self
            .execute("sign in", Auth::None, Retry::Idempotent, || self.http.get(&url).bearer_auth(&token))
            .await?;
        let r: RefreshResponse = self.json("sign in", resp).await?;
        let creds = credentials_from(r.user, r.new_token, sso.mnemonic)?;
        self.set_credentials(Some(creds.clone()))?;
        Ok(creds)
    }

    pub async fn security_details(&self, email: &str) -> Result<SecurityDetails> {
        let url = self.drive_url("/auth/login");
        let body = json!({ "email": email });
        let resp = self
            .execute("login", Auth::None, Retry::Idempotent, || self.http.post(&url).json(&body))
            .await?;
        self.json("login", resp).await
    }

    /// Email/password sign-in. `tfa` is the 6-digit code when 2FA is enabled.
    pub async fn login_password(&self, email: &str, password: &str, tfa: Option<&str>) -> Result<Credentials> {
        let details = self.security_details(email).await?;
        if details.tfa && tfa.is_none_or(str::is_empty) {
            return Err(ApiError::Other("two-factor code required".into()));
        }
        let proof = crypto::encrypt_password_hash(password, &details.s_key)?;
        let mut body = json!({ "email": email, "password": proof });
        if let Some(code) = tfa.filter(|c| !c.is_empty()) {
            body["tfa"] = json!(code);
        }
        let url = self.drive_url("/auth/login/access");
        let resp = self
            .execute("login", Auth::None, Retry::Once, || self.http.post(&url).json(&body))
            .await?;
        let r: LoginAccessResponse = self.json("login", resp).await?;
        let mnemonic = crypto::decrypt_text_with_key(&r.user.mnemonic, password)?;
        if !crypto::validate_mnemonic(&mnemonic) {
            return Err(ApiError::Crypto("decrypted mnemonic is invalid".into()));
        }
        let creds = credentials_from(r.user, r.new_token, mnemonic)?;
        self.set_credentials(Some(creds.clone()))?;
        Ok(creds)
    }

    pub async fn logout(&self) {
        let url = self.drive_url("/auth/logout");
        if self.credentials().is_ok() {
            let _ = self.execute("logout", Auth::Bearer, Retry::Once, || self.http.get(&url)).await;
        }
        let _ = self.set_credentials(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sso_url_shape() {
        let u = sso_login_url("https://drive.internxt.com", "http://127.0.0.1:5000/callback");
        assert!(u.starts_with("https://drive.internxt.com/login?universalLink=true&redirectUri="));
        let enc = url::Url::parse(&u).unwrap();
        let r = enc.query_pairs().find(|(k, _)| k == "redirectUri").unwrap().1.into_owned();
        assert_eq!(STANDARD.decode(r).unwrap(), b"http://127.0.0.1:5000/callback");
    }

    #[test]
    fn sso_callback_parses() {
        let m = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let q = format!(
            "?newToken={}&mnemonic={}&privateKey={}",
            STANDARD.encode("tok"),
            urlencode(&STANDARD.encode(m)),
            STANDARD.encode("pk")
        );
        let r = parse_sso_callback(&q).unwrap();
        assert_eq!(r.token, "tok");
        assert_eq!(r.mnemonic, m);
        assert!(parse_sso_callback("?newToken=dG9r").is_err());
    }

    fn urlencode(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }
}
