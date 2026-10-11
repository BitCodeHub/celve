//! Sign-in in front of Sage: Cloudflare Access.
//!
//! When `CELVE_ACCESS_TEAM`, `CELVE_ACCESS_AUD` and `CELVE_ALLOWED_EMAILS` are set, Cloudflare
//! Access guards `/ws` and `/signin` at the edge (sign-in with a one-time code sent by email), and
//! this server checks the pass Access attaches to every request it lets through (a JWT in the
//! `Cf-Access-Jwt-Assertion` header): signed by the team's keys, for this application, not
//! expired, and for an allowed email. A request that reaches the server some other way, without
//! a valid pass, gets no Sage.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::Value as Json;

/// Re-read the team's signing keys at most this often (Cloudflare rotates them).
const KEYS_MAX_AGE: Duration = Duration::from_secs(3600);
/// And not more often than this when a pass names a key we don't have.
const KEYS_MIN_AGE: Duration = Duration::from_secs(60);

pub struct Gate {
    /// `https://<team>.cloudflareaccess.com`: the passes' issuer.
    team: String,
    /// The Access application's audience tag.
    aud: String,
    emails: Vec<String>,
    keys: Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    by_kid: Vec<(String, DecodingKey)>,
    fetched: Option<Instant>,
}

impl Gate {
    /// The gate from the environment: `None` when sign-in isn't configured, `Err` when it's
    /// configured only partly.
    pub fn from_env() -> Option<Result<Arc<Gate>, String>> {
        let get = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let (team, aud, emails) = (get("CELVE_ACCESS_TEAM"), get("CELVE_ACCESS_AUD"), get("CELVE_ALLOWED_EMAILS"));
        if team.is_none() && aud.is_none() && emails.is_none() {
            return None;
        }
        let (Some(team), Some(aud), Some(emails)) = (team, aud, emails) else {
            return Some(Err("sign-in needs CELVE_ACCESS_TEAM, CELVE_ACCESS_AUD and CELVE_ALLOWED_EMAILS together".into()));
        };
        let team = if team.contains("://") { team } else { format!("https://{team}") };
        let team = team.trim_end_matches('/').to_string();
        if !team.starts_with("https://") {
            return Some(Err("CELVE_ACCESS_TEAM must be an https:// address".into()));
        }
        Some(Ok(Arc::new(Gate::new(team, aud, &emails))))
    }

    fn new(team: String, aud: String, emails: &str) -> Gate {
        let emails = emails.split(',').map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).collect();
        Gate { team, aud, emails, keys: Mutex::new(Keys::default()) }
    }

    pub fn team(&self) -> &str {
        &self.team
    }

    pub fn emails(&self) -> &[String] {
        &self.emails
    }

    /// The signed-in email, when the request carries a valid pass for an allowed person.
    pub async fn check(self: &Arc<Self>, headers: &HeaderMap) -> Result<String, &'static str> {
        let token = headers.get("cf-access-jwt-assertion").and_then(|v| v.to_str().ok()).ok_or("no sign-in pass")?;
        let kid = jsonwebtoken::decode_header(token).map_err(|_| "unreadable sign-in pass")?.kid.ok_or("sign-in pass names no key")?;
        if self.key(&kid).is_none() && self.refresh_due(false) {
            let me = self.clone();
            // The fetch blocks; keep it off the async workers.
            let _ = tokio::task::spawn_blocking(move || me.refresh()).await;
        } else if self.refresh_due(true) {
            let me = self.clone();
            tokio::task::spawn_blocking(move || me.refresh());
        }
        let key = self.key(&kid).ok_or("sign-in pass signed by an unknown key")?;
        self.verify(token, &key)
    }

    fn verify(&self, token: &str, key: &DecodingKey) -> Result<String, &'static str> {
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.aud]);
        v.set_issuer(&[&self.team]);
        v.set_required_spec_claims(&["exp", "iss", "aud"]);
        let data = jsonwebtoken::decode::<Json>(token, key, &v).map_err(|_| "invalid sign-in pass")?;
        let email = data.claims.get("email").and_then(Json::as_str).ok_or("sign-in pass has no email")?.to_ascii_lowercase();
        if self.emails.contains(&email) { Ok(email) } else { Err("this account isn't allowed to use Sage") }
    }

    fn key(&self, kid: &str) -> Option<DecodingKey> {
        let keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        keys.by_kid.iter().find(|(k, _)| k == kid).map(|(_, key)| key.clone())
    }

    /// Whether the keys should be re-read: `stale` asks "are they old?", otherwise "may we
    /// look for a key we don't have yet?".
    fn refresh_due(&self, stale: bool) -> bool {
        let keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let limit = if stale { KEYS_MAX_AGE } else { KEYS_MIN_AGE };
        keys.fetched.is_none_or(|t| t.elapsed() >= limit)
    }

    // The server never runs on wasm, where the clock traps.
    #[allow(clippy::disallowed_methods)]
    fn refresh(&self) {
        {
            // Claim this refresh so concurrent requests don't all fetch.
            let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
            if keys.fetched.is_some_and(|t| t.elapsed() < KEYS_MIN_AGE) && !keys.by_kid.is_empty() {
                return;
            }
            keys.fetched = Some(Instant::now());
        }
        let url = format!("{}/cdn-cgi/access/certs", self.team);
        let body = match ureq::get(&url).timeout(Duration::from_secs(10)).call().map(|r| r.into_string()) {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return log::warn!("sign-in keys: {e}"),
            Err(e) => return log::warn!("sign-in keys: {e}"),
        };
        let by_kid = parse_keys(&body);
        if by_kid.is_empty() {
            return log::warn!("sign-in keys: none found at {url}");
        }
        log::info!("sign-in keys: {} loaded", by_kid.len());
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).by_kid = by_kid;
    }
}

/// The RSA keys in a JWKS document (`{"keys":[{kid,kty,n,e},…]}`).
fn parse_keys(body: &str) -> Vec<(String, DecodingKey)> {
    let Ok(doc) = serde_json::from_str::<Json>(body) else { return vec![] };
    let Some(keys) = doc.get("keys").and_then(Json::as_array) else { return vec![] };
    keys.iter()
        .filter(|k| k.get("kty").and_then(Json::as_str) == Some("RSA"))
        .filter_map(|k| {
            let kid = k.get("kid")?.as_str()?.to_string();
            let key = DecodingKey::from_rsa_components(k.get("n")?.as_str()?, k.get("e")?.as_str()?).ok()?;
            Some((kid, key))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    // A throwaway key made for these tests only.
    const PRIVATE: &str = include_str!("../tests/access-test-key.pem");
    const PUBLIC: &str = include_str!("../tests/access-test-key.pub.pem");

    fn gate() -> Gate {
        let g = Gate::new("https://team.cloudflareaccess.com".into(), "aud-1".into(), "Owner@Example.com, other@example.com");
        g.keys.lock().unwrap().by_kid = vec![("k1".into(), DecodingKey::from_rsa_pem(PUBLIC.as_bytes()).unwrap())];
        g
    }

    fn pass(claims: Json) -> String {
        let mut h = Header::new(Algorithm::RS256);
        h.kid = Some("k1".into());
        jsonwebtoken::encode(&h, &claims, &EncodingKey::from_rsa_pem(PRIVATE.as_bytes()).unwrap()).unwrap()
    }

    #[allow(clippy::disallowed_methods)]
    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn only_valid_passes_for_allowed_people_get_in() {
        let g = gate();
        let key = g.key("k1").unwrap();
        let ok = json!({"email": "owner@example.com", "aud": ["aud-1"], "iss": "https://team.cloudflareaccess.com", "exp": now() + 600});
        assert_eq!(g.verify(&pass(ok.clone()), &key), Ok("owner@example.com".into()));

        let mut stranger = ok.clone();
        stranger["email"] = json!("stranger@example.com");
        assert!(g.verify(&pass(stranger), &key).is_err());
        let mut other_app = ok.clone();
        other_app["aud"] = json!(["aud-2"]);
        assert!(g.verify(&pass(other_app), &key).is_err());
        let mut other_team = ok.clone();
        other_team["iss"] = json!("https://evil.cloudflareaccess.com");
        assert!(g.verify(&pass(other_team), &key).is_err());
        let mut expired = ok.clone();
        expired["exp"] = json!(now() - 600);
        assert!(g.verify(&pass(expired), &key).is_err());
        let mut no_exp = ok.clone();
        no_exp.as_object_mut().unwrap().remove("exp");
        assert!(g.verify(&pass(no_exp), &key).is_err());

        // A forged pass: right claims, tampered payload.
        let good = pass(ok);
        let mut parts: Vec<String> = good.split('.').map(str::to_string).collect();
        parts[1] = parts[1].chars().rev().collect();
        assert!(g.verify(&parts.join("."), &key).is_err());
        // Unsigned.
        let unsigned = format!("{}.{}.", parts[0], good.split('.').nth(1).unwrap());
        assert!(g.verify(&unsigned, &key).is_err());
    }

    #[test]
    fn keys_are_read_from_a_jwks_document() {
        let doc = r#"{"keys":[{"kid":"a","kty":"RSA","alg":"RS256","n":"sXchDaQebHnPiGvyDOAT4saGEUetSyo9MKLOoWFsueri23bOdgWp4Dy1WlUzewbgBHod5pcM9H95GQRV3JDXboIRROSBigeC5yjU1hGzHHyXss8UDprecbAYxknTcQkhslANGRUZmdTOQ5qTRsLAt6BTYuyvVRdhS8exSZEy_c4gs_7svlJJQ4H9_NxsiIoLwAEk7-Q3UXERGYw_75IDrGA84-lA_-Ct4eTlXHBIY2EaV7t7LjJaynVJCpkv4LKjTTAumiGUIuQhrNhZLuF_RJLqHpM2kgWFLU7-VTdL1VbC2tejvcI2BlMkEpk1BzBZI0KQB0GaDWFLN-aEAw3vRw","e":"AQAB"},{"kid":"b","kty":"EC"}]}"#;
        let keys = parse_keys(doc);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "a");
        assert!(parse_keys("not json").is_empty());
        assert!(parse_keys(r#"{"keys":"nope"}"#).is_empty());
    }

    #[tokio::test]
    async fn requests_without_a_pass_are_refused() {
        let g = Arc::new(gate());
        assert!(g.check(&HeaderMap::new()).await.is_err());
        let mut h = HeaderMap::new();
        h.insert("cf-access-jwt-assertion", "garbage".parse().unwrap());
        assert!(g.check(&h).await.is_err());
        let ok = json!({"email": "other@example.com", "aud": "aud-1", "iss": "https://team.cloudflareaccess.com", "exp": now() + 600});
        h.insert("cf-access-jwt-assertion", pass(ok).parse().unwrap());
        assert_eq!(g.check(&h).await, Ok("other@example.com".into()));
    }
}
