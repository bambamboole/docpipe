//! Authentication of `/v1` requests, as in pdf-ua-api: a JWT verified against
//! the issuer's JWKS, or a static API key, or none.
//!
//! JWTs are OAuth 2.0 access tokens signed with RS256 (RFC 9068); a client
//! gets one with the client credentials grant and names itself in the
//! `client_id` claim.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

/// Unknown key ids trigger a JWKS refresh at most this often, so a stream of
/// forged tokens cannot hammer the issuer.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum Auth {
    /// No credentials required; only for local development.
    Public,
    ApiKey(String),
    Jwt(JwtVerifier),
}

impl Auth {
    /// The mode named by `AUTH_MODE`: `jwt` with `JWT_ISSUER` and
    /// `JWT_JWKS_URL`, `api_key` with `API_KEY`, or `none`. There is no
    /// default, so a forgotten setting cannot leave the service open.
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let required = |name: &str| var(name).ok_or_else(|| format!("AUTH_MODE requires {name}"));

        match var("AUTH_MODE").as_deref().map(str::trim) {
            Some("jwt") => Ok(Self::Jwt(JwtVerifier::new(JwtConfig {
                issuer: required("JWT_ISSUER")?,
                jwks_url: required("JWT_JWKS_URL")?,
                audience: var("JWT_AUDIENCE"),
                allowed_clients: var("JWT_ALLOWED_CLIENTS")
                    .map(|clients| clients.split(',').map(|client| client.trim().to_owned()).collect())
                    .unwrap_or_default(),
            }))),
            Some("api_key") => Ok(Self::ApiKey(required("API_KEY")?)),
            Some("none") => Ok(Self::Public),
            Some(mode) => Err(format!("unknown AUTH_MODE {mode:?}; use jwt, api_key or none")),
            None => Err("AUTH_MODE is not set; use jwt, api_key or none".to_owned()),
        }
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::Public => "none",
            Self::ApiKey(_) => "api_key",
            Self::Jwt(_) => "jwt",
        }
    }

    /// Whether the `Authorization` header value grants access. JWT
    /// verification may fetch the JWKS, so call this off the async runtime.
    pub fn authorize(&self, header: Option<&str>) -> Result<(), AuthError> {
        // The scheme name is case-insensitive (RFC 9110, section 11.1).
        let bearer = || {
            header
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token.trim())
                .filter(|token| !token.is_empty())
                .ok_or(AuthError::Missing)
        };

        match self {
            Self::Public => Ok(()),
            Self::ApiKey(key) => {
                if constant_time_eq(bearer()?.as_bytes(), key.as_bytes()) {
                    Ok(())
                } else {
                    Err(AuthError::Invalid("the API key does not match".to_owned()))
                }
            }
            Self::Jwt(verifier) => verifier.verify(bearer()?),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AuthError {
    #[error("no bearer token was sent")]
    Missing,
    #[error("the token is invalid: {0}")]
    Invalid(String),
    #[error("the issuer's keys cannot be fetched: {0}")]
    Jwks(String),
}

#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Expected `iss`.
    pub issuer: String,
    pub jwks_url: String,
    /// Expected `aud`; when unset, the audience is not checked.
    pub audience: Option<String>,
    /// Accepted `client_id` claims; when empty, every client of the issuer is.
    pub allowed_clients: Vec<String>,
}

#[derive(Debug)]
pub struct JwtVerifier {
    config: JwtConfig,
    keys: RwLock<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

impl std::fmt::Debug for KeyCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeyCache")
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .field("fetched_at", &self.fetched_at)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    client_id: Option<String>,
}

impl JwtVerifier {
    pub fn new(config: JwtConfig) -> Self {
        Self {
            config,
            keys: RwLock::new(KeyCache::default()),
        }
    }

