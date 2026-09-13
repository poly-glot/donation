use std::collections::HashMap;
use std::sync::RwLock;

use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::AppError;

const ACCESS_TOKEN: &str = "access";
const RS256: &str = "RS256";

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    kid: String,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
}

#[derive(Debug, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub client_id: String,
    pub token_use: String,
    pub iss: String,
    pub exp: i64,
}

pub struct Cognito {
    audience: String,
    issuer: String,
    jwks_url: String,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, Vec<u8>>>,
}

fn decode(segment: &str) -> Result<Vec<u8>, AppError> {
    URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|err| AppError::Forbidden(format!("token is not base64url: {err}")))
}

fn parse<T: serde::de::DeserializeOwned>(segment: &str) -> Result<T, AppError> {
    serde_json::from_slice(&decode(segment)?).map_err(|err| AppError::Forbidden(format!("token segment is not the expected JSON: {err}")))
}

pub fn bearer(authorization: Option<&str>) -> Result<&str, AppError> {
    let Some(header) = authorization else {
        return Err(AppError::Forbidden("no Authorization header".into()));
    };
    let Some(token) = header.strip_prefix("Bearer ") else {
        return Err(AppError::Forbidden("Authorization is not a Bearer token".into()));
    };

    Ok(token.trim())
}

