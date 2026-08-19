use secrecy::SecretString;
use tracing::{debug, info, warn, Instrument};

use crate::error::MemoryError;

/// GitHub device flow provider implementation.
pub mod github;
pub use github::GitHubDeviceFlow;

// ---------------------------------------------------------------------------
// DeviceFlowProvider trait (RFC 8628)
// ---------------------------------------------------------------------------

/// Abstraction over OAuth device authorization grant (RFC 8628) parameters.
///
/// Each provider supplies its own client ID, endpoint URLs, and scopes.
/// Implementations must validate their own parameters via [`validate`](Self::validate).
pub trait DeviceFlowProvider: Send + Sync {
    /// Returns the OAuth client ID for this provider.
    fn client_id(&self) -> &str;
    /// Returns the device code endpoint URL.
    fn device_code_url(&self) -> &str;
    /// Returns the access token endpoint URL.
    fn access_token_url(&self) -> &str;
    /// Returns the list of OAuth scopes to request.
    fn scopes(&self) -> &[&str];
    /// Validates provider configuration; returns an error if any value is invalid.
    fn validate(&self) -> Result<(), MemoryError>;
}

// ---------------------------------------------------------------------------
// URL validation helper
// ---------------------------------------------------------------------------

/// Validates that a URL uses HTTPS, with an exception for localhost (dev/testing).
pub(crate) fn validate_endpoint_url(url: &str, field_name: &str) -> Result<(), MemoryError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| MemoryError::OAuth(format!("invalid {field_name} URL: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => Ok(()),
        _ => Err(MemoryError::OAuth(format!(
            "{field_name} must use HTTPS (got {url})"
        ))),
    }
}

