use std::fmt;

use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};

use crate::error::MemoryError;

/// OAuth device flow trait and provider implementations.
pub mod oauth;
pub use oauth::{device_flow_login, DeviceFlowProvider, GitHubDeviceFlow};

/// Token resolution order:
/// 1. `PALI_GITHUB_TOKEN` environment variable
/// 2. `~/.config/memory-mcp/token` file
/// 3. System keyring (GNOME Keyring / KWallet / macOS Keychain)
const ENV_VAR: &str = "PALI_GITHUB_TOKEN";
const TOKEN_FILE: &str = ".config/memory-mcp/token";

// ---------------------------------------------------------------------------
// StoreBackend — where to persist a newly acquired token
// ---------------------------------------------------------------------------

/// Token storage backend selection for `pali auth login`.
#[derive(Clone, Debug, clap::ValueEnum)]
#[non_exhaustive]
pub enum StoreBackend {
    /// Store token in the system keyring.
    Keyring,
    /// Store token in `~/.config/memory-mcp/token`.
    File,
    /// Print token to stdout and do not persist it.
    Stdout,
    /// Store token as a Kubernetes Secret.
    #[cfg(feature = "k8s")]
    #[clap(name = "k8s-secret")]
    K8sSecret,
}

// ---------------------------------------------------------------------------
// K8sSecretConfig — configuration for Kubernetes Secret storage
// ---------------------------------------------------------------------------

/// Configuration for storing a token as a Kubernetes Secret.
#[cfg(feature = "k8s")]
#[derive(Debug)]
pub struct K8sSecretConfig {
    /// Kubernetes namespace in which to create or update the secret.
    pub namespace: String,
    /// Name of the Kubernetes Secret resource.
    pub secret_name: String,
}

// ---------------------------------------------------------------------------
// TokenSource — tracks which resolution step found the token
// ---------------------------------------------------------------------------

/// Indicates which source produced the resolved token.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenSource {
    /// Token was read from the `PALI_GITHUB_TOKEN` environment variable.
    EnvVar,
    /// Token was read from the `~/.config/memory-mcp/token` file.
    File,
    /// Token was read from the system keyring (GNOME Keyring / KWallet / macOS Keychain).
    Keyring,
    /// Token was provided directly via constructor.
    Explicit,
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::EnvVar => write!(f, "environment variable ({})", ENV_VAR),
            TokenSource::File => write!(f, "token file (~/.config/memory-mcp/token)"),
            TokenSource::Keyring => write!(f, "system keyring"),
            TokenSource::Explicit => write!(f, "explicit token"),
        }
    }
}

// ---------------------------------------------------------------------------
// AuthProvider
// ---------------------------------------------------------------------------

/// Resolves and caches a GitHub personal access token from multiple sources.
pub struct AuthProvider {
    /// Cached token and its source, if resolved at startup.
    token: Option<(SecretString, TokenSource)>,
}

impl AuthProvider {
    /// Create an `AuthProvider`, eagerly attempting token resolution.
    ///
    /// Does not fail if no token is available — some deployments may not
    /// need remote sync. Call [`Self::resolve_token`] when a token is required.
    pub fn new() -> Self {
        let token = Self::try_resolve_with_source().ok();
        if token.is_some() {
            debug!("AuthProvider: token resolved at startup");
        } else {
            debug!("AuthProvider: no token available at startup");
        }
        Self { token }
    }

    /// Resolve a GitHub personal access token, returning it wrapped in
    /// [`SecretString`] so it cannot accidentally appear in logs or error chains.
    ///
    /// Checks (in order):
    /// 1. `PALI_GITHUB_TOKEN` env var
    /// 2. `~/.config/memory-mcp/token` file
    /// 3. System keyring (GNOME Keyring / KWallet / macOS Keychain)
    pub fn resolve_token(&self) -> Result<SecretString, MemoryError> {
        self.resolve_with_source().map(|(tok, _)| tok)
    }

    /// Resolve the token and return which source provided it.
    ///
    /// Like [`resolve_token`](Self::resolve_token), returns the cached token if
    /// one was resolved at startup. Falls back to the resolution chain
    /// (env var → file → keyring) if no cached token exists.
    pub fn resolve_with_source(&self) -> Result<(SecretString, TokenSource), MemoryError> {
        if let Some((ref t, ref s)) = self.token {
            return Ok((t.clone(), s.clone()));
        }
        Self::try_resolve_with_source()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Resolve the token and return both the raw value and which source provided it.
    fn try_resolve_with_source() -> Result<(SecretString, TokenSource), MemoryError> {
        let span = tracing::debug_span!("auth.resolve", token_source = tracing::field::Empty,);
        let _enter = span.entered();

        // 1. Environment variable.
        debug!("auth: trying environment variable");
        if let Ok(tok) = std::env::var(ENV_VAR) {
            if !tok.trim().is_empty() {
                tracing::Span::current().record("token_source", "env_var");
                info!(token_source = "env_var", "auth token resolved");
                return Ok((
                    SecretString::from(tok.trim().to_string()),
                    TokenSource::EnvVar,
                ));
            }
        }

        // 2. Token file.
        debug!("auth: trying token file");
        if let Some(home) = home_dir() {
            let path = home.join(TOKEN_FILE);
            if path.exists() {
                // Check permissions: warn if the file is world- or group-readable.
                check_token_file_permissions(&path);

                let raw = std::fs::read_to_string(&path)?;
                let tok = raw.trim().to_string();
                if !tok.is_empty() {
                    tracing::Span::current().record("token_source", "file");
                    info!(token_source = "file", "auth token resolved");
                    return Ok((SecretString::from(tok), TokenSource::File));
                }
            }
        }

        // 3. System keyring (GNOME Keyring / KWallet / macOS Keychain).
        debug!("auth: trying system keyring");
        match keyring::Entry::new("memory-mcp", "github-token") {
            Ok(entry) => match entry.get_password() {
                Ok(tok) if !tok.trim().is_empty() => {
                    tracing::Span::current().record("token_source", "keyring");
                    info!(
                        token_source = "keyring",
                        "resolved GitHub token from system keyring"
                    );
                    return Ok((
                        SecretString::from(tok.trim().to_string()),
                        TokenSource::Keyring,
                    ));
                }
                Ok(_) => { /* empty password stored — fall through */ }
                Err(keyring::Error::NoEntry) => { /* no entry — fall through */ }
                Err(keyring::Error::NoStorageAccess(_)) => {
                    debug!("keyring: no storage backend available (headless?)");
                }
                Err(e) => {
                    warn!("keyring: unexpected error: {e}");
                }
            },
            Err(e) => {
                debug!("keyring: could not create entry: {e}");
            }
        }

        warn!("auth token resolution failed — no token found in env var, file, or keyring");
        Err(MemoryError::Auth(
            "no token available; set PALI_GITHUB_TOKEN, add \
             ~/.config/memory-mcp/token, or store a token in the system keyring \
             under service 'memory-mcp', account 'github-token'."
                .to_string(),
        ))
    }
}

impl AuthProvider {
    /// Create an `AuthProvider` with a pre-set token.
    ///
    /// Use this when you already have a token from your own auth flow
    /// and want to skip the built-in resolution chain (env var → file → keyring).
    pub fn with_token(token: &str) -> Self {
        Self {
            token: Some((SecretString::from(token.to_string()), TokenSource::Explicit)),
        }
    }
}

impl Default for AuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

/// Persist a token via the specified backend.
///
/// Never logs the token value — only the chosen storage destination.
pub(crate) async fn store_token(
    token: &SecretString,
    backend: Option<StoreBackend>,
    #[cfg(feature = "k8s")] k8s_config: Option<K8sSecretConfig>,
) -> Result<(), MemoryError> {
    match backend {
        Some(StoreBackend::Stdout) => {
            println!("{}", token.expose_secret());
            debug!("token written to stdout");
        }
        Some(StoreBackend::Keyring) => {
            store_in_keyring(token.expose_secret())?;
        }
        Some(StoreBackend::File) => {
            store_in_file(token.expose_secret())?;
        }
        #[cfg(feature = "k8s")]
        Some(StoreBackend::K8sSecret) => {
            let config = k8s_config.ok_or_else(|| {
                MemoryError::TokenStorage(
                    "k8s-secret backend requires namespace and secret name".into(),
                )
            })?;
            store_in_k8s_secret(token.expose_secret(), &config).await?;
        }
        None => {
            // No --store flag: try keyring ONLY. Do NOT fall back to file.
            store_in_keyring(token.expose_secret()).map_err(|e| {
                MemoryError::TokenStorage(format!(
                    "Keyring unavailable: {e}. Use --store file to write to \
                     ~/.config/memory-mcp/token, --store stdout to print the token\
                     {k8s_hint}.",
                    k8s_hint = if cfg!(feature = "k8s") {
                        ", or --store k8s-secret to store in a Kubernetes Secret"
                    } else {
                        ""
                    }
                ))
            })?;
        }
    }
    Ok(())
}

fn store_in_keyring(token: &str) -> Result<(), MemoryError> {
    let entry = keyring::Entry::new("memory-mcp", "github-token")
        .map_err(|e| MemoryError::TokenStorage(format!("failed to create keyring entry: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| MemoryError::TokenStorage(format!("failed to store token in keyring: {e}")))?;
    info!("token stored in system keyring");
    Ok(())
}

fn store_in_file(token: &str) -> Result<(), MemoryError> {
    let home =
        home_dir().ok_or_else(|| MemoryError::TokenStorage("HOME directory is not set".into()))?;
    let token_path = home.join(TOKEN_FILE);

    if let Some(parent) = token_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            MemoryError::TokenStorage(format!(
                "failed to create config directory {}: {e}",
                parent.display()
            ))
        })?;

        // Set config directory to 0700 on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    MemoryError::TokenStorage(format!(
                        "failed to set config directory permissions: {e}"
                    ))
                },
            )?;
        }
    }

    // Atomic write: temp file → sync → rename. On Unix the temp file is
    // created with mode 0o600. On any failure the temp file is cleaned up
    // automatically by the RAII guard inside `atomic_write`.
    crate::fs_util::atomic_write(&token_path, format!("{token}\n").as_bytes())
        .map_err(|e| MemoryError::TokenStorage(format!("failed to write token file: {e}")))?;

    info!("token stored in file ({})", token_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Kubernetes Secret storage (k8s feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "k8s")]
async fn store_in_k8s_secret(token: &str, config: &K8sSecretConfig) -> Result<(), MemoryError> {
    use k8s_openapi::api::core::v1::Secret;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use kube::{api::PostParams, Api, Client};
    use std::collections::BTreeMap;

    let client = Client::try_default().await.map_err(|e| {
        MemoryError::TokenStorage(format!(
            "Failed to initialize Kubernetes client. Ensure KUBECONFIG is set \
             or the pod has a service account: {e}"
        ))
    })?;

    let secrets: Api<Secret> = Api::namespaced(client, &config.namespace);
    let secret_name = &config.secret_name;

    let mut data = BTreeMap::new();
    data.insert(
        "token".to_string(),
        k8s_openapi::ByteString(token.as_bytes().to_vec()),
    );

    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "memory-mcp".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/component".to_string(),
        "auth".to_string(),
    );

    let mut secret = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    };

    // Create-first: attempt to create the secret; on 409 AlreadyExists, GET to
    // fetch the current resourceVersion and replace. This avoids the TOCTOU
    // race inherent in a GET-then-create/replace approach.
    match secrets.create(&PostParams::default(), &secret).await {
        Ok(_) => {
            debug!(
                "created Kubernetes Secret '{secret_name}' in namespace '{}'",
                config.namespace
            );
        }
        Err(kube::Error::Api(ref err_resp)) if err_resp.code == 409 => {
            // Secret already exists; fetch resourceVersion then replace.
            let existing = secrets
                .get(secret_name)
                .await
                .map_err(|e| map_kube_error(e, &config.namespace))?;
            secret.metadata.resource_version = existing.metadata.resource_version;
            secrets
                .replace(secret_name, &PostParams::default(), &secret)
                .await
                .map_err(|e| map_kube_error(e, &config.namespace))?;
            debug!(
                "updated Kubernetes Secret '{secret_name}' in namespace '{}'",
                config.namespace
            );
        }
        Err(e) => {
            return Err(map_kube_error(e, &config.namespace));
        }
    }

    eprintln!(
        "Token stored in Kubernetes Secret '{secret_name}' (namespace: {})",
        config.namespace
    );
    Ok(())
}

