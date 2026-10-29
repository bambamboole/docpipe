//! Authentication of `/v1` requests, as in pdf-ua-api: a JWT verified against
//! the issuer's JWKS, or a static API key, or none.
//!
//! JWTs are OAuth 2.0 access tokens signed with RS256 (RFC 9068); a client
//! gets one with the client credentials grant and names itself in the
//! `client_id` claim. Verification follows the JWT best current practice
//! (RFC 8725): the algorithm is pinned to RS256 before the signature is
//! looked at, keys are selected by `kid` only, and `exp`, `nbf`, `iss` and,
//! when configured, `aud` are all checked.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use subtle::ConstantTimeEq;

/// Unknown key ids trigger a JWKS refresh at most this often, so a stream of
/// forged tokens cannot hammer the issuer.
const REFRESH_INTERVAL: Duration = Duration::from_mins(1);

/// How long a JWKS fetch may take, connection included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How `/v1` requests are authenticated.
pub enum Auth {
    /// No credentials required; only for local development.
    Public,
    /// A static bearer token that every request must carry.
    ApiKey(String),
    /// An RS256 JWT of a known issuer.
    Jwt(JwtVerifier),
}

/// The API key is a secret: `Config` derives `Debug` and is logged at
/// startup, so it must not appear there.
impl fmt::Debug for Auth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Public => formatter.write_str("Public"),
            Self::ApiKey(_) => formatter.debug_tuple("ApiKey").field(&"<redacted>").finish(),
            Self::Jwt(verifier) => formatter.debug_tuple("Jwt").field(verifier).finish(),
        }
    }
}

impl Auth {
    /// The mode named by `AUTH_MODE`: `jwt` with `JWT_ISSUER` and
    /// `JWT_JWKS_URL`, `api_key` with `API_KEY`, or `none`. There is no
    /// default, so a forgotten setting cannot leave the service open.
    /// [`crate::config`] passes the environment through `lookup`.
    pub(crate) fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
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

    /// The mode as `AUTH_MODE` names it: `none`, `api_key` or `jwt`.
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Public => "none",
            Self::ApiKey(_) => "api_key",
            Self::Jwt(_) => "jwt",
        }
    }

    /// Fetches what later requests need, so that the first of them does not
    /// pay for it and a misconfigured issuer shows up at startup: the JWKS
    /// in JWT mode, nothing in the others. A failure leaves the service
    /// usable; the keys are fetched again on the first request.
    pub fn preload(&self) -> Result<(), AuthError> {
        match self {
            Self::Jwt(verifier) => verifier.preload(),
            Self::Public | Self::ApiKey(_) => Ok(()),
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
                // Constant time, so that the response time does not tell how
                // long a matching prefix is; slices of different lengths
                // compare unequal.
                if bool::from(bearer()?.as_bytes().ct_eq(key.as_bytes())) {
                    Ok(())
                } else {
                    Err(AuthError::Invalid("the API key does not match".to_owned()))
                }
            }
            Self::Jwt(verifier) => verifier.verify(bearer()?),
        }
    }
}

/// Why a request was not authenticated. The reasons are logged, never
/// returned to the client.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// The request carried no `Authorization: Bearer` header.
    #[error("no bearer token was sent")]
    Missing,
    /// The credentials were sent but do not grant access.
    #[error("the token is invalid: {0}")]
    Invalid(String),
    /// The JWKS could not be fetched or parsed, so the token could not be
    /// checked; the request is answered with 503 rather than 401.
    #[error("the issuer's keys cannot be fetched: {0}")]
    Jwks(String),
}

/// What a JWT must carry to be accepted.
#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Expected `iss`.
    pub issuer: String,
    /// Where the issuer publishes its public keys as a JWK Set.
    pub jwks_url: String,
    /// Expected `aud`; when unset, the audience is not checked.
    pub audience: Option<String>,
    /// Accepted `client_id` claims; when empty, every client of the issuer is.
    pub allowed_clients: Vec<String>,
}

/// Verifies RS256 JWTs against the issuer's JWKS, which it caches.
#[derive(Debug)]
pub struct JwtVerifier {
    config: JwtConfig,
    keys: RwLock<KeyCache>,
    /// Held while the JWKS is fetched: concurrent requests with unknown keys
    /// wait for one fetch instead of each starting their own, while
    /// verifications with known keys go on reading the cache.
    fetching: Mutex<()>,
    /// Unknown key ids refresh the JWKS at most this often.
    refresh_interval: Duration,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    /// When a fetch was last attempted, successful or not; `None` until the
    /// first attempt.
    attempted_at: Option<Instant>,
}