impl Cognito {
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>, jwks_url: impl Into<String>) -> Self {
        Self {
            audience: audience.into(),
            issuer: issuer.into(),
            jwks_url: jwks_url.into(),
            http: reqwest::Client::new(),
            keys: RwLock::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Result<Self, AppError> {
        let named = |name: &str| std::env::var(name).map_err(|_| AppError::Internal(format!("{name} is not set")));
        let issuer = named("COGNITO_ISSUER")?;
        let jwks_url = std::env::var("COGNITO_JWKS_URL").unwrap_or_else(|_| format!("{}/.well-known/jwks.json", issuer.trim_end_matches('/')));

        Ok(Self::new(issuer, named("COGNITO_CLIENT_ID")?, jwks_url))
    }

    pub async fn verify(&self, token: &str, now: DateTime<Utc>) -> Result<Claims, AppError> {
        let [head, body, signature] = token.split('.').collect::<Vec<_>>()[..] else {
            return Err(AppError::Forbidden("token is not three dot-separated segments".into()));
        };

        let header: Header = parse(head)?;
        if header.alg != RS256 {
            return Err(AppError::Forbidden(format!("token algorithm is {}, not {RS256}", header.alg)));
        }

        let signed = format!("{head}.{body}");
        self.verify_signature(&header.kid, signed.as_bytes(), &decode(signature)?).await?;

        let claims: Claims = parse(body)?;
        self.validate(&claims, now)?;
        Ok(claims)
    }

    fn validate(&self, claims: &Claims, now: DateTime<Utc>) -> Result<(), AppError> {
        if claims.iss != self.issuer {
            return Err(AppError::Forbidden("token was issued by another pool".into()));
        }
        if claims.client_id != self.audience {
            return Err(AppError::Forbidden("token was issued to another client".into()));
        }
        if claims.token_use != ACCESS_TOKEN {
            return Err(AppError::Forbidden(format!(
                "token is a {} token, not an {ACCESS_TOKEN} token",
                claims.token_use
            )));
        }
        if claims.exp <= now.timestamp() {
            return Err(AppError::Forbidden("token has expired".into()));
        }
        Ok(())
    }

    async fn verify_signature(&self, kid: &str, signed: &[u8], signature: &[u8]) -> Result<(), AppError> {
        if let Some(modulus) = self.modulus(kid) {
            return check(&modulus, signed, signature);
        }

        self.refresh().await?;
        let Some(modulus) = self.modulus(kid) else {
            return Err(AppError::Forbidden(format!("no signing key {kid} in the pool's JWKS")));
        };
        check(&modulus, signed, signature)
    }

    fn modulus(&self, kid: &str) -> Option<Vec<u8>> {
        let keys = self.keys.read().ok()?;
        keys.get(kid).cloned()
    }

    async fn refresh(&self) -> Result<(), AppError> {
        let unreachable = |err: reqwest::Error| AppError::Internal(format!("JWKS fetch failed: {err}"));
        let body = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(unreachable)?
            .text()
            .await
            .map_err(unreachable)?;
        let jwks: Jwks = serde_json::from_str(&body).map_err(|err| AppError::Internal(format!("JWKS is not the expected JSON: {err}")))?;

        let mut fetched = HashMap::new();
        for key in jwks.keys {
            fetched.insert(key.kid, decode(&key.n)?);
        }

        let mut keys = self.keys.write().map_err(|_| AppError::Internal("the JWKS cache is poisoned".into()))?;
        *keys = fetched;
        Ok(())
    }
}

fn check(modulus: &[u8], signed: &[u8], signature: &[u8]) -> Result<(), AppError> {
    let key = RsaPublicKeyComponents {
        n: modulus,
        e: &[0x01, 0x00, 0x01],
    };

    key.verify(&RSA_PKCS1_2048_8192_SHA256, signed, signature)
        .map_err(|_| AppError::Forbidden("token signature does not match the pool's signing key".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6IkNvZ25pdG9Mb2NhbCJ9.eyJhdXRoX3RpbWUiOjE3ODkyODk3MTgsImNsaWVudF9pZCI6IjEwYjkxbzFzbHB4cDJ5ZGZnbHpwb3I5cGciLCJldmVudF9pZCI6IjNlZDA4YWZlLTkwYzQtNDVmZS05YTQ3LTQyMmZmNDU4YWRlMCIsImlhdCI6MTc4OTI4OTcxOCwianRpIjoiOGIxNzlhZjEtNjA4OC00ZWRkLThmYjYtNjg0NzFlMGMxMDY3Iiwic2NvcGUiOiJhd3MuY29nbml0by5zaWduaW4udXNlci5hZG1pbiIsInN1YiI6ImZhZjI3NTlhLTc5MTgtNDllMS1hOTAyLWVmMzdkZDA1YjI3MiIsInRva2VuX3VzZSI6ImFjY2VzcyIsInVzZXJuYW1lIjoiZmFmMjc1OWEtNzkxOC00OWUxLWE5MDItZWYzN2RkMDViMjcyIiwiZXhwIjoxNzg5Mzc2MTE4LCJpc3MiOiJodHRwOi8vMC4wLjAuMDo5MjI5L2xvY2FsXzRrOVZEMEhlIn0.PGn-B1kiB4xP0OnQyeBbSC_wJmw8DUhnPOAYeRbSxTUkhFjR9KHQ7zS7ddNeUa0DEZZpdArHmamTFXI_jH5nM4WwbPJOLZcwNLNn0pA_QYZW26BJQhDQQ7rah4LfFn3_rzAdqSWedMQaYsBEJLf4eYvN0B23yU334IxlzYtLIsCFn5hDfE_GQqTOgmvit6p_gINT0NpLAbCE51pmFwVXnGmRx4QKXk3hMMYlkBMo2yO1v6u3axAaoHMqiTfrlE306tnivajd70lyJ59BWnJQqH6j4PdbuI4m7BcFyTZbcUasDYbKcEAFxVSFeWnUX3Rs3dTK1gT0cRZCVanfkXWUeg";
    const MODULUS: &str = "2uLO7yh1_6Icfd89V3nNTc_qhfpDN7vEmOYlmJQlc9_RmOns26lg88fXXFntZESwHOm7_homO2Ih6NOtu4P5eskGs8d8VQMOQfF4YrP-pawVz-gh1S7eSvzZRDHBT4ItUuoiVP1B9HN_uScKxIqjmitpPqEQB_o2NJv8npCfqUAU-4KmxquGtjdmfctswSZGdz59M3CAYKDfuvLH9_vV6TRGgbUaUAXWC2WJrbbEXzK3XUDBrmF3Xo-yw8f3SgD3JOPl3HaaWMKL1zGVAsge7gQaGiJBzBurg5vwN61uDGGz0QZC1JqcUTl3cZnrx_L8isIR7074SJEuljIZRnCcjQ";
    const ISSUER: &str = "http://0.0.0.0:9229/local_4k9VD0He";
    const CLIENT_ID: &str = "10b91o1slpxp2ydfglzpor9pg";
    const KID: &str = "CognitoLocal";
    const ISSUED_AT: i64 = 1789289778;
    const EXPIRES_AT: i64 = 1789376118;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("a valid timestamp")
    }

    fn pool(issuer: &str, audience: &str) -> Cognito {
        let cognito = Cognito::new(issuer, audience, "http://jwks.invalid");
        let modulus = URL_SAFE_NO_PAD.decode(MODULUS).expect("the fixture modulus is base64url");

        cognito.keys.write().expect("a fresh lock").insert(KID.to_string(), modulus);
        cognito
    }

    #[tokio::test]
    async fn a_token_the_pool_signed_is_accepted_and_its_claims_come_back() {
        let claims = pool(ISSUER, CLIENT_ID).verify(TOKEN, at(ISSUED_AT)).await.expect("the fixture token verifies");

        assert_eq!(claims.client_id, CLIENT_ID, "the client the token was minted for");
        assert_eq!(claims.token_use, ACCESS_TOKEN, "an access token, not an id token");
        assert_eq!(claims.iss, ISSUER, "the pool that signed it");
    }

    #[tokio::test]
    async fn a_token_whose_signature_does_not_match_the_key_is_refused() {
        let (head, tail) = TOKEN.rsplit_once('.').expect("three segments");
        let flipped = format!("{head}.{}A", &tail[..tail.len() - 1]);

        let refused = pool(ISSUER, CLIENT_ID).verify(&flipped, at(ISSUED_AT)).await;
        assert!(matches!(&refused, Err(AppError::Forbidden(_))), "got {refused:?}");
    }

    #[tokio::test]
    async fn a_payload_edited_after_signing_is_refused() {
        let [head, body, signature] = TOKEN.split('.').collect::<Vec<_>>()[..] else {
            panic!("the fixture token has three segments");
        };
        let mut claims: serde_json::Value = serde_json::from_slice(&decode(body).expect("base64url")).expect("JSON");
        claims["sub"] = serde_json::json!("somebody-else");
        let forged = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("re-encodes"));

        let refused = pool(ISSUER, CLIENT_ID).verify(&format!("{head}.{forged}.{signature}"), at(ISSUED_AT)).await;
        assert!(matches!(&refused, Err(AppError::Forbidden(_))), "got {refused:?}");
    }

    #[tokio::test]
    async fn a_token_the_pool_signed_is_still_refused_when_it_does_not_belong_here() {
        let cases = [
            ("a token from another user pool", pool("https://another.pool", CLIENT_ID), at(ISSUED_AT)),
            ("a token minted for another app client", pool(ISSUER, "another-client"), at(ISSUED_AT)),
            ("a token that has expired", pool(ISSUER, CLIENT_ID), at(EXPIRES_AT)),
        ];
        for (label, cognito, now) in cases {
            let refused = cognito.verify(TOKEN, now).await;
            assert!(matches!(&refused, Err(AppError::Forbidden(_))), "{label}: got {refused:?}");
        }
    }

    #[tokio::test]
    async fn a_token_that_is_not_a_signed_rs256_jwt_never_reaches_the_key() {
        let unsigned_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","kid":"CognitoLocal"}"#);
        let cases = [
            ("a bare word", "nonsense".to_string()),
            ("only two segments", TOKEN.rsplit_once('.').expect("three segments").0.to_string()),
            ("a segment that is not base64url", format!("!!!.{TOKEN}")),
            ("an unsigned token claiming alg none", format!("{unsigned_header}.e30.")),
        ];
        for (label, token) in cases {
            let refused = pool(ISSUER, CLIENT_ID).verify(&token, at(ISSUED_AT)).await;
            assert!(matches!(&refused, Err(AppError::Forbidden(_))), "{label}: got {refused:?}");
        }
    }
}