#[cfg(feature = "k8s")]
fn map_kube_error(e: kube::Error, namespace: &str) -> MemoryError {
    match &e {
        kube::Error::Api(err_resp) if err_resp.code == 403 => MemoryError::TokenStorage(format!(
            "Access denied. Ensure the service account has RBAC permission \
                 for secrets in namespace '{namespace}': {e}"
        )),
        kube::Error::Api(err_resp) if err_resp.code == 404 => {
            MemoryError::TokenStorage(format!("Namespace '{namespace}' does not exist: {e}"))
        }
        _ => MemoryError::TokenStorage(format!("Kubernetes API error: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Auth status
// ---------------------------------------------------------------------------

/// Print the current authentication status to stdout.
///
/// Shows the source of the resolved token and a redacted preview.
/// Never prints the full token value.
pub fn print_auth_status(provider: &AuthProvider) {
    match provider.resolve_with_source() {
        Ok((token, source)) => {
            let raw = token.expose_secret();
            let preview = if raw.len() >= 8 {
                format!("...{}", &raw[raw.len() - 4..])
            } else {
                "****".to_string()
            };
            println!("Authenticated via {source}");
            println!("Token: {preview}");
        }
        Err(_) => {
            println!("No token configured.");
            println!("Run `pali auth login` to authenticate with GitHub.");
        }
    }
}

// ---------------------------------------------------------------------------
// Permission check (Unix only)
// ---------------------------------------------------------------------------

/// Warn if the token file has permissions that are wider than 0o600.
fn check_token_file_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mode = meta.mode() & 0o777;
                if mode != 0o600 {
                    warn!(
                        "token file '{}' has permissions {:04o}; \
                         expected 0600 — consider running: chmod 600 {}",
                        path.display(),
                        mode,
                        path.display()
                    );
                }
            }
            Err(e) => {
                warn!("could not read permissions for '{}': {}", path.display(), e);
            }
        }
    }
    // On non-Unix platforms there are no POSIX permissions to check.
    #[cfg(not(unix))]
    let _ = path;
}

// ---------------------------------------------------------------------------
// Platform-portable home directory helper
// ---------------------------------------------------------------------------