// ---------------------------------------------------------------------------
// OAuth response types
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(serde::Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Device flow login
// ---------------------------------------------------------------------------

/// Authenticate via the OAuth device flow and persist the token.
///
/// Prints user-facing prompts to stderr. Never logs the token value.
pub async fn device_flow_login(
    provider: &dyn DeviceFlowProvider,
    store: Option<super::StoreBackend>,
    #[cfg(feature = "k8s")] k8s_config: Option<super::K8sSecretConfig>,
) -> Result<(), MemoryError> {
    use std::time::{Duration, Instant};
    use tokio::time::sleep;

    // Derive a safe provider label from the host of the device code URL, falling
    // back to the literal URL string. Never records client_id (could be secret).
    let provider_label = reqwest::Url::parse(provider.device_code_url())
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| provider.device_code_url().to_owned());

    let span = tracing::info_span!(
        "auth.device_flow_login",
        provider = %provider_label,
        scopes = %provider.scopes().join(" "),
        poll_count = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let start = Instant::now();

    let result = async {
        provider.validate()?;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| MemoryError::OAuth(format!("failed to build HTTP client: {e}")))?;

        let scope = provider.scopes().join(" ");

        // Step 1: Request a device code.
        debug!(
            url = provider.device_code_url(),
            "auth.device_flow: requesting device code"
        );
        let device_resp = async {
            client
                .post(provider.device_code_url())
                .header("Accept", "application/json")
                .form(&[("client_id", provider.client_id()), ("scope", &scope)])
                .send()
                .await
                .map_err(|e| {
                    MemoryError::OAuth(format!("failed to contact device code endpoint: {e}"))
                })?
                .error_for_status()
                .map_err(|e| MemoryError::OAuth(format!("device code request failed: {e}")))?
                .json::<DeviceCodeResponse>()
                .await
                .map_err(|e| {
                    MemoryError::OAuth(format!("failed to parse device code response: {e}"))
                })
        }
        .instrument(tracing::debug_span!("auth.device_flow.request_code"))
        .await?;

        // Compute overall deadline from expires_in, capped at 30 minutes.
        let expires_in = device_resp.expires_in.min(1800);
        let deadline = Instant::now() + Duration::from_secs(expires_in);

        debug!(
            expires_in,
            verification_uri = %device_resp.verification_uri,
            "auth.device_flow: device code obtained"
        );

        // Step 2: Display instructions to the user.
        eprintln!();
        eprintln!("  Open this URL in your browser:");
        eprintln!("    {}", device_resp.verification_uri);
        eprintln!();
        eprintln!("  Enter this code when prompted:");
        eprintln!("    {}", device_resp.user_code);
        eprintln!();
        eprintln!("  Waiting for authorization...");

        // Step 3: Poll for the access token.
        let mut poll_interval = device_resp.interval.clamp(1, 30);
        let mut poll_count: u32 = 0;
        let token = loop {
            if Instant::now() >= deadline {
                tracing::Span::current().record("poll_count", poll_count);
                warn!(
                    poll_count,
                    expires_in, "auth.device_flow: device code expired"
                );
                return Err(MemoryError::OAuth(format!(
                    "Device code expired after {expires_in} seconds"
                )));
            }

            sleep(Duration::from_secs(poll_interval)).await;
            poll_count += 1;

            debug!(
                poll = poll_count,
                interval_secs = poll_interval,
                "auth.device_flow: polling token endpoint"
            );

            let resp = client
                .post(provider.access_token_url())
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", provider.client_id()),
                    ("device_code", device_resp.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .await
                .map_err(|e| MemoryError::OAuth(format!("polling token endpoint failed: {e}")))?
                .error_for_status()
                .map_err(|e| {
                    MemoryError::OAuth(format!("token request returned error status: {e}"))
                })?
                .json::<AccessTokenResponse>()
                .await
                .map_err(|e| MemoryError::OAuth(format!("failed to parse token response: {e}")))?;

            if let Some(tok) = resp.access_token.filter(|t| !t.trim().is_empty()) {
                break SecretString::from(tok);
            }

            match resp.error.as_deref() {
                Some("authorization_pending") => {
                    debug!(poll = poll_count, "auth.device_flow: authorization pending");
                    continue;
                }
                Some("slow_down") => {
                    poll_interval = (poll_interval + 5).min(60);
                    debug!(
                        poll = poll_count,
                        new_interval_secs = poll_interval,
                        "auth.device_flow: slow_down received, backing off"
                    );
                    continue;
                }
                Some("expired_token") => {
                    tracing::Span::current().record("poll_count", poll_count);
                    warn!(
                        poll_count,
                        "auth.device_flow: device code expired during poll"
                    );
                    return Err(MemoryError::OAuth(
                        "device code expired; please run `pali auth login` again".to_string(),
                    ));
                }
                Some("access_denied") => {
                    tracing::Span::current().record("poll_count", poll_count);
                    warn!(poll_count, "auth.device_flow: access denied by user");
                    return Err(MemoryError::OAuth(
                        "authorization denied by user".to_string(),
                    ));
                }
                Some(other) => {
                    let desc = resp
                        .error_description
                        .as_deref()
                        .unwrap_or("no description");
                    tracing::Span::current().record("poll_count", poll_count);
                    warn!(
                        poll_count,
                        error = other,
                        description = desc,
                        "auth.device_flow: unexpected OAuth error"
                    );
                    return Err(MemoryError::OAuth(format!(
                        "unexpected OAuth error '{other}': {desc}"
                    )));
                }
                None => {
                    tracing::Span::current().record("poll_count", poll_count);
                    warn!(
                        poll_count,
                        "auth.device_flow: server returned neither access_token nor error"
                    );
                    return Err(MemoryError::OAuth(
                        "server returned neither an access_token nor an error field; \
                         unexpected response"
                            .to_string(),
                    ));
                }
            }
        };

        tracing::Span::current().record("poll_count", poll_count);
        info!(poll_count, "auth.device_flow: token obtained successfully");

        // Step 4: Store the token.
        super::store_token(
            &token,
            store,
            #[cfg(feature = "k8s")]
            k8s_config,
        )
        .await?;
        eprintln!("Authentication successful.");

        Ok(())
    }
    .instrument(span.clone())
    .await;

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let outcome = if result.is_ok() { "success" } else { "error" };
    span.record("elapsed_ms", elapsed_ms);
    span.record("outcome", outcome);

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDeviceFlow {
        client_id: &'static str,
        device_code_url: &'static str,
        access_token_url: &'static str,
        scopes: &'static [&'static str],
    }

    impl DeviceFlowProvider for MockDeviceFlow {
        fn client_id(&self) -> &str {
            self.client_id
        }
        fn device_code_url(&self) -> &str {
            self.device_code_url
        }
        fn access_token_url(&self) -> &str {
            self.access_token_url
        }
        fn scopes(&self) -> &[&str] {
            self.scopes
        }
        fn validate(&self) -> Result<(), MemoryError> {
            github::validate_github_client_id(self.client_id)?;
            validate_endpoint_url(self.device_code_url, "device_code_url")?;
            validate_endpoint_url(self.access_token_url, "access_token_url")?;
            Ok(())
        }
    }

    fn valid_mock() -> MockDeviceFlow {
        MockDeviceFlow {
            client_id: "test-client-id",
            device_code_url: "https://example.com/device/code",
            access_token_url: "https://example.com/oauth/token",
            scopes: &["repo"],
        }
    }

    // TC-08a: GitHubDeviceFlow returns expected values
    #[test]
    fn github_provider_returns_expected_values() {
        let p = GitHubDeviceFlow;
        assert_eq!(p.client_id(), "Ov23liWxHYkwXTxCrYHp");
        assert_eq!(p.device_code_url(), "https://github.com/login/device/code");
        assert_eq!(
            p.access_token_url(),
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(p.scopes(), &["repo"]);
    }

    // TC-08b: device_flow_login accepts &dyn DeviceFlowProvider (compile-time check)
    #[allow(dead_code)]
    async fn accepts_trait_object(provider: &dyn DeviceFlowProvider) {
        let _ = device_flow_login(
            provider,
            None,
            #[cfg(feature = "k8s")]
            None,
        )
        .await;
    }

    // TC-09a: GitHubDeviceFlow validates OK
    #[test]
    fn github_provider_validates_ok() {
        assert!(GitHubDeviceFlow.validate().is_ok());
    }

    // TC-09b: Empty client ID fails validation
    #[test]
    fn empty_client_id_fails_validation() {
        let mock = MockDeviceFlow {
            client_id: "",
            ..valid_mock()
        };
        let err = mock.validate().unwrap_err();
        assert!(err.to_string().contains("client ID"), "got: {err}");
    }

    // TC-09c: Malformed client ID fails (GitHub-specific format check)
    #[test]
    fn malformed_github_client_id_fails_validation() {
        assert!(github::validate_github_client_id("").is_err());
        assert!(github::validate_github_client_id("x").is_err());
        assert!(github::validate_github_client_id("Ov23liWxHYkwXTxCrYHp").is_ok());
    }

    // TC-10a: HTTP URL fails validation
    #[test]
    fn http_url_fails_validation() {
        let mock = MockDeviceFlow {
            device_code_url: "http://example.com/device/code",
            ..valid_mock()
        };
        assert!(mock.validate().is_err());
    }

    // TC-10b: HTTP localhost passes validation
    #[test]
    fn http_localhost_passes_validation() {
        let mock = MockDeviceFlow {
            device_code_url: "http://localhost/device/code",
            access_token_url: "http://localhost/oauth/token",
            ..valid_mock()
        };
        assert!(mock.validate().is_ok());
    }

    // TC-10c: HTTPS URLs pass validation
    #[test]
    fn https_urls_pass_validation() {
        assert!(valid_mock().validate().is_ok());
    }

    // IPv6 loopback passes validation (host_str returns "[::1]" with brackets)
    #[test]
    fn http_ipv6_localhost_passes_validation() {
        let mock = MockDeviceFlow {
            device_code_url: "http://[::1]/device/code",
            access_token_url: "http://[::1]/oauth/token",
            ..valid_mock()
        };
        assert!(mock.validate().is_ok());
    }

    // Non-loopback IPv6 HTTP URL is rejected
    #[test]
    fn http_ipv6_non_loopback_fails_validation() {
        let mock = MockDeviceFlow {
            device_code_url: "http://[::2]/device/code",
            ..valid_mock()
        };
        assert!(mock.validate().is_err());
    }

    // 127.0.0.1 passes validation
    #[test]
    fn http_127_0_0_1_passes_validation() {
        let mock = MockDeviceFlow {
            device_code_url: "http://127.0.0.1/device/code",
            access_token_url: "http://127.0.0.1/oauth/token",
            ..valid_mock()
        };
        assert!(mock.validate().is_ok());
    }

    /// Device flow requires real OAuth — skip in CI.
    #[tokio::test]
    #[ignore = "requires real OAuth interaction"]
    async fn device_flow_login_ignored_in_ci() {
        device_flow_login(
            &GitHubDeviceFlow,
            Some(super::super::StoreBackend::Stdout),
            #[cfg(feature = "k8s")]
            None,
        )
        .await
        .expect("device flow should succeed");
    }
}
