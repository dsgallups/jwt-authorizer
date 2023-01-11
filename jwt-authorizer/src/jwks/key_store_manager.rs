use jsonwebtoken::{
    jwk::{Jwk, JwkSet},
    Algorithm, DecodingKey,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

use crate::error::AuthError;

#[derive(Clone)]
pub enum RefreshStrategy {
    /// refresh periodicaly
    Interval(Duration),
    /// when kid not found in the store
    KeyNotFound,
    // other strategies? KeyNotFoundOrInterval(Duration), Once,
}

#[derive(Clone)]
pub struct KeyStoreManager {
    key_url: String,
    refresh: RefreshStrategy,
    /// in case of fail loading (error or key not found), minimal interval
    minimal_refresh_interval: Duration,
    keystore: Arc<Mutex<KeyStore>>,
}

pub struct KeyStore {
    /// key set
    jwks: JwkSet,
    /// time of the last successfully loaded jwkset
    load_time: Option<Instant>,
    /// time of the last failed load
    fail_time: Option<Instant>,
}

impl KeyStoreManager {
    pub(crate) fn new(url: &str, refresh: RefreshStrategy) -> KeyStoreManager {
        KeyStoreManager {
            key_url: url.to_owned(),
            refresh,
            minimal_refresh_interval: Duration::from_secs(5), // TODO: make configurable
            keystore: Arc::new(Mutex::new(KeyStore {
                jwks: JwkSet { keys: vec![] },
                load_time: None,
                fail_time: None,
            })),
        }
    }

    pub(crate) fn with_refresh(url: &str) -> KeyStoreManager {
        KeyStoreManager::new(url, RefreshStrategy::KeyNotFound)
    }

    pub(crate) fn with_refresh_interval(url: &str, interval: Duration) -> KeyStoreManager {
        KeyStoreManager::new(url, RefreshStrategy::Interval(interval))
    }

    pub(crate) async fn get_key(&self, header: &jsonwebtoken::Header) -> Result<jsonwebtoken::DecodingKey, AuthError> {
        let kstore = self.keystore.clone();
        let mut ks_gard = kstore.lock().await;
        let key = match self.refresh {
            RefreshStrategy::Interval(refresh_interval) => {
                if ks_gard.should_refresh(refresh_interval) && ks_gard.can_refresh(self.minimal_refresh_interval) {
                    ks_gard.refresh(&self.key_url, &[]).await?;
                }
                if let Some(ref kid) = header.kid {
                    ks_gard
                        .find_kid(kid)
                        .ok_or_else(|| AuthError::InvalidKid(kid.to_owned()))?
                } else {
                    ks_gard
                        .find_alg(&header.alg)
                        .ok_or(AuthError::InvalidKeyAlg(header.alg))?
                }
            }
            RefreshStrategy::KeyNotFound => {
                if let Some(ref kid) = header.kid {
                    let jwk_opt = ks_gard.find_kid(kid);
                    if let Some(jwk) = jwk_opt {
                        jwk
                    } else if ks_gard.can_refresh(self.minimal_refresh_interval) {
                        ks_gard.refresh(&self.key_url, &[("kid", kid)]).await?;
                        ks_gard
                            .find_kid(kid)
                            .ok_or_else(|| AuthError::InvalidKid(kid.to_owned()))?
                    } else {
                        return Err(AuthError::InvalidKid(kid.to_owned()));
                    }
                } else {
                    let jwk_opt = ks_gard.find_alg(&header.alg);
                    // .ok_or(AuthError::InvalidKeyAlg(header.alg))?
                    if let Some(jwk) = jwk_opt {
                        jwk
                    } else if ks_gard.can_refresh(self.minimal_refresh_interval) {
                        ks_gard
                            .refresh(
                                &self.key_url,
                                &[(
                                    "alg",
                                    &serde_json::to_string(&header.alg)
                                        .map_err(|_| AuthError::InvalidKeyAlg(header.alg))?,
                                )],
                            )
                            .await?;
                        ks_gard
                            .find_alg(&header.alg)
                            .ok_or_else(|| AuthError::InvalidKeyAlg(header.alg))?
                    } else {
                        return Err(AuthError::InvalidKeyAlg(header.alg));
                    }
                }
            }
        };

        DecodingKey::from_jwk(key).map_err(|err| AuthError::InvalidKey(err.to_string()))
    }
}

impl KeyStore {
    fn should_refresh(&self, refresh_interval: Duration) -> bool {
        if let Some(t) = self.load_time {
            t.elapsed() > refresh_interval
        } else {
            true
        }
    }

    fn can_refresh(&self, minimal_refresh_interval: Duration) -> bool {
        if let Some(ft) = self.fail_time {
            if let Some(lt) = self.load_time {
                ft.elapsed() > minimal_refresh_interval && lt.elapsed() > minimal_refresh_interval
            } else {
                ft.elapsed() > minimal_refresh_interval
            }
        } else if let Some(lt) = self.load_time {
            lt.elapsed() > minimal_refresh_interval
        } else {
            true
        }
    }

    async fn refresh(&mut self, key_url: &str, qparam: &[(&str, &str)]) -> Result<(), AuthError> {
        reqwest::Client::new()
            .get(key_url)
            .query(qparam)
            .send()
            .await
            .map_err(AuthError::JwksRefreshError)?
            .json::<JwkSet>()
            .await
            .map(|jwks| {
                self.load_time = Some(Instant::now());
                self.jwks = jwks;
                Ok(())
            })
            .map_err(|e| {
                self.fail_time = Some(Instant::now());
                AuthError::JwksRefreshError(e)
            })?
    }

    /// Find the key in the set that matches the given key id, if any.
    pub fn find_kid(&self, kid: &str) -> Option<&Jwk> {
        self.jwks.find(kid)
    }

    /// Find the key in the set that matches the given key id, if any.
    pub fn find_alg(&self, alg: &Algorithm) -> Option<&Jwk> {
        self.jwks.keys.iter().find(|jwk| {
            if let Some(ref a) = jwk.common.algorithm {
                alg == a
            } else {
                false
            }
        })
    }

    /// Find first key.
    pub fn find_first(&self) -> Option<&Jwk> {
        self.jwks.keys.get(0)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use jsonwebtoken::Algorithm;
    use jsonwebtoken::{jwk::Jwk, Header};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::jwks::key_store_manager::{KeyStore, KeyStoreManager};

    #[test]
    fn keystore_should_refresh() {
        let ks = KeyStore {
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![] },
            fail_time: None,
            load_time: Some(Instant::now()),
        };

        assert!(!ks.should_refresh(Duration::from_secs(5)));

        let ks = KeyStore {
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![] },
            fail_time: None,
            load_time: Some(Instant::now() - Duration::from_secs(6)),
        };

        assert!(ks.should_refresh(Duration::from_secs(5)));
    }

    #[test]
    fn keystore_can_refresh() {
        let ks = KeyStore {
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![] },
            fail_time: Some(Instant::now() - Duration::from_secs(5)),
            load_time: None,
        };
        assert!(ks.can_refresh(Duration::from_secs(4)));
        assert!(!ks.can_refresh(Duration::from_secs(6)));

        let ks = KeyStore {
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![] },
            fail_time: None,
            load_time: Some(Instant::now() - Duration::from_secs(5)),
        };
        assert!(ks.can_refresh(Duration::from_secs(4)));
        assert!(!ks.can_refresh(Duration::from_secs(6)));
    }

    #[test]
    fn find_kid() {
        let jwk0: Jwk = serde_json::from_str(r#"{"kid":"1","kty":"RSA","alg":"RS256","n":"xxxx","e":"AQAB"}"#).unwrap();
        let jwk1: Jwk = serde_json::from_str(r#"{"kid":"2","kty":"RSA","alg":"RS256","n":"xxxx","e":"AQAB"}"#).unwrap();
        let ks = KeyStore {
            load_time: None,
            fail_time: None,
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![jwk0, jwk1] },
        };
        assert!(ks.find_kid("1").is_some());
        assert!(ks.find_kid("2").is_some());
        assert!(ks.find_kid("3").is_none());
    }

    #[test]
    fn find_alg() {
        let jwk0: Jwk = serde_json::from_str(r#"{"kty": "RSA", "alg": "RS256", "n": "xxx","e": "yyy"}"#).unwrap();
        let ks = KeyStore {
            load_time: None,
            fail_time: None,
            jwks: jsonwebtoken::jwk::JwkSet { keys: vec![jwk0] },
        };
        assert!(ks.find_alg(&Algorithm::RS256).is_some());
        assert!(ks.find_alg(&Algorithm::EdDSA).is_none());
    }

    async fn mock_jwks_response_once(mock_server: &MockServer, jwk: &str) {
        let jwk0: Jwk = serde_json::from_str(jwk).unwrap();
        let jwks = jsonwebtoken::jwk::JwkSet { keys: vec![jwk0] };
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&jwks))
            .expect(1)
            .mount(&mock_server)
            .await;
    }

    fn build_header(kid: &str, alg: Algorithm) -> Header {
        let mut header = Header::new(alg);
        header.kid = Some(kid.to_owned());
        header
    }

    #[tokio::test]
    async fn keystore_manager_find_key_with_refresh_interval() {
        let mock_server = MockServer::start().await;
        mock_jwks_response_once(
            &mock_server,
            r#"{
                "kty": "OKP",
                "use": "sig",
                "crv": "Ed25519",
                "x": "uWtSkE-I9aTMYTTvuTE1rtu0rNdxp3DU33cJ_ksL1Gk",
                "kid": "key-ed",
                "alg": "EdDSA"
              }"#,
        )
        .await;

        let ksm = KeyStoreManager::with_refresh_interval(&mock_server.uri(), Duration::from_secs(3000));
        let r = ksm.get_key(&Header::new(Algorithm::EdDSA)).await;
        assert!(r.is_ok());
        mock_server.verify().await;
    }

    #[tokio::test]
    async fn keystore_manager_find_key_with_refresh() {
        let mock_server = MockServer::start().await;
        mock_jwks_response_once(
            &mock_server,
            r#"{
                "kty": "OKP",
                "use": "sig",
                "crv": "Ed25519",
                "x": "uWtSkE-I9aTMYTTvuTE1rtu0rNdxp3DU33cJ_ksL1Gk",
                "kid": "key-ed",
                "alg": "EdDSA"
              }"#,
        )
        .await;

        let mut ksm = KeyStoreManager::with_refresh(&mock_server.uri());

        // STEP 1: initial (lazy) reloading
        let r = ksm.get_key(&build_header("key-ed", Algorithm::EdDSA)).await;
        assert!(r.is_ok());
        mock_server.verify().await;

        // STEP2: new kid -> reloading ksm
        mock_server.reset().await;
        mock_jwks_response_once(
            &mock_server,
            r#"{
                "kty": "OKP",
                "use": "sig",
                "crv": "Ed25519",
                "x": "uWtSkE-I9aTMYTTvuTE1rtu0rNdxp3DU33cJ_ksL1Gk",
                "kid": "key-ed02",
                "alg": "EdDSA"
              }"#,
        )
        .await;
        let h = build_header("key-ed02", Algorithm::EdDSA);
        assert!(ksm.get_key(&h).await.is_err());

        ksm.minimal_refresh_interval = Duration::from_millis(100);
        tokio::time::sleep(Duration::from_millis(101)).await;
        assert!(ksm.get_key(&h).await.is_ok());

        mock_server.verify().await;

        // STEP3: new algorithm -> try to reload
        mock_server.reset().await;
        mock_jwks_response_once(
            &mock_server,
            r#"{
                "kty": "EC",
                "crv": "P-256",
                "x": "w7JAoU_gJbZJvV-zCOvU9yFJq0FNC_edCMRM78P8eQQ",
                "y": "wQg1EytcsEmGrM70Gb53oluoDbVhCZ3Uq3hHMslHVb4",
                "kid": "ec01",
                "alg": "ES256",
                "use": "sig"
              }"#,
        )
        .await;
        let h = Header::new(Algorithm::ES256);
        assert!(ksm.get_key(&h).await.is_err());

        tokio::time::sleep(Duration::from_millis(101)).await;
        assert!(ksm.get_key(&h).await.is_ok());

        mock_server.verify().await;
    }
}