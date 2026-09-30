//! Kiro provider: the AWS CodeWhisperer streaming API
//! (`GenerateAssistantResponse`) authenticated with a Kiro login.
//!
//! Request shaping, event stream decoding and event translation live in the
//! I/O-free `jcode-provider-kiro` crate; this module owns HTTP, retries and
//! token lifecycle. Tokens come from [`crate::auth::kiro`] and are refreshed
//! before expiry, plus once more when the API rejects them.

use super::{EventStream, ModelRoute, Provider};
use crate::auth::kiro as kiro_auth;
use crate::message::{ConnectionPhase, Message as ChatMessage, StreamEvent, ToolDefinition};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use jcode_provider_kiro::errors::{HttpErrorKind, classify_http_error, describe_http_error};
use jcode_provider_kiro::eventstream::Decoder;
use jcode_provider_kiro::models;
use jcode_provider_kiro::request::{RequestParams, build_request};
use jcode_provider_kiro::stream::StreamTranslator;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// Default model override.
pub const MODEL_ENV: &str = "JCODE_KIRO_MODEL";
/// User-Agent override for API requests.
pub const USER_AGENT_ENV: &str = "JCODE_KIRO_USER_AGENT";

const DEFAULT_USER_AGENT: &str =
    "aws-sdk-js/3.738.0 ua/2.1 api/codewhispererstreaming#3.738.0 m/E KiroIDE jcode";
const AMZ_USER_AGENT: &str = "aws-sdk-js/3.738.0 KiroIDE jcode";
const GENERATE_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 1_000;
/// Longest silence tolerated mid-stream (large models may think for minutes).
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

type EventSender = mpsc::Sender<Result<StreamEvent>>;

#[derive(Clone)]
pub struct KiroProvider {
    client: reqwest::Client,
    model: Arc<RwLock<String>>,
    tokens: Arc<tokio::sync::Mutex<Option<kiro_auth::KiroTokens>>>,
    conversation_id: String,
}

struct StreamFailure {
    error: anyhow::Error,
    retryable: bool,
}

impl StreamFailure {
    fn retryable(error: anyhow::Error) -> Self {
        Self {
            error,
            retryable: true,
        }
    }

    fn fatal(error: anyhow::Error) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

impl Default for KiroProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroProvider {
    pub fn new() -> Self {
        Self {
            client: crate::provider::shared_http_client(),
            model: Arc::new(RwLock::new(configured_default_model())),
            tokens: Arc::new(tokio::sync::Mutex::new(None)),
            conversation_id: Uuid::new_v4().to_string(),
        }
    }

    /// Whether a Kiro login is stored locally.
    pub fn has_credentials() -> bool {
        kiro_auth::has_credentials()
    }

    fn current_model(&self) -> String {
        self.model
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn clear_cached_tokens(&self) {
        // A refresh holding the lock will store fresh tokens anyway.
        if let Ok(mut cached) = self.tokens.try_lock() {
            *cached = None;
        }
    }

    /// Current tokens, refreshed when close to expiry or when `force_refresh`.
    async fn tokens(&self, force_refresh: bool) -> Result<kiro_auth::KiroTokens> {
        let mut cached = self.tokens.lock().await;
        let current = match cached.take() {
            Some(tokens) => tokens,
            None => kiro_auth::load_tokens()?,
        };
        if !force_refresh && !current.needs_refresh() {
            *cached = Some(current.clone());
            return Ok(current);
        }
        match kiro_auth::refresh_tokens(&current).await {
            Ok(refreshed) => {
                *cached = Some(refreshed.clone());
                Ok(refreshed)
            }
            Err(err) => {
                *cached = Some(current);
                Err(err)
            }
        }
    }

    async fn stream_request(
        &self,
        body: String,
        model: String,
        context_window: usize,
        tx: EventSender,
    ) {
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refresh = false;
        let mut auth_retry_used = false;

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 && !force_refresh {
                let delay = crate::provider::attempt_tracker::retry_backoff_delay(
                    attempt,
                    RETRY_BASE_DELAY_MS,
                );
                crate::logging::info(&format!(
                    "Retrying Kiro request (attempt {}/{}) after {}ms",
                    attempt + 1,
                    MAX_ATTEMPTS,
                    delay.as_millis()
                ));
                let retrying = StreamEvent::ConnectionPhase {
                    phase: ConnectionPhase::Retrying {
                        attempt: attempt + 1,
                        max: MAX_ATTEMPTS,
                    },
                };
                if !emit(&tx, retrying).await {
                    return;
                }
                tokio::time::sleep(delay).await;
            }

            let tokens = match self.tokens(force_refresh).await {
                Ok(tokens) => tokens,
                Err(err) => {
                    emit_error(&tx, err).await;
                    return;
                }
            };
            force_refresh = false;

            // Retries use an unpooled client so a broken pooled connection
            // cannot fail the retry the same way.
            let client = if attempt == 0 {
                self.client.clone()
            } else {
                crate::provider::fresh_transport_client()
            };
            let response = client
                .post(format!("{}/generateAssistantResponse", tokens.api_base()))
                .header("Content-Type", "application/x-amz-json-1.0")
                .header("X-Amz-Target", GENERATE_TARGET)
                .header("Authorization", format!("Bearer {}", tokens.access_token))
                .header("User-Agent", user_agent())
                .header("x-amz-user-agent", AMZ_USER_AGENT)
                .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
                .header(
                    "amz-sdk-request",
                    format!("attempt={}; max={}", attempt + 1, MAX_ATTEMPTS),
                )
                .header("x-amzn-codewhisperer-optout", "true")
                .header("x-amzn-kiro-agent-mode", "vibe")
                .body(body.clone())
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    let error = anyhow!("Kiro API request failed: {err:#}");
                    if attempt + 1 < MAX_ATTEMPTS
                        && crate::provider::is_transient_transport_error(&format!("{err:#}"))
                    {
                        last_error = Some(error);
                        continue;
                    }
                    emit_error(&tx, error).await;
                    return;
                }
            };

