use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};

/// Reads the `exp` claim without verifying the signature (the server does that).
pub fn expiry(token: &str) -> Option<DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = v.get("exp")?.as_f64()?;
    Utc.timestamp_opt(exp as i64, 0).single()
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_exp() {
        // {"alg":"none"}.{"exp":2000000000}.
        let t = "eyJhbGciOiJub25lIn0.eyJleHAiOjIwMDAwMDAwMDB9.";
        assert_eq!(super::expiry(t).unwrap().timestamp(), 2_000_000_000);
        assert!(super::expiry("garbage").is_none());
    }
}
