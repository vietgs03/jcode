//! Kiro authentication.
//!
//! Kiro accepts bearer tokens issued by AWS IAM Identity Center (SSO-OIDC):
//!
//! - **AWS Builder ID** (personal accounts): the OAuth device flow against the
//!   fixed Builder ID start URL in `us-east-1`.
//! - **IAM Identity Center** (organizations): the same device flow against the
//!   organization's start URL and region.
//! - **Kiro IDE import**: the tokens the Kiro IDE stores in
//!   `~/.aws/sso/cache/kiro-auth-token.json` (this covers the IDE's Google /
//!   GitHub social sign-in, which has no public device flow).
//!
//! jcode keeps its own copy in `~/.jcode/kiro_oauth.json` and refreshes it
//! independently. The device flow registers a dedicated OIDC client for
//! jcode, so it never shares a session with the Kiro IDE. An imported login
//! shares its refresh token with the IDE, so the IDE may later need to sign
//! in again; the device flow is the recommended path.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::AuthState;

pub const PROVIDER_ID: &str = "kiro";
pub const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";
pub const DEFAULT_REGION: &str = "us-east-1";
/// IAM Identity Center start URL (switches login from Builder ID to IdC).
pub const START_URL_ENV: &str = "JCODE_KIRO_START_URL";
/// IAM Identity Center region for the device flow.
pub const REGION_ENV: &str = "JCODE_KIRO_REGION";
/// Override for the Kiro API region (`us-east-1` or `eu-central-1`).
pub const API_REGION_ENV: &str = "JCODE_KIRO_API_REGION";
/// Override for the Kiro API base URL.
pub const API_BASE_ENV: &str = "JCODE_KIRO_API_BASE";
/// CodeWhisperer profile ARN (required by some IAM Identity Center setups).
pub const PROFILE_ARN_ENV: &str = "JCODE_KIRO_PROFILE_ARN";
/// Kiro IDE token file, relative to the home directory.
pub const KIRO_IDE_TOKEN_PATH: &str = ".aws/sso/cache/kiro-auth-token.json";

const SSO_CACHE_DIR: &str = ".aws/sso/cache";
const OIDC_CLIENT_NAME: &str = "jcode";
const OIDC_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const KIRO_AUTH_USER_AGENT: &str = "aws-sdk-js/3.738.0 KiroIDE jcode";
/// Refresh this long before the access token expires.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECS: i64 = 3600;

/// How a Kiro login was obtained; decides the refresh endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KiroAuthMethod {
    BuilderId,
    IdentityCenter,
    /// Kiro IDE social sign-in (Google/GitHub), refreshed via the Kiro auth service.
    Social,
}

impl KiroAuthMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuilderId => "AWS Builder ID",
            Self::IdentityCenter => "AWS IAM Identity Center",
            Self::Social => "Kiro social sign-in",
        }
    }
}

/// Persisted Kiro credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Access-token expiry (Unix epoch milliseconds).
    pub expires_at: i64,
    pub auth_method: KiroAuthMethod,
    /// Region of the OIDC client / Kiro auth service that issued the tokens.
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// OIDC client registration expiry (Unix epoch milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// Source file when the login was imported from the Kiro IDE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
}

impl KiroTokens {
    pub fn is_expired(&self) -> bool {
        self.expires_at <= now_ms() + 60_000
    }

    pub fn needs_refresh(&self) -> bool {
        self.expires_at <= now_ms() + REFRESH_MARGIN_MS
    }

    /// Profile ARN sent with requests (`JCODE_KIRO_PROFILE_ARN` wins).
    pub fn effective_profile_arn(&self) -> Option<String> {
        env_nonempty(PROFILE_ARN_ENV).or_else(|| self.profile_arn.clone())
    }

    /// Kiro API region: override, then the profile ARN's region, then the
    /// API region serving the login region.
    pub fn api_region(&self) -> String {
        if let Some(region) = env_nonempty(API_REGION_ENV) {
            return region;
        }
        if let Some(region) = self
            .effective_profile_arn()
            .as_deref()
            .and_then(region_from_arn)
        {
            return region;
        }
        api_region_for_login_region(&self.region)
    }