            let status = response.status();
            if !status.is_success() {
                let body_text = crate::util::http_error_body(response, "HTTP error").await;
                let kind = classify_http_error(status.as_u16(), &body_text);
                let mut message = describe_http_error(status.as_u16(), &body_text);
                if kind == HttpErrorKind::Auth && !auth_retry_used {
                    auth_retry_used = true;
                    force_refresh = true;
                    crate::logging::info("Kiro rejected the access token; refreshing and retrying");
                    last_error = Some(anyhow!(message));
                    continue;
                }
                if kind.is_retryable() && attempt + 1 < MAX_ATTEMPTS {
                    crate::logging::info(&format!("Retryable Kiro error: {message}"));
                    last_error = Some(anyhow!(message));
                    continue;
                }
                if kind == HttpErrorKind::Auth
                    && tokens.auth_method == kiro_auth::KiroAuthMethod::IdentityCenter
                    && tokens.effective_profile_arn().is_none()
                {
                    message.push_str(&format!(
                        " (IAM Identity Center logins may need {} set to your Kiro profile ARN)",
                        kiro_auth::PROFILE_ARN_ENV
                    ));
                }
                emit_error(&tx, anyhow!(message)).await;
                return;
            }

            let is_json_response = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .is_some_and(|value| value.to_str().is_ok_and(|text| text.contains("json")));
            if is_json_response {
                let body_text = crate::util::http_error_body(response, "HTTP error").await;
                emit_error(
                    &tx,
                    anyhow!("Kiro API returned JSON instead of an event stream: {body_text}"),
                )
                .await;
                return;
            }

            let connection = StreamEvent::ConnectionType {
                connection: format!("kiro ({model})"),
            };
            if !emit(&tx, connection).await {
                return;
            }

            // Track whether this attempt streamed visible output so a
            // mid-stream failure can roll it back before the retry replays.
            let (attempt_tx, attempt_guard) =
                crate::provider::attempt_tracker::track_attempt_output(tx.clone());
            let outcome = process_event_stream(response, attempt_tx, context_window).await;
            let saw_output = attempt_guard.finish().await;
            match outcome {
                Ok(()) => return,
                Err(failure) if failure.retryable && attempt + 1 < MAX_ATTEMPTS => {
                    crate::logging::warn(&format!(
                        "Kiro stream failed (attempt {}/{}), retrying: {}",
                        attempt + 1,
                        MAX_ATTEMPTS,
                        failure.error
                    ));
                    if saw_output {
                        let rollback = StreamEvent::RetryRollback {
                            attempt: attempt + 2,
                            max: MAX_ATTEMPTS,
                        };
                        if !emit(&tx, rollback).await {
                            return;
                        }
                    }
                    last_error = Some(failure.error);
                }
                Err(failure) => {
                    emit_error(&tx, failure.error).await;
                    return;
                }
            }
        }

        if let Some(err) = last_error {
            emit_error(
                &tx,
                anyhow!("Kiro request failed after {MAX_ATTEMPTS} attempts: {err}"),
            )
            .await;
        }
    }
}

fn configured_default_model() -> String {
    match std::env::var(MODEL_ENV) {
        Ok(model) if !model.trim().is_empty() => models::normalize_model_id(&model),
        _ => models::DEFAULT_MODEL.to_string(),
    }
}

fn user_agent() -> String {
    match std::env::var(USER_AGENT_ENV) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_USER_AGENT.to_string(),
    }
}

