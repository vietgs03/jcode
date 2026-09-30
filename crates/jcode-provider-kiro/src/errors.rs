//! Classification of Kiro API HTTP failures.
//!
//! Error messages deliberately include the phrases jcode's generic recovery
//! logic keys on ("input is too long", "rate limit", "quota") so context
//! overflows trigger compaction and quota errors are not retried blindly.

/// Coarse category of a failed `GenerateAssistantResponse` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorKind {
    /// 401/403: token expired, revoked, or not entitled.
    Auth,
    /// 429 / `ThrottlingException`.
    Throttled,
    /// `INSUFFICIENT_MODEL_CAPACITY`: transient server-side capacity shortage.
    Capacity,
    /// Monthly request or credit quota exhausted.
    QuotaExhausted,
    /// Conversation is too large for the model context.
    ContextTooLong,
    /// Model id not available for this account or region.
    InvalidModel,
    /// 5xx.
    Server,
    /// Any other client error.
    BadRequest,
}

impl HttpErrorKind {
    /// Whether retrying the same request after a backoff can succeed.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Throttled | Self::Capacity | Self::Server)
    }
}

/// Classify an HTTP status plus response body.
pub fn classify_http_error(status: u16, body: &str) -> HttpErrorKind {
    let lower = body.to_ascii_lowercase();
    if lower.contains("monthly_request_count")
        || lower.contains("servicequotaexceeded")
        || lower.contains("quota exceeded")
    {
        HttpErrorKind::QuotaExhausted
    } else if lower.contains("insufficient_model_capacity") {
        HttpErrorKind::Capacity
    } else if status == 413
        || lower.contains("content_length_exceeds_threshold")
        || lower.contains("input is too long")
        || lower.contains("context window")
    {
        HttpErrorKind::ContextTooLong
    } else if lower.contains("invalid_model_id") || lower.contains("invalid model") {
        HttpErrorKind::InvalidModel
    } else if status == 401 || status == 403 {
        HttpErrorKind::Auth
    } else if status == 429
        || lower.contains("throttlingexception")
        || lower.contains("too many requests")
    {
        HttpErrorKind::Throttled
    } else if status >= 500 {
        HttpErrorKind::Server
    } else {
        HttpErrorKind::BadRequest
    }
}

/// Human-readable error message for a failed call.
pub fn describe_http_error(status: u16, body: &str) -> String {
    let body = truncate_chars(body.trim(), 2_000);
    let hint = match classify_http_error(status, &body) {
        HttpErrorKind::Auth => {
            "authentication was rejected; run `jcode login --provider kiro` to sign in again"
        }
        HttpErrorKind::Throttled => "rate limit exceeded (throttled by Kiro)",
        HttpErrorKind::Capacity => "model capacity temporarily unavailable",
        HttpErrorKind::QuotaExhausted => "Kiro usage quota exhausted for this account",
        HttpErrorKind::ContextTooLong => "input is too long for the model context window",
        HttpErrorKind::InvalidModel => {
            "model is not available for this Kiro account or region; choose another with /model"
        }
        HttpErrorKind::Server | HttpErrorKind::BadRequest => "",
    };
    match (hint.is_empty(), body.is_empty()) {
        (true, true) => format!("Kiro API error (HTTP {status})"),
        (true, false) => format!("Kiro API error (HTTP {status}): {body}"),
        (false, true) => format!("Kiro API error (HTTP {status}): {hint}"),
        (false, false) => format!("Kiro API error (HTTP {status}): {hint}: {body}"),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(limit).collect();
        truncated.push('…');
        truncated
    }
}
