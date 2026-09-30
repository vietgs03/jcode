//! Decoded event stream messages -> jcode [`StreamEvent`]s.
//!
//! `GenerateAssistantResponse` streams one JSON payload per event. The event
//! kind comes from the `:event-type` header:
//!
//! - `assistantResponseEvent` `{content}`: assistant text delta;
//! - `reasoningContentEvent` `{text, signature?}`: reasoning delta;
//! - `toolUseEvent` `{toolUseId, name, input?, stop?}`: tool call fragments,
//!   grouped by `toolUseId` until `stop` is set;
//! - `contextUsageEvent` / `metadataEvent` `{contextUsagePercentage}`;
//! - metering, follow-up prompt, citation and metadata events (ignored).
//!
//! Exceptions arrive as messages with `:message-type` `exception`.

use crate::eventstream::Message;
use jcode_message_types::StreamEvent;
use serde_json::Value;
use std::collections::HashSet;

/// A failure reported by the service inside the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFailure {
    /// Exception or error type, e.g. `ThrottlingException`.
    pub kind: String,
    pub message: String,
}

impl StreamFailure {
    /// Whether retrying the same request can reasonably succeed.
    pub fn is_retryable(&self) -> bool {
        let kind = self.kind.to_ascii_lowercase();
        let message = self.message.to_ascii_lowercase();
        kind.contains("throttling")
            || kind.contains("internalserver")
            || kind.contains("serviceunavailable")
            || message.contains("insufficient_model_capacity")
            || message.contains("too many requests")
    }
}

impl std::fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for StreamFailure {}

/// Stateful translator for one response stream.
#[derive(Debug)]
pub struct StreamTranslator {
    context_window: usize,
    open_tool: Option<String>,
    closed_tools: HashSet<String>,
    tool_calls: usize,
    reasoning_open: bool,
    output_chars: usize,
    context_usage_percentage: Option<f64>,
    finished: bool,
}

impl StreamTranslator {
    /// `context_window` converts `contextUsagePercentage` into token counts.
    pub fn new(context_window: usize) -> Self {
        Self {
            context_window,
            open_tool: None,
            closed_tools: HashSet::new(),
            tool_calls: 0,
            reasoning_open: false,
            output_chars: 0,
            context_usage_percentage: None,
            finished: false,
        }
    }

    /// Number of completed tool calls so far.
    pub fn tool_calls(&self) -> usize {
        self.tool_calls
    }

    /// Whether any text, reasoning, or tool call has been emitted.
    pub fn has_output(&self) -> bool {
        self.output_chars > 0 || self.tool_calls > 0 || self.open_tool.is_some()
    }

    /// Translate one decoded message.
    pub fn handle(&mut self, message: &Message) -> Result<Vec<StreamEvent>, StreamFailure> {
        match message.message_type() {
            Some("exception") => {
                return Err(failure_from_payload(
                    message.exception_type().unwrap_or("Exception"),
                    message,
                ));
            }
            Some("error") => {
                return Err(StreamFailure {
                    kind: message
                        .header_str(":error-code")
                        .unwrap_or("Error")
                        .to_string(),
                    message: message
                        .header_str(":error-message")
                        .map(str::to_string)
                        .unwrap_or_else(|| message.payload_text()),
                });
            }
            _ => {}
        }

        // Every event we understand carries a JSON payload; frames with any
        // other payload (empty keep-alives, future binary events) carry
        // nothing to translate.
        let payload = match message.payload_json() {
            Ok(payload) => payload,
            Err(_) => return Ok(Vec::new()),
        };
        let event_type = message
            .event_type()
            .map(str::to_string)
            .unwrap_or_else(|| infer_event_type(&payload).to_string());
        if event_type.ends_with("Exception") {
            return Err(failure_from_payload(&event_type, message));
        }

        if let Some(percentage) = payload
            .get("contextUsagePercentage")
            .and_then(Value::as_f64)
        {
            self.context_usage_percentage = Some(percentage);
        }

        let mut events = Vec::new();
        match event_type.as_str() {
            "assistantResponseEvent" => {
                if let Some(content) = payload.get("content").and_then(Value::as_str)
                    && !content.is_empty()
                {
                    self.close_reasoning(&mut events);
                    self.close_tool(&mut events);
                    self.output_chars += content.chars().count();
                    events.push(StreamEvent::TextDelta(content.to_string()));
                }
            }
            "reasoningContentEvent" => {
                if let Some(text) = payload.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    self.close_tool(&mut events);
                    if !self.reasoning_open {
                        self.reasoning_open = true;
                        events.push(StreamEvent::ThinkingStart);
                    }
                    self.output_chars += text.chars().count();
                    events.push(StreamEvent::ThinkingDelta(text.to_string()));
                }
            }
            "toolUseEvent" => self.handle_tool_use(&payload, &mut events),
            "invalidStateEvent" => {
                return Err(StreamFailure {
                    kind: "InvalidStateEvent".to_string(),
                    message: payload
                        .get("message")
                        .or_else(|| payload.get("reason"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| payload.to_string()),
                });
            }
            _ => {}
        }
        Ok(events)
    }