impl fmt::Debug for KeyCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyCache")
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .field("attempted_at", &self.attempted_at)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    client_id: Option<String>,
}

impl JwtVerifier {
    /// A verifier with an empty key cache; the JWKS is fetched by
    /// [`preload`](Self::preload) or the first verification.
    pub fn new(config: JwtConfig) -> Self {
        Self {
            config,
            keys: RwLock::new(KeyCache::default()),
            fetching: Mutex::new(()),
            refresh_interval: REFRESH_INTERVAL,
        }
    }

    /// Fetches the JWKS now and replaces the cached keys with it. On failure
    /// the cache keeps whatever it held before.
    pub fn preload(&self) -> Result<(), AuthError> {
        let _fetch = self.fetching.lock().unwrap_or_else(PoisonError::into_inner);
        self.refresh()
    }

    /// The claim checks of this issuer. RS256 only: with a single accepted
    /// algorithm, a token cannot pick one that misuses the key, e.g. HS256
    /// with the public key as secret.
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.config.issuer]);
        if let Some(audience) = &self.config.audience {
            validation.set_audience(&[audience]);
            // Without this, a token that names no audience at all passes.
            validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        } else {
            validation.validate_aud = false;
            validation.set_required_spec_claims(&["exp", "iss"]);
        }
        validation
    }

    /// Whether `token` is an RS256 JWT of the configured issuer, signed by a
    /// key of its JWKS, not expired, not used before its `nbf`, for the
    /// configured audience and from an allowed client.
    pub fn verify(&self, token: &str) -> Result<(), AuthError> {
        let header = decode_header(token).map_err(|error| AuthError::Invalid(error.to_string()))?;

        // Checked before the key is looked up, so that `alg` never decides
        // how a key is used; `decode` checks it once more against
        // `validation.algorithms`.
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Invalid(format!("unexpected algorithm {:?}", header.alg)));
        }

        let kid = header
            .kid
            .ok_or_else(|| AuthError::Invalid("the token names no key".to_owned()))?;
        let key = self.key(&kid)?;

        let claims = decode::<Claims>(token, &key, &self.validation())
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

        let _fetch = self.fetching.lock().unwrap_or_else(PoisonError::into_inner);

        // Another request may have refreshed while this one waited.
        if let Some(key) = self.cached(kid) {
            return Ok(key);
        }

        let attempted_recently = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .attempted_at
            .is_some_and(|attempted| attempted.elapsed() < self.refresh_interval);
        if attempted_recently {
            return Err(AuthError::Invalid(format!("unknown key {kid}")));
        }

        self.refresh()?;

        self.cached(kid)
            .ok_or_else(|| AuthError::Invalid(format!("unknown key {kid}")))
    }

    /// Replaces the cached keys with a fresh fetch. The fetch runs without
    /// the cache lock, so verifications with known keys go on meanwhile. The
    /// attempt is recorded whether it succeeds or not, so a failing issuer is
    /// asked at most once per interval; a failure keeps the last good keys.
    fn refresh(&self) -> Result<(), AuthError> {
        self.keys.write().unwrap_or_else(PoisonError::into_inner).attempted_at = Some(Instant::now());
        let keys = fetch_jwks(&self.config.jwks_url)?;
        self.keys.write().unwrap_or_else(PoisonError::into_inner).keys = keys;
        Ok(())
    }

    fn cached(&self, kid: &str) -> Option<DecodingKey> {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
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
            cache.attempted_at = Some(Instant::now());
        }
        verifier
    }
}

fn fetch_jwks(url: &str) -> Result<HashMap<String, DecodingKey>, AuthError> {
    let body = ureq::get(url)
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()
        .map_err(|error| AuthError::Jwks(error.to_string()))?
        .body_mut()
        .read_to_string()
        .map_err(|error| AuthError::Jwks(error.to_string()))?;

    parse_jwks(&body)
}