/// Send one event; `false` means the consumer is gone and streaming can stop.
async fn emit(tx: &EventSender, event: StreamEvent) -> bool {
    tx.send(Ok(event)).await.is_ok()
}

async fn emit_error(tx: &EventSender, error: anyhow::Error) {
    if tx.send(Err(error)).await.is_err() {
        crate::logging::info("Kiro stream consumer closed before the error was delivered");
    }
}

async fn process_event_stream(
    response: reqwest::Response,
    tx: EventSender,
    context_window: usize,
) -> std::result::Result<(), StreamFailure> {
    use futures::StreamExt;

    let mut body = response.bytes_stream();
    let mut decoder = Decoder::new();
    let mut translator = StreamTranslator::new(context_window);

    loop {
        let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(err))) => {
                return Err(StreamFailure::retryable(anyhow!(
                    "Kiro stream error: {err:#}"
                )));
            }
            Ok(None) => break,
            Err(_) => {
                return Err(StreamFailure::retryable(anyhow!(
                    "Kiro stream read timeout: no data received for {} seconds",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )));
            }
        };
        decoder.push(&chunk);

        loop {
            let message = match decoder.next_message() {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(err) => {
                    return Err(StreamFailure::fatal(anyhow!(
                        "Kiro stream decode error: {err}"
                    )));
                }
            };
            let events = match translator.handle(&message) {
                Ok(events) => events,
                Err(failure) => {
                    let retryable = failure.is_retryable();
                    let error = anyhow!("Kiro stream error: {failure}");
                    return Err(StreamFailure { error, retryable });
                }
            };
            for event in events {
                if !emit(&tx, event).await {
                    return Ok(());
                }
            }
        }
    }

    if decoder.buffered_len() > 0 {
        crate::logging::warn(&format!(
            "Kiro stream ended with {} undecoded trailing bytes",
            decoder.buffered_len()
        ));
    }
    if !translator.has_output() {
        return Err(StreamFailure::retryable(anyhow!(
            "Kiro returned an empty response"
        )));
    }
    for event in translator.finish() {
        if !emit(&tx, event).await {
            return Ok(());
        }
    }
    Ok(())
}

#[async_trait]
impl Provider for KiroProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        // Resolve credentials up front so a missing/expired login fails the
        // call (and can trigger provider failover) instead of the stream.
        let tokens = self.tokens(false).await?;
        let model = self.current_model();
        let profile_arn = tokens.effective_profile_arn();
        let built = build_request(&RequestParams {
            messages,
            tools,
            system,
            model_id: &model,
            conversation_id: &self.conversation_id,
            profile_arn: profile_arn.as_deref(),
            allow_images: models::model_supports_images(&model),
        });
        if !built.skipped_tools.is_empty() {
            crate::logging::warn(&format!(
                "Kiro: skipped tools with names longer than 64 characters: {}",
                built.skipped_tools.join(", ")
            ));
        }
        crate::logging::info(&format!(
            "Kiro request: model={} history={} tools={} api_region={}",
            model,
            built.history_len,
            tools.len(),
            tokens.api_region()
        ));

        let context_window = models::context_window_for_model(&model);
        let body = built.body.to_string();
        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(100);
        let provider = self.clone();
        tokio::spawn(async move {
            provider
                .stream_request(body, model, context_window, tx)
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "kiro"
    }

    fn model(&self) -> String {
        self.current_model()
    }

    fn set_model(&self, model: &str) -> Result<()> {
        let normalized = models::normalize_model_id(model);
        if normalized.is_empty() {
            anyhow::bail!("Kiro model cannot be empty");
        }
        *self
            .model
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = normalized;
        Ok(())
    }

    fn available_models(&self) -> Vec<&'static str> {
        models::model_ids()
    }

    fn model_routes(&self) -> Vec<ModelRoute> {
        models::KIRO_MODELS
            .iter()
            .map(|model| ModelRoute {
                model: model.id.to_string(),
                provider: "Kiro".to_string(),
                api_method: "kiro".to_string(),
                available: true,
                detail: String::new(),
                cheapness: None,
            })
            .collect()
    }

    fn supports_image_input(&self) -> bool {
        models::model_supports_images(&self.current_model())
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn context_window(&self) -> usize {
        models::context_window_for_model(&self.current_model())
    }

    fn on_auth_changed(&self) {
        self.clear_cached_tokens();
    }

    async fn invalidate_credentials(&self) {
        *self.tokens.lock().await = None;
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            client: self.client.clone(),
            model: Arc::new(RwLock::new(self.current_model())),
            tokens: self.tokens.clone(),
            conversation_id: Uuid::new_v4().to_string(),
        })
    }
}