    pub fn verify(&self, token: &str) -> Result<(), AuthError> {
        let header = decode_header(token).map_err(|error| AuthError::Invalid(error.to_string()))?;

        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Invalid(format!("unexpected algorithm {:?}", header.alg)));
        }

        let kid = header
            .kid
            .ok_or_else(|| AuthError::Invalid("the token names no key".to_owned()))?;
        let key = self.key(&kid)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_required_spec_claims(&["exp", "iss"]);
        match &self.config.audience {
            Some(audience) => validation.set_audience(&[audience]),
            None => validation.validate_aud = false,
        }

        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|error| AuthError::Invalid(error.to_string()))?
            .claims;

        if !self.config.allowed_clients.is_empty()
            && !claims
                .client_id
                .as_ref()
                .is_some_and(|client| self.config.allowed_clients.contains(client))
        {
            return Err(AuthError::Invalid("the token was issued to another client".to_owned()));
        }

        Ok(())
    }

    /// The key for `kid`, refreshing the JWKS when the key is unknown, e.g.
    /// after the issuer rotated its keys.
    fn key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = self.cached(kid) {
            return Ok(key);
        }

        let mut cache = self.keys.write().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Another request may have refreshed while this one waited.
        if let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }

        if cache
            .fetched_at
            .is_some_and(|fetched| fetched.elapsed() < REFRESH_INTERVAL)
        {
            return Err(AuthError::Invalid(format!("unknown key {kid}")));
        }

        cache.keys = fetch_jwks(&self.config.jwks_url)?;
        cache.fetched_at = Some(Instant::now());

        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| AuthError::Invalid(format!("unknown key {kid}")))
    }

    fn cached(&self, kid: &str) -> Option<DecodingKey> {
        self.keys
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys
            .get(kid)
            .cloned()
    }

    #[cfg(test)]
    fn with_keys(config: JwtConfig, jwks: &str) -> Self {
        let verifier = Self::new(config);
        {
            let mut cache = verifier.keys.write().unwrap();
            cache.keys = parse_jwks(jwks).unwrap();
            cache.fetched_at = Some(Instant::now());
        }
        verifier
    }
}

fn fetch_jwks(url: &str) -> Result<HashMap<String, DecodingKey>, AuthError> {
    let body = ureq::get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .call()
        .map_err(|error| AuthError::Jwks(error.to_string()))?
        .body_mut()
        .read_to_string()
        .map_err(|error| AuthError::Jwks(error.to_string()))?;

    parse_jwks(&body)
}