    /// Close any open blocks and emit usage plus the final `MessageEnd`.
    /// Calling it more than once returns no further events.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.finished {
            return events;
        }
        self.finished = true;
        self.close_reasoning(&mut events);
        self.close_tool(&mut events);

        let input_tokens = self
            .context_usage_percentage
            .filter(|percentage| percentage.is_finite() && *percentage > 0.0)
            .map(|percentage| ((percentage / 100.0) * self.context_window as f64).round() as u64);
        let output_tokens = (self.output_chars > 0).then(|| (self.output_chars as u64).div_ceil(4));
        if input_tokens.is_some() || output_tokens.is_some() {
            events.push(StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            });
        }

        let stop_reason = if self.tool_calls > 0 {
            "tool_use"
        } else {
            "end_turn"
        };
        events.push(StreamEvent::MessageEnd {
            stop_reason: Some(stop_reason.to_string()),
        });
        events
    }

    fn handle_tool_use(&mut self, payload: &Value, events: &mut Vec<StreamEvent>) {
        let Some(id) = payload
            .get("toolUseId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        // The service occasionally repeats events for a tool call it already
        // stopped; replaying them would duplicate the call.
        if self.closed_tools.contains(id) {
            return;
        }
        self.close_reasoning(events);
        if self.open_tool.as_deref() != Some(id) {
            self.close_tool(events);
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
            events.push(StreamEvent::ToolUseStart {
                id: id.to_string(),
                name: name.to_string(),
            });
            self.open_tool = Some(id.to_string());
        }

        let fragment = match payload.get("input") {
            Some(Value::String(fragment)) if !fragment.is_empty() => Some(fragment.clone()),
            Some(Value::Object(map)) if !map.is_empty() => {
                Some(Value::Object(map.clone()).to_string())
            }
            _ => None,
        };
        if let Some(fragment) = fragment {
            self.output_chars += fragment.chars().count();
            events.push(StreamEvent::ToolInputDelta(fragment));
        }

        if payload.get("stop").and_then(Value::as_bool) == Some(true) {
            self.close_tool(events);
        }
    }

    fn close_tool(&mut self, events: &mut Vec<StreamEvent>) {
        if let Some(id) = self.open_tool.take() {
            events.push(StreamEvent::ToolUseEnd);
            self.tool_calls += 1;
            self.closed_tools.insert(id);
        }
    }

    fn close_reasoning(&mut self, events: &mut Vec<StreamEvent>) {
        if self.reasoning_open {
            self.reasoning_open = false;
            events.push(StreamEvent::ThinkingEnd);
        }
    }
}

fn failure_from_payload(kind: &str, message: &Message) -> StreamFailure {
    let detail = match message.payload_json() {
        Ok(payload) => payload
            .get("message")
            .or_else(|| payload.get("Message"))
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(_) => None,
    }
    .unwrap_or_else(|| message.payload_text());
    StreamFailure {
        kind: kind.to_string(),
        message: detail,
    }
}

fn infer_event_type(payload: &Value) -> &'static str {
    let has = |key: &str| payload.get(key).is_some();
    if has("toolUseId") {
        "toolUseEvent"
    } else if has("content") {
        "assistantResponseEvent"
    } else if has("text") || has("signature") || has("redactedContent") {
        "reasoningContentEvent"
    } else {
        "unknown"
    }
}