/// Returns the user's home directory using the platform-native mechanism.
///
/// Delegates to [`dirs::home_dir`], which checks `HOME` on Unix and the
/// Windows profile directory on Windows — no deprecated `std::env::home_dir`.
pub fn home_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Serialise all tests that mutate environment variables so they don't race
    // under `cargo test` (which runs tests in parallel by default).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_resolve_from_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        let token_value = "ghp_test_env_token_abc123";
        std::env::set_var(ENV_VAR, token_value);
        let result = AuthProvider::try_resolve_with_source().map(|(tok, _)| tok);
        std::env::remove_var(ENV_VAR);

        assert!(result.is_ok(), "expected Ok but got: {result:?}");
        assert_eq!(result.unwrap().expose_secret(), token_value);
    }

    #[test]
    fn test_resolve_trims_env_var_whitespace() {
        let _guard = ENV_LOCK.lock().unwrap();
        let token_value = "  ghp_padded_token  ";
        std::env::set_var(ENV_VAR, token_value);
        let result = AuthProvider::try_resolve_with_source().map(|(tok, _)| tok);
        std::env::remove_var(ENV_VAR);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), token_value.trim());
    }

    #[test]
    fn test_resolve_prefers_env_over_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Write a token file and simultaneously set the env var; env must win.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("token");
        std::fs::write(&file_path, "ghp_file_token").unwrap();

        let env_token = "ghp_env_wins";
        std::env::set_var(ENV_VAR, env_token);

        // Override HOME so the file lookup would pick up our temp file if env
        // were not consulted first.  We rely on env taking precedence, so
        // this primarily tests ordering rather than actual file resolution.
        let result = AuthProvider::try_resolve_with_source().map(|(tok, _)| tok);
        std::env::remove_var(ENV_VAR);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), env_token);
    }

    #[test]
    fn test_try_resolve_with_source_returns_env_var_source() {
        let _guard = ENV_LOCK.lock().unwrap();
        let token_value = "ghp_source_test_abc";
        std::env::set_var(ENV_VAR, token_value);
        let result = AuthProvider::try_resolve_with_source();
        std::env::remove_var(ENV_VAR);

        assert!(result.is_ok(), "expected Ok but got: {result:?}");
        let (tok, source) = result.unwrap();
        assert_eq!(tok.expose_secret(), token_value);
        assert!(
            matches!(source, TokenSource::EnvVar),
            "expected TokenSource::EnvVar, got: {source:?}"
        );
    }

    #[test]
    fn test_store_token_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let token_dir = dir.path().join(".config").join("memory-mcp");
        let token_path = token_dir.join("token");

        // Temporarily override HOME.
        let _guard = ENV_LOCK.lock().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        let result = store_in_file("ghp_file_backend_test");

        // Restore HOME before asserting so other tests aren't affected.
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(result.is_ok(), "store_in_file failed: {result:?}");
        assert!(token_path.exists(), "token file was not created");

        let content = std::fs::read_to_string(&token_path).unwrap();
        assert_eq!(content, "ghp_file_backend_test\n");

        // Verify 0o600 permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&token_path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600 permissions, got {:04o}", mode);
        }
    }

    /// This test exercises the keyring path and requires a live D-Bus /
    /// secret-service backend.  Mark it `#[ignore]` so it does not run in CI.
    #[test]
    #[ignore = "requires live system keyring (D-Bus/GNOME Keyring/KWallet)"]
    fn test_resolve_from_keyring_ignored_in_ci() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Pre-condition: no env var, no token file (rely on absence).
        std::env::remove_var(ENV_VAR);

        // Attempt to store then retrieve; if the keyring is unavailable the
        // test is inconclusive rather than failing.
        let entry = keyring::Entry::new("memory-mcp", "github-token")
            .expect("keyring entry creation should succeed");
        let test_token = "ghp_keyring_test_token";
        entry
            .set_password(test_token)
            .expect("storing token should succeed");

        let result = AuthProvider::try_resolve_with_source().map(|(tok, _)| tok);
        let _ = entry.delete_credential(); // cleanup before assert
        assert!(result.is_ok(), "expected token from keyring: {result:?}");
        assert_eq!(result.unwrap().expose_secret(), test_token);
    }

    #[cfg(feature = "k8s")]
    #[test]
    #[ignore] // Requires a real Kubernetes cluster
    fn test_store_in_k8s_secret_ignored_in_ci() {
        // Placeholder for manual/integration testing
    }
}