fn parse_jwks(body: &str) -> Result<HashMap<String, DecodingKey>, AuthError> {
    let set: JwkSet = serde_json::from_str(body).map_err(|error| AuthError::Jwks(error.to_string()))?;

    Ok(set
        .keys
        .iter()
        .filter_map(|jwk| Some((jwk.common.key_id.clone()?, DecodingKey::from_jwk(jwk).ok()?)))
        .collect())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::LazyLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use rsa::RsaPrivateKey;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use serde_json::{Value, json};

    use super::*;

    pub(crate) const ISSUER: &str = "https://issuer.test";

    /// The issuer's key, published in its JWKS as `test-key`.
    static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_key);
    /// A key the issuer never published.
    static OTHER_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_key);

    fn generate_key() -> RsaPrivateKey {
        RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap()
    }

    fn jwks() -> String {
        let encode = |bytes: Vec<u8>| URL_SAFE_NO_PAD.encode(bytes);

        json!({
            "keys": [{
                "kty": "RSA",
                "alg": "RS256",
                "use": "sig",
                "kid": "test-key",
                "n": encode(KEY.n().to_bytes_be()),
                "e": encode(KEY.e().to_bytes_be()),
            }]
        })
        .to_string()
    }

    pub(crate) fn verifier(audience: Option<&str>, allowed_clients: &[&str]) -> JwtVerifier {
        JwtVerifier::with_keys(
            JwtConfig {
                issuer: ISSUER.to_owned(),
                jwks_url: "http://127.0.0.1:9/never-fetched".to_owned(),
                audience: audience.map(str::to_owned),
                allowed_clients: allowed_clients.iter().map(|client| (*client).to_owned()).collect(),
            },
            &jwks(),
        )
    }

    /// A token signed with the test key, valid for five minutes, with
    /// `overrides` merged into the claims.
    pub(crate) fn token(overrides: Value) -> String {
        sign(&KEY, "test-key", overrides)
    }

    fn sign(key: &RsaPrivateKey, kid: &str, overrides: Value) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut claims = json!({
            "iss": ISSUER,
            "aud": ISSUER,
            "sub": "example-client",
            "client_id": "example-client",
            "iat": now,
            "exp": now + 300,
        });
        for (name, value) in overrides.as_object().unwrap() {
            claims[name] = value.clone();
        }

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        let der = key.to_pkcs1_der().unwrap();
        encode(&header, &claims, &EncodingKey::from_rsa_der(der.as_bytes())).unwrap()
    }

    #[test]
    fn accepts_a_valid_token_of_an_allowed_client() {
        assert_eq!(
            verifier(Some(ISSUER), &["example-client"]).verify(&token(json!({}))),
            Ok(())
        );
    }

    #[test]
    fn rejects_tokens_of_other_clients_issuers_and_audiences() {
        let verifier = verifier(Some(ISSUER), &["example-client"]);

        assert!(verifier.verify(&token(json!({ "client_id": "other-client" }))).is_err());
        assert!(verifier.verify(&token(json!({ "iss": "https://evil.test" }))).is_err());
        assert!(
            verifier
                .verify(&token(json!({ "aud": "https://elsewhere.test" })))
                .is_err()
        );
    }

    #[test]
    fn rejects_expired_tokens_and_foreign_signatures() {
        let verifier = verifier(None, &[]);
        let expired = token(json!({ "exp": 1_000_000_000 }));
        let forged = sign(&OTHER_KEY, "test-key", json!({}));

        assert!(verifier.verify(&expired).is_err());
        assert!(verifier.verify(&forged).is_err());
    }

    #[test]
    fn does_not_refetch_the_jwks_for_unknown_keys_right_after_a_fetch() {
        let token = sign(&OTHER_KEY, "unknown-key", json!({}));

        assert_eq!(
            verifier(None, &[]).verify(&token),
            Err(AuthError::Invalid("unknown key unknown-key".to_owned()))
        );
    }

    #[test]
    fn checks_the_api_key() {
        let auth = Auth::ApiKey("secret".to_owned());

        assert_eq!(auth.authorize(Some("Bearer secret")), Ok(()));
        assert_eq!(auth.authorize(Some("bearer secret")), Ok(()));
        assert_eq!(auth.authorize(Some("Basic secret")), Err(AuthError::Missing));
        assert!(auth.authorize(Some("Bearer wrong")).is_err());
        assert_eq!(auth.authorize(None), Err(AuthError::Missing));
        assert_eq!(Auth::Public.authorize(None), Ok(()));
    }

    fn from(vars: &[(&str, &str)]) -> Result<Auth, String> {
        Auth::from_vars(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn requires_an_explicit_mode() {
        assert!(from(&[]).is_err());
        assert!(from(&[("API_KEY", "secret")]).is_err());
        assert!(from(&[("AUTH_MODE", "basic")]).is_err());
        assert_eq!(from(&[("AUTH_MODE", "none")]).unwrap().mode(), "none");
    }

    #[test]
    fn requires_the_settings_of_the_mode() {
        assert!(from(&[("AUTH_MODE", "api_key")]).is_err());
        assert!(from(&[("AUTH_MODE", "jwt"), ("JWT_ISSUER", ISSUER)]).is_err());
        assert_eq!(
            from(&[("AUTH_MODE", "api_key"), ("API_KEY", "secret")]).unwrap().mode(),
            "api_key"
        );
        assert_eq!(
            from(&[
                ("AUTH_MODE", "jwt"),
                ("JWT_ISSUER", ISSUER),
                ("JWT_JWKS_URL", "https://issuer.test/jwks")
            ])
            .unwrap()
            .mode(),
            "jwt"
        );
    }
}