/// The keys of a JWK Set by `kid`. Keys without an id or of a kind
/// jsonwebtoken cannot use are skipped, since no token can name them.
fn parse_jwks(body: &str) -> Result<HashMap<String, DecodingKey>, AuthError> {
    let set: JwkSet = serde_json::from_str(body).map_err(|error| AuthError::Jwks(error.to_string()))?;

    Ok(set
        .keys
        .iter()
        .filter_map(|jwk| Some((jwk.common.key_id.clone()?, DecodingKey::from_jwk(jwk).ok()?)))
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::LazyLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use rsa::RsaPrivateKey;
    use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};
    use rsa::traits::PublicKeyParts;
    use serde_json::{Value, json};

    use super::*;

    pub(crate) const ISSUER: &str = "https://issuer.test";

    /// Nothing listens here, so every fetch fails at once.
    const UNREACHABLE_JWKS_URL: &str = "http://127.0.0.1:9/never-fetched";

    /// The issuer's key, published in its JWKS as `test-key`.
    static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_key);
    /// A key the issuer never published.
    static OTHER_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_key);

    fn generate_key() -> RsaPrivateKey {
        RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap()
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
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

    fn config(audience: Option<&str>, allowed_clients: &[&str], jwks_url: &str) -> JwtConfig {
        JwtConfig {
            issuer: ISSUER.to_owned(),
            jwks_url: jwks_url.to_owned(),
            audience: audience.map(str::to_owned),
            allowed_clients: allowed_clients.iter().map(|client| (*client).to_owned()).collect(),
        }
    }

    /// A verifier that already holds the issuer's JWKS and cannot fetch it.
    pub(crate) fn verifier(audience: Option<&str>, allowed_clients: &[&str]) -> JwtVerifier {
        JwtVerifier::with_keys(config(audience, allowed_clients, UNREACHABLE_JWKS_URL), &jwks())
    }

    /// Serves `body` as JSON to the first connection and returns the URL;
    /// a second fetch finds nobody listening.
    fn serve_once(body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/jwks", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 4096];
            let mut read = 0;
            while read < request.len() && !request[..read].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                match stream.read(&mut request[read..]).unwrap() {
                    0 => break,
                    count => read += count,
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
        });

        url
    }

    /// A token signed with the test key, valid for five minutes, with
    /// `overrides` merged into the claims; a `null` override removes the
    /// claim.
    pub(crate) fn token(overrides: Value) -> String {
        sign(&KEY, Some("test-key"), overrides)
    }

    fn claims(overrides: Value) -> Value {
        let now = now();
        let mut claims = json!({
            "iss": ISSUER,
            "aud": ISSUER,
            "sub": "example-client",
            "client_id": "example-client",
            "iat": now,
            "exp": now + 300,
        });
        let Value::Object(overrides) = overrides else {
            panic!("claim overrides must be an object, got {overrides}");
        };
        for (name, value) in overrides {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(&name);
            } else {
                claims[name] = value;
            }
        }
        claims
    }

    fn sign(key: &RsaPrivateKey, kid: Option<&str>, overrides: Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_owned);
        let der = key.to_pkcs1_der().unwrap();
        encode(&header, &claims(overrides), &EncodingKey::from_rsa_der(der.as_bytes())).unwrap()
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
    fn requires_an_audience_only_when_one_is_configured() {
        let without_aud = token(json!({ "aud": null }));

        assert!(verifier(Some(ISSUER), &[]).verify(&without_aud).is_err());
        assert_eq!(verifier(None, &[]).verify(&without_aud), Ok(()));
    }

    #[test]
    fn rejects_expired_tokens_and_foreign_signatures() {
        let verifier = verifier(None, &[]);
        let expired = token(json!({ "exp": 1_000_000_000 }));
        let forged = sign(&OTHER_KEY, Some("test-key"), json!({}));

        assert!(verifier.verify(&expired).is_err());
        assert!(verifier.verify(&forged).is_err());
    }

    #[test]
    fn rejects_tokens_before_their_nbf() {
        let verifier = verifier(None, &[]);

        assert!(verifier.verify(&token(json!({ "nbf": now() + 600 }))).is_err());
        assert_eq!(verifier.verify(&token(json!({ "nbf": now() - 60 }))), Ok(()));
    }

    /// RFC 8725, section 2.1: the public key used as an HMAC secret.
    #[test]
    fn rejects_algorithm_confusion() {
        let public_key = KEY.to_public_key().to_pkcs1_der().unwrap();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".to_owned());
        let confused = encode(
            &header,
            &claims(json!({})),
            &EncodingKey::from_secret(public_key.as_bytes()),
        )
        .unwrap();

        assert_eq!(
            verifier(None, &[]).verify(&confused),
            Err(AuthError::Invalid("unexpected algorithm HS256".to_owned()))
        );
    }

    /// RFC 8725, section 3.1: unsigned tokens.
    #[test]
    fn rejects_unsigned_tokens() {
        let verifier = verifier(None, &[]);
        let encode = |value: Value| URL_SAFE_NO_PAD.encode(value.to_string());
        let none = format!(
            "{}.{}.",
            encode(json!({ "alg": "none", "typ": "JWT", "kid": "test-key" })),
            encode(claims(json!({})))
        );
        let signature_removed = {
            let signed = token(json!({}));
            let (message, _) = signed.rsplit_once('.').unwrap();
            format!("{message}.")
        };

        assert!(verifier.verify(&none).is_err());
        assert!(verifier.verify(&signature_removed).is_err());
    }

    #[test]
    fn rejects_tokens_without_a_key_id() {
        assert_eq!(
            verifier(None, &[]).verify(&sign(&KEY, None, json!({}))),
            Err(AuthError::Invalid("the token names no key".to_owned()))
        );
    }

    #[test]
    fn does_not_refetch_the_jwks_for_unknown_keys_right_after_a_fetch() {
        let token = sign(&OTHER_KEY, Some("unknown-key"), json!({}));

        assert_eq!(
            verifier(None, &[]).verify(&token),
            Err(AuthError::Invalid("unknown key unknown-key".to_owned()))
        );
    }

    #[test]
    fn keeps_the_cached_keys_when_a_refresh_fails() {
        let mut verifier = verifier(None, &[]);
        verifier.refresh_interval = Duration::ZERO;
        let unknown = sign(&OTHER_KEY, Some("unknown-key"), json!({}));

        assert!(matches!(verifier.verify(&unknown), Err(AuthError::Jwks(_))));
        assert!(verifier.cached("test-key").is_some());
        assert_eq!(verifier.verify(&token(json!({}))), Ok(()));
    }

    #[test]
    fn asks_a_failing_issuer_at_most_once_per_interval() {
        let mut verifier = verifier(None, &[]);
        verifier.refresh_interval = Duration::ZERO;
        let unknown = sign(&OTHER_KEY, Some("unknown-key"), json!({}));
        assert!(matches!(verifier.verify(&unknown), Err(AuthError::Jwks(_))));

        // The failed attempt counts: within the interval, no second fetch.
        verifier.refresh_interval = REFRESH_INTERVAL;
        assert_eq!(
            verifier.verify(&unknown),
            Err(AuthError::Invalid("unknown key unknown-key".to_owned()))
        );
    }

    #[test]
    fn preload_fetches_the_jwks_once() {
        let verifier = JwtVerifier::new(config(None, &[], &serve_once(jwks())));
        assert!(verifier.cached("test-key").is_none());

        assert_eq!(verifier.preload(), Ok(()));
        assert!(verifier.cached("test-key").is_some());
        // Served from the cache; the server took only one connection.
        assert_eq!(verifier.verify(&token(json!({}))), Ok(()));
    }

    #[test]
    fn preload_reports_an_unreachable_issuer() {
        let verifier = JwtVerifier::new(config(None, &[], UNREACHABLE_JWKS_URL));

        assert!(matches!(verifier.preload(), Err(AuthError::Jwks(_))));
        assert!(verifier.cached("test-key").is_none());
    }

    #[test]
    fn preload_does_nothing_without_jwt() {
        assert_eq!(Auth::Public.preload(), Ok(()));
        assert_eq!(Auth::ApiKey("secret".to_owned()).preload(), Ok(()));
    }

    #[test]
    fn checks_the_api_key() {
        let auth = Auth::ApiKey("secret".to_owned());

        assert_eq!(auth.authorize(Some("Bearer secret")), Ok(()));
        assert_eq!(auth.authorize(Some("bearer secret")), Ok(()));
        assert_eq!(auth.authorize(Some("Basic secret")), Err(AuthError::Missing));
        assert!(auth.authorize(Some("Bearer wrong")).is_err());
        assert!(auth.authorize(Some("Bearer secre")).is_err());
        assert!(auth.authorize(Some("Bearer secrets")).is_err());
        assert_eq!(auth.authorize(None), Err(AuthError::Missing));
        assert_eq!(Auth::Public.authorize(None), Ok(()));
    }

    #[test]
    fn debug_output_redacts_the_api_key() {
        let debug = format!("{:?}", Auth::ApiKey("s3cret-key".to_owned()));

        assert!(debug.contains("ApiKey"), "{debug}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert_eq!(format!("{:?}", Auth::Public), "Public");
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