    /// Kiro API base URL (no trailing slash).
    pub fn api_base(&self) -> String {
        match env_nonempty(API_BASE_ENV) {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => format!("https://q.{}.amazonaws.com", self.api_region()),
        }
    }

    /// Short description of the login for status output.
    pub fn describe(&self) -> String {
        let mut detail = self.auth_method.label().to_string();
        if self.auth_method == KiroAuthMethod::IdentityCenter
            && let Some(start_url) = self.start_url.as_deref()
        {
            detail.push_str(&format!(" ({start_url})"));
        }
        if self.imported_from.is_some() {
            detail.push_str(", imported from Kiro IDE");
        }
        detail
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

fn json_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Region component of an ARN (`arn:aws:codewhisperer:<region>:...`).
pub fn region_from_arn(arn: &str) -> Option<String> {
    let parts: Vec<&str> = arn.trim().split(':').collect();
    if parts.len() >= 6 && parts[0] == "arn" && !parts[3].is_empty() {
        Some(parts[3].to_string())
    } else {
        None
    }
}

/// The Kiro API is only deployed in a few regions; map a login region onto
/// the one that serves it.
pub fn api_region_for_login_region(region: &str) -> String {
    if region.trim().starts_with("eu-") {
        "eu-central-1".to_string()
    } else {
        DEFAULT_REGION.to_string()
    }
}

// --- storage -----------------------------------------------------------------

pub fn tokens_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("kiro_oauth.json"))
}

pub fn load_tokens() -> Result<KiroTokens> {
    let path = tokens_path()?;
    if !path.exists() {
        anyhow::bail!("No Kiro login found. Run `jcode login --provider kiro`.");
    }
    crate::storage::harden_secret_file_permissions(&path);
    crate::storage::read_json(&path).with_context(|| format!("Failed to read {}", path.display()))
}

pub fn save_tokens(tokens: &KiroTokens) -> Result<()> {
    let path = tokens_path()?;
    crate::storage::write_json_secret(&path, tokens)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    super::AuthStatus::invalidate_cache();
    Ok(())
}

pub fn clear_tokens() -> Result<()> {
    let path = tokens_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    super::AuthStatus::invalidate_cache();
    Ok(())
}

pub fn has_credentials() -> bool {
    load_tokens().is_ok()
}

/// Auth state for status surfaces. A login with a refresh token counts as
/// available even when the short-lived access token has expired.
pub fn auth_state() -> AuthState {
    match load_tokens() {
        Ok(tokens) if !tokens.refresh_token.trim().is_empty() || !tokens.is_expired() => {
            AuthState::Available
        }
        Ok(_) => AuthState::Expired,
        Err(_) => AuthState::NotConfigured,
    }
}

// --- device flow ---------------------------------------------------------------

/// Where a device-flow login signs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KiroLoginTarget {
    pub auth_method: KiroAuthMethod,
    pub start_url: String,
    pub region: String,
}

impl KiroLoginTarget {
    pub fn builder_id() -> Self {
        Self {
            auth_method: KiroAuthMethod::BuilderId,
            start_url: BUILDER_ID_START_URL.to_string(),
            region: DEFAULT_REGION.to_string(),
        }
    }

    pub fn identity_center(start_url: &str, region: Option<&str>) -> Result<Self> {
        let start_url = start_url.trim().trim_end_matches('/');
        if !start_url.starts_with("https://") {
            anyhow::bail!(
                "IAM Identity Center start URL must start with https:// (for example https://my-org.awsapps.com/start)"
            );
        }
        if start_url == BUILDER_ID_START_URL {
            return Ok(Self::builder_id());
        }
        let region = region
            .map(str::trim)
            .filter(|region| !region.is_empty())
            .unwrap_or(DEFAULT_REGION);
        Ok(Self {
            auth_method: KiroAuthMethod::IdentityCenter,
            start_url: start_url.to_string(),
            region: region.to_string(),
        })
    }

    /// Builder ID unless `JCODE_KIRO_START_URL` selects an IAM Identity Center.
    pub fn from_env() -> Result<Self> {
        match env_nonempty(START_URL_ENV) {
            Some(start_url) => {
                Self::identity_center(&start_url, env_nonempty(REGION_ENV).as_deref())
            }
            None => Ok(Self::builder_id()),
        }
    }
}

/// A started device authorization, waiting for the user to approve it.
#[derive(Debug, Clone)]
pub struct KiroDeviceAuthorization {
    pub target: KiroLoginTarget,
    pub client_id: String,
    pub client_secret: String,
    pub client_secret_expires_at: Option<i64>,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

impl KiroDeviceAuthorization {
    /// URL to open in the browser (pre-filled with the user code when possible).
    pub fn browser_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

fn oidc_endpoint(region: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com")
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    action: &str,
) -> Result<Value> {
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("Failed to {action}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = crate::util::http_error_body(response, "HTTP error").await;
        anyhow::bail!("Failed to {action} (HTTP {status}): {body}");
    }
    response
        .json::<Value>()
        .await
        .with_context(|| format!("Failed to parse the response while trying to {action}"))
}

/// Register an OIDC client for jcode and start the device authorization.
pub async fn start_device_authorization(
    client: &reqwest::Client,
    target: &KiroLoginTarget,
) -> Result<KiroDeviceAuthorization> {
    let endpoint = oidc_endpoint(&target.region);
    let registration = post_json(
        client,
        &format!("{endpoint}/client/register"),
        &json!({
            "clientName": OIDC_CLIENT_NAME,
            "clientType": "public",
            "scopes": OIDC_SCOPES,
            "grantTypes": [DEVICE_CODE_GRANT, "refresh_token"],
        }),
        "register a Kiro sign-in client with AWS SSO-OIDC",
    )
    .await?;
    let client_id = json_str(&registration, "clientId")
        .context("AWS SSO-OIDC client registration did not return a clientId")?;
    let client_secret = json_str(&registration, "clientSecret")
        .context("AWS SSO-OIDC client registration did not return a clientSecret")?;
    let client_secret_expires_at = registration
        .get("clientSecretExpiresAt")
        .and_then(Value::as_i64)
        .map(|secs| secs * 1000);

    let device = post_json(
        client,
        &format!("{endpoint}/device_authorization"),
        &json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "startUrl": target.start_url,
        }),
        "start the AWS device authorization",
    )
    .await?;

    Ok(KiroDeviceAuthorization {
        target: target.clone(),
        client_id,
        client_secret,
        client_secret_expires_at,
        device_code: json_str(&device, "deviceCode")
            .context("AWS device authorization did not return a deviceCode")?,
        user_code: json_str(&device, "userCode")
            .context("AWS device authorization did not return a userCode")?,
        verification_uri: json_str(&device, "verificationUri")
            .context("AWS device authorization did not return a verificationUri")?,
        verification_uri_complete: json_str(&device, "verificationUriComplete"),
        interval_secs: device
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 30),
        expires_in_secs: device
            .get("expiresIn")
            .and_then(Value::as_u64)
            .unwrap_or(600)
            .max(60),
    })
}

/// Normalize an OIDC error (`error` field or `__type` exception name).
fn oidc_error_code(payload: &Value) -> String {
    if let Some(error) = json_str(payload, "error") {
        return error.to_ascii_lowercase();
    }
    let exception = json_str(payload, "__type").unwrap_or_else(String::new);
    let exception = exception.rsplit('#').next().unwrap_or("");
    match exception {
        "AuthorizationPendingException" => "authorization_pending".to_string(),
        "SlowDownException" => "slow_down".to_string(),
        "ExpiredTokenException" => "expired_token".to_string(),
        "AccessDeniedException" => "access_denied".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Poll until the user approves (or rejects) the device authorization.
pub async fn poll_device_token(
    client: &reqwest::Client,
    authorization: &KiroDeviceAuthorization,
) -> Result<KiroTokens> {
    let endpoint = oidc_endpoint(&authorization.target.region);
    let deadline = Instant::now() + Duration::from_secs(authorization.expires_in_secs);
    let mut interval = Duration::from_secs(authorization.interval_secs);

    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "The Kiro device code expired before it was approved. Run `jcode login --provider kiro` again."
            );
        }
        tokio::time::sleep(interval).await;

        let response = match client
            .post(format!("{endpoint}/token"))
            .json(&json!({
                "clientId": authorization.client_id,
                "clientSecret": authorization.client_secret,
                "deviceCode": authorization.device_code,
                "grantType": DEVICE_CODE_GRANT,
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                crate::logging::warn(&format!("Kiro token poll failed, retrying: {err}"));
                continue;
            }
        };
        let status = response.status();
        let text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                crate::logging::warn(&format!(
                    "Kiro token poll response unreadable, retrying: {err}"
                ));
                continue;
            }
        };
        if status.is_server_error() {
            continue;
        }
        let payload: Value = match serde_json::from_str(&text) {
            Ok(payload) => payload,
            Err(_) => anyhow::bail!(
                "Kiro device authorization failed (HTTP {status}): unexpected response: {}",
                text.chars().take(300).collect::<String>()
            ),
        };

        if status.is_success() {
            return tokens_from_device_grant(authorization, &payload);
        }

        match oidc_error_code(&payload).as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            "expired_token" => anyhow::bail!(
                "The Kiro device code expired before it was approved. Run `jcode login --provider kiro` again."
            ),
            "access_denied" => anyhow::bail!("Kiro sign-in was denied in the browser."),
            code => anyhow::bail!(
                "Kiro device authorization failed (HTTP {status}, {code}): {}",
                json_str(&payload, "error_description")
                    .or_else(|| json_str(&payload, "message"))
                    .unwrap_or_else(|| text.clone())
            ),
        }
    }
}

fn tokens_from_device_grant(
    authorization: &KiroDeviceAuthorization,
    payload: &Value,
) -> Result<KiroTokens> {
    let access_token =
        json_str(payload, "accessToken").context("AWS SSO-OIDC did not return an access token")?;
    let refresh_token =
        json_str(payload, "refreshToken").context("AWS SSO-OIDC did not return a refresh token")?;
    let expires_in = payload
        .get("expiresIn")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
    Ok(KiroTokens {
        access_token,
        refresh_token,
        expires_at: now_ms() + expires_in * 1000,
        auth_method: authorization.target.auth_method,
        region: authorization.target.region.clone(),
        client_id: Some(authorization.client_id.clone()),
        client_secret: Some(authorization.client_secret.clone()),
        client_secret_expires_at: authorization.client_secret_expires_at,
        start_url: Some(authorization.target.start_url.clone()),
        profile_arn: env_nonempty(PROFILE_ARN_ENV),
        imported_from: None,
    })
}

/// Best-effort lookup of the CodeWhisperer profile ARN for the signed-in
/// identity (IAM Identity Center accounts need it on every request).
pub async fn discover_profile_arn(client: &reqwest::Client, tokens: &KiroTokens) -> Option<String> {
    let mut bases = vec![tokens.api_base()];
    if env_nonempty(API_BASE_ENV).is_none() {
        for region in [DEFAULT_REGION, "eu-central-1"] {
            let base = format!("https://q.{region}.amazonaws.com");
            if !bases.contains(&base) {
                bases.push(base);
            }
        }
    }
    for base in bases {
        let response = client
            .post(format!("{base}/"))
            .header("Content-Type", "application/x-amz-json-1.0")
            .header(
                "X-Amz-Target",
                "AmazonCodeWhispererService.ListAvailableProfiles",
            )
            .header("Authorization", format!("Bearer {}", tokens.access_token))
            .body("{}")
            .send()
            .await;
        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                crate::logging::info(&format!(
                    "Kiro ListAvailableProfiles at {base} returned HTTP {}",
                    response.status()
                ));
                continue;
            }
            Err(err) => {
                crate::logging::info(&format!(
                    "Kiro ListAvailableProfiles at {base} failed: {err}"
                ));
                continue;
            }
        };
        let payload = match response.json::<Value>().await {
            Ok(payload) => payload,
            Err(err) => {
                crate::logging::info(&format!(
                    "Kiro ListAvailableProfiles at {base} returned unreadable JSON: {err}"
                ));
                continue;
            }
        };
        let arn = payload
            .get("profiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|profile| json_str(profile, "arn"));
        if arn.is_some() {
            return arn;
        }
    }
    None
}

// --- refresh -------------------------------------------------------------------

/// Refresh `observed`, serialized with any concurrent refresh. If another task
/// already stored newer tokens while we waited, those are returned instead.
pub async fn refresh_tokens(observed: &KiroTokens) -> Result<KiroTokens> {
    let observed_access_token = observed.access_token.clone();
    crate::auth::refresh_coordinator::single_flight(
        PROVIDER_ID.to_string(),
        || match load_tokens() {
            Ok(tokens) => Some(tokens),
            Err(_) => None,
        },
        move |stored: &KiroTokens| {
            !stored.needs_refresh() && stored.access_token != observed_access_token
        },
        {
            let observed = observed.clone();
            move |stored: Option<KiroTokens>| async move {
                let source = stored.unwrap_or(observed);
                refresh_tokens_uncoordinated(&source).await
            }
        },
    )
    .await
}

async fn refresh_tokens_uncoordinated(tokens: &KiroTokens) -> Result<KiroTokens> {
    let result: Result<KiroTokens> = async {
        if tokens.refresh_token.trim().is_empty() {
            anyhow::bail!(
                "The saved Kiro login has no refresh token. Run `jcode login --provider kiro`."
            );
        }
        let client = crate::provider::shared_http_client();
        let payload = match tokens.auth_method {
            KiroAuthMethod::Social => refresh_social(&client, tokens).await?,
            KiroAuthMethod::BuilderId | KiroAuthMethod::IdentityCenter => {
                refresh_oidc(&client, tokens).await?
            }
        };
        let refreshed = apply_refresh(tokens, &payload)?;
        save_tokens(&refreshed)?;
        Ok(refreshed)
    }
    .await;

    let record = match &result {
        Ok(_) => crate::auth::refresh_state::record_success(PROVIDER_ID),
        Err(err) => crate::auth::refresh_state::record_failure(PROVIDER_ID, err.to_string()),
    };
    if let Err(err) = record {
        crate::logging::warn(&format!("Failed to record Kiro refresh state: {err}"));
    }
    result
}

async fn refresh_oidc(client: &reqwest::Client, tokens: &KiroTokens) -> Result<Value> {
    let (Some(client_id), Some(client_secret)) =
        (tokens.client_id.as_deref(), tokens.client_secret.as_deref())
    else {
        anyhow::bail!(
            "The saved Kiro login is missing its OIDC client registration. Run `jcode login --provider kiro`."
        );
    };
    let response = client
        .post(format!("{}/token", oidc_endpoint(&tokens.region)))
        .json(&json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "refreshToken": tokens.refresh_token,
            "grantType": "refresh_token",
        }))
        .send()
        .await
        .context("Failed to reach AWS SSO-OIDC to refresh the Kiro login")?;
    refresh_response_json(response).await
}

async fn refresh_social(client: &reqwest::Client, tokens: &KiroTokens) -> Result<Value> {
    let response = client
        .post(format!(
            "https://prod.{}.auth.desktop.kiro.dev/refreshToken",
            tokens.region
        ))
        .header("User-Agent", KIRO_AUTH_USER_AGENT)
        .json(&json!({ "refreshToken": tokens.refresh_token }))
        .send()
        .await
        .context("Failed to reach the Kiro auth service to refresh the login")?;
    refresh_response_json(response).await
}

async fn refresh_response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let body = crate::util::http_error_body(response, "HTTP error").await;
        anyhow::bail!(
            "Kiro token refresh failed (HTTP {status}): {body}. Run `jcode login --provider kiro` to sign in again."
        );
    }
    response
        .json::<Value>()
        .await
        .context("Failed to parse the Kiro token refresh response")
}

fn apply_refresh(tokens: &KiroTokens, payload: &Value) -> Result<KiroTokens> {
    let access_token = json_str(payload, "accessToken")
        .or_else(|| json_str(payload, "access_token"))
        .context("Kiro token refresh response did not include an access token")?;
    let expires_in = payload
        .get("expiresIn")
        .or_else(|| payload.get("expires_in"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
    let mut refreshed = tokens.clone();
    refreshed.access_token = access_token;
    refreshed.expires_at = now_ms() + expires_in * 1000;
    if let Some(refresh_token) =
        json_str(payload, "refreshToken").or_else(|| json_str(payload, "refresh_token"))
    {
        refreshed.refresh_token = refresh_token;
    }
    if let Some(profile_arn) = json_str(payload, "profileArn") {
        refreshed.profile_arn = Some(profile_arn);
    }
    Ok(refreshed)
}

// --- Kiro IDE import -------------------------------------------------------------

pub fn kiro_ide_token_path() -> Result<PathBuf> {
    crate::storage::user_home_path(KIRO_IDE_TOKEN_PATH)
}

pub fn kiro_ide_token_exists() -> bool {
    kiro_ide_token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Copy the Kiro IDE login into jcode's own token store. The IDE file is only
/// read, never modified.
pub fn import_kiro_ide_tokens() -> Result<KiroTokens> {
    let path = kiro_ide_token_path()?;
    let safe_path = crate::storage::validate_external_auth_file(&path)?;
    let raw = std::fs::read_to_string(&safe_path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let data: Value = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let tokens = tokens_from_kiro_ide_json(&data, &path)?;
    save_tokens(&tokens)?;
    Ok(tokens)
}

fn tokens_from_kiro_ide_json(data: &Value, source: &Path) -> Result<KiroTokens> {
    let access_token = json_str(data, "accessToken")
        .with_context(|| format!("{} has no accessToken", source.display()))?;
    let refresh_token = json_str(data, "refreshToken")
        .with_context(|| format!("{} has no refreshToken", source.display()))?;
    let expires_at = data
        .get("expiresAt")
        .and_then(parse_expiry_ms)
        // Unknown expiry: treat as expired so the first request refreshes it.
        .unwrap_or_else(|| now_ms() - 1);

    let mut client_id = json_str(data, "clientId");
    let mut client_secret = json_str(data, "clientSecret");
    let mut client_secret_expires_at = None;
    if client_id.is_none()
        && let Some(client_id_hash) = json_str(data, "clientIdHash")
        && let Some(registration) = load_device_registration(&client_id_hash)?
    {
        client_id = json_str(&registration, "clientId");
        client_secret = json_str(&registration, "clientSecret");
        client_secret_expires_at = registration.get("expiresAt").and_then(parse_expiry_ms);
    }

    let auth_method_field = json_str(data, "authMethod").map(|value| value.to_ascii_lowercase());
    let provider_field = json_str(data, "provider").map(|value| value.to_ascii_lowercase());
    let has_client = client_id.is_some() && client_secret.is_some();
    let auth_method = if auth_method_field.as_deref() == Some("social") || !has_client {
        KiroAuthMethod::Social
    } else if provider_field.as_deref() == Some("builderid") {
        KiroAuthMethod::BuilderId
    } else {
        KiroAuthMethod::IdentityCenter
    };

    Ok(KiroTokens {
        access_token,
        refresh_token,
        expires_at,
        auth_method,
        region: json_str(data, "region").unwrap_or_else(|| DEFAULT_REGION.to_string()),
        client_id,
        client_secret,
        client_secret_expires_at,
        start_url: json_str(data, "startUrl"),
        profile_arn: json_str(data, "profileArn"),
        imported_from: Some(source.display().to_string()),
    })
}

/// Load `~/.aws/sso/cache/<clientIdHash>.json` (the IDE's OIDC client).
fn load_device_registration(client_id_hash: &str) -> Result<Option<Value>> {
    let is_safe_name = client_id_hash
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if client_id_hash.is_empty() || !is_safe_name {
        anyhow::bail!("Kiro IDE login references an invalid device registration id");
    }
    let path = crate::storage::user_home_path(format!("{SSO_CACHE_DIR}/{client_id_hash}.json"))?;
    if !path.exists() {
        return Ok(None);
    }
    let safe_path = crate::storage::validate_external_auth_file(&path)?;
    let raw = std::fs::read_to_string(&safe_path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let registration = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(registration))
}

/// Parse an RFC 3339 string or epoch seconds/milliseconds into epoch milliseconds.
fn parse_expiry_ms(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => match chrono::DateTime::parse_from_rfc3339(text.trim()) {
            Ok(parsed) => Some(parsed.timestamp_millis()),
            Err(_) => None,
        },
        Value::Number(number) => number.as_i64().map(|raw| {
            if raw < 10_000_000_000 {
                raw * 1000
            } else {
                raw
            }
        }),
        _ => None,
    }
}
