//! jcode conversation -> `GenerateAssistantResponse` request body.
//!
//! The Kiro API models a conversation as strictly alternating
//! `userInputMessage` / `assistantResponseMessage` history entries plus one
//! `currentMessage`, and it validates the tool-calling structure strictly:
//!
//! - there is no separate system field, so the system prompt is prepended to
//!   the first user message;
//! - tool definitions ride on the current message only, and every tool name
//!   replayed in history must also be declared there;
//! - every `toolUses` entry must be answered by `toolResults` in the next user
//!   message, and every tool result must answer a tool use from the previous
//!   assistant message;
//! - requests without tool definitions must not contain tool content at all;
//! - message content must be non-empty.
//!
//! jcode stores each tool result as its own user message and may carry
//! orphaned tool content after compaction or an interrupted turn, so this
//! module normalizes the transcript into that shape. Tool content that cannot
//! be represented structurally is kept as plain text so context is not lost.

use jcode_message_types::{ContentBlock, Message, Role, ToolDefinition, sanitize_tool_id};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};

/// `userInputMessage.origin` value used by the Kiro IDE.
pub const ORIGIN: &str = "AI_EDITOR";
/// Placeholder for messages that would otherwise be empty.
pub const EMPTY_PLACEHOLDER: &str = "(empty placeholder)";
/// Placeholder for a user turn that only carries tool results.
pub const TOOL_RESULTS_PLACEHOLDER: &str = "Tool results provided.";
/// Placeholder for an empty tool result.
pub const EMPTY_TOOL_RESULT: &str = "(empty result)";
/// Tool results longer than this are middle-truncated.
pub const TOOL_RESULT_CHAR_LIMIT: usize = 250_000;
/// Longer tool descriptions are rejected by the API; they move to the system prompt.
pub const TOOL_DESCRIPTION_CHAR_LIMIT: usize = 10_000;
/// The API rejects tool names longer than this.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Inputs for [`build_request`].
#[derive(Debug, Clone, Copy)]
pub struct RequestParams<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub system: &'a str,
    /// Wire model id (see [`crate::models::normalize_model_id`]).
    pub model_id: &'a str,
    pub conversation_id: &'a str,
    pub profile_arn: Option<&'a str>,
    /// Whether the selected model accepts images in the current message.
    pub allow_images: bool,
}

/// A request body plus diagnostics about what had to be adapted.
#[derive(Debug, Clone)]
pub struct BuiltRequest {
    pub body: Value,
    /// Tool names that were not sent because they exceed [`MAX_TOOL_NAME_LEN`].
    pub skipped_tools: Vec<String>,
    /// Number of history entries in the request.
    pub history_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
struct ToolUse {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Clone)]
struct ToolResult {
    tool_use_id: String,
    content: String,
    is_error: bool,
}

#[derive(Debug, Clone)]
struct Image {
    format: String,
    data: String,
}

#[derive(Debug, Clone)]
struct Turn {
    role: TurnRole,
    text: String,
    images: Vec<Image>,
    tool_uses: Vec<ToolUse>,
    tool_results: Vec<ToolResult>,
}

impl Turn {
    fn new(role: TurnRole) -> Self {
        Self {
            role,
            text: String::new(),
            images: Vec::new(),
            tool_uses: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.images.is_empty()
            && self.tool_uses.is_empty()
            && self.tool_results.is_empty()
    }

    fn push_text(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
    }

    fn absorb(&mut self, other: Turn) {
        self.push_text(&other.text);
        self.images.extend(other.images);
        self.tool_uses.extend(other.tool_uses);
        self.tool_results.extend(other.tool_results);
    }

    fn content_or_placeholder(&self) -> String {
        if !self.text.trim().is_empty() {
            self.text.clone()
        } else if !self.tool_results.is_empty() {
            TOOL_RESULTS_PLACEHOLDER.to_string()
        } else {
            EMPTY_PLACEHOLDER.to_string()
        }
    }
}

/// Build the `GenerateAssistantResponse` JSON body.
pub fn build_request(params: &RequestParams<'_>) -> BuiltRequest {
    let tool_specs = build_tool_specs(params.tools);
    let tools_enabled = !tool_specs.specs.is_empty();

    let mut turns = collect_turns(params.messages);
    if !tools_enabled {
        for turn in &mut turns {
            flatten_tool_content(turn);
        }
    }
    let mut turns = merge_adjacent(turns);
    if tools_enabled {
        repair_tool_pairs(&mut turns);
    }
    if turns.first().map(|turn| turn.role) == Some(TurnRole::Assistant) {
        turns.insert(0, Turn::new(TurnRole::User));
    }

    let mut current = match turns.pop() {
        Some(turn) if turn.role == TurnRole::User => turn,
        Some(assistant) => {
            turns.push(assistant);
            Turn::new(TurnRole::User)
        }
        None => Turn::new(TurnRole::User),
    };

    let system_text = format!("{}{}", params.system.trim(), tool_specs.documentation);
    let system_text = system_text.trim();
    if !system_text.is_empty() {
        let target = turns
            .iter_mut()
            .find(|turn| turn.role == TurnRole::User)
            .unwrap_or(&mut current);
        target.text = if target.text.trim().is_empty() {
            system_text.to_string()
        } else {
            format!("{system_text}\n\n{}", target.text)
        };
    }

    let mut declared_tools = tool_specs.specs;
    if tools_enabled {
        let mut declared_names = tool_specs.names;
        for turn in &turns {
            for tool_use in &turn.tool_uses {
                if declared_names.insert(tool_use.name.clone()) {
                    declared_tools.push(placeholder_tool_spec(&tool_use.name));
                }
            }
        }
    }

    let history: Vec<Value> = turns
        .iter()
        .map(|turn| history_entry(turn, params.model_id))
        .collect();

    if !params.allow_images && !current.images.is_empty() {
        let omitted = current.images.len();
        current.images.clear();
        current.push_text(&format!(
            "[{omitted} image(s) omitted: the selected Kiro model does not accept images]"
        ));
    }

    let mut user_input = Map::new();
    user_input.insert(
        "content".to_string(),
        Value::String(current.content_or_placeholder()),
    );
    user_input.insert(
        "modelId".to_string(),
        Value::String(params.model_id.to_string()),
    );
    user_input.insert("origin".to_string(), Value::String(ORIGIN.to_string()));
    if !current.images.is_empty() {
        user_input.insert("images".to_string(), images_json(&current.images));
    }
    let mut context = Map::new();
    if !current.tool_results.is_empty() {
        context.insert(
            "toolResults".to_string(),
            tool_results_json(&current.tool_results),
        );
    }
    if tools_enabled {
        context.insert("tools".to_string(), Value::Array(declared_tools));
    }
    if !context.is_empty() {
        user_input.insert(
            "userInputMessageContext".to_string(),
            Value::Object(context),
        );
    }

    let mut conversation_state = Map::new();
    conversation_state.insert(
        "chatTriggerType".to_string(),
        Value::String("MANUAL".to_string()),
    );
    conversation_state.insert(
        "conversationId".to_string(),
        Value::String(params.conversation_id.to_string()),
    );
    conversation_state.insert(
        "currentMessage".to_string(),
        json!({ "userInputMessage": Value::Object(user_input) }),
    );
    let history_len = history.len();
    if !history.is_empty() {
        conversation_state.insert("history".to_string(), Value::Array(history));
    }

    let mut body = Map::new();
    body.insert(
        "conversationState".to_string(),
        Value::Object(conversation_state),
    );
    if let Some(profile_arn) = params
        .profile_arn
        .map(str::trim)
        .filter(|arn| !arn.is_empty())
    {
        body.insert(
            "profileArn".to_string(),
            Value::String(profile_arn.to_string()),
        );
    }

    BuiltRequest {
        body: Value::Object(body),
        skipped_tools: tool_specs.skipped,
        history_len,
    }
}

fn collect_turns(messages: &[Message]) -> Vec<Turn> {
    let mut turns = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            Role::User => TurnRole::User,
            Role::Assistant => TurnRole::Assistant,
        };
        let mut turn = Turn::new(role);
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => turn.push_text(text),
                ContentBlock::Image { media_type, data } if role == TurnRole::User => {
                    if let Some(image) = image_from_block(media_type, data) {
                        turn.images.push(image);
                    }
                }
                ContentBlock::Image { .. } => {}
                ContentBlock::ToolUse {
                    id, name, input, ..
                } if role == TurnRole::Assistant => turn.tool_uses.push(ToolUse {
                    id: sanitize_tool_id(id),
                    name: name.clone(),
                    input: normalize_tool_input(input),
                }),
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => turn.push_text(&tool_use_as_text(name, id, input)),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if role == TurnRole::User => turn.tool_results.push(ToolResult {
                    tool_use_id: sanitize_tool_id(tool_use_id),
                    content: content.clone(),
                    is_error: is_error.unwrap_or(false),
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => turn.push_text(&tool_result_as_text(tool_use_id, content)),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ReasoningTrace { .. }
                | ContentBlock::AnthropicThinking { .. }
                | ContentBlock::OpenAIReasoning { .. }
                | ContentBlock::OpenAICompaction { .. } => {}
            }
        }
        if !turn.is_empty() {
            turns.push(turn);
        }
    }
    turns
}

fn merge_adjacent(turns: Vec<Turn>) -> Vec<Turn> {
    let mut merged: Vec<Turn> = Vec::with_capacity(turns.len());
    for turn in turns {
        match merged.last_mut() {
            Some(last) if last.role == turn.role => last.absorb(turn),
            _ => merged.push(turn),
        }
    }
    merged
}

/// Convert structured tool content to text (used when no tools are declared).
fn flatten_tool_content(turn: &mut Turn) {
    let tool_uses = std::mem::take(&mut turn.tool_uses);
    for tool_use in tool_uses {
        turn.push_text(&tool_use_as_text(
            &tool_use.name,
            &tool_use.id,
            &tool_use.input,
        ));
    }
    let tool_results = std::mem::take(&mut turn.tool_results);
    for result in tool_results {
        turn.push_text(&tool_result_as_text(&result.tool_use_id, &result.content));
    }
}

/// Keep only tool uses/results that pair up across adjacent turns; anything
/// else is preserved as text. Expects merged (alternating) turns.
fn repair_tool_pairs(turns: &mut [Turn]) {
    for index in 0..turns.len() {
        if turns[index].role != TurnRole::User || turns[index].tool_results.is_empty() {
            continue;
        }
        let known: HashSet<String> = match index.checked_sub(1).map(|prev| &turns[prev]) {
            Some(prev) if prev.role == TurnRole::Assistant => prev
                .tool_uses
                .iter()
                .map(|tool_use| tool_use.id.clone())
                .collect(),
            _ => HashSet::new(),
        };
        let results = std::mem::take(&mut turns[index].tool_results);
        let mut answered = HashSet::new();
        let mut orphaned = Vec::new();
        for result in results {
            if known.contains(&result.tool_use_id) && answered.insert(result.tool_use_id.clone()) {
                turns[index].tool_results.push(result);
            } else {
                orphaned.push(tool_result_as_text(&result.tool_use_id, &result.content));
            }
        }
        for text in orphaned {
            turns[index].push_text(&text);
        }
    }

    for index in 0..turns.len() {
        if turns[index].role != TurnRole::Assistant || turns[index].tool_uses.is_empty() {
            continue;
        }
        let answered: HashSet<String> = match turns.get(index + 1) {
            Some(next) if next.role == TurnRole::User => next
                .tool_results
                .iter()
                .map(|result| result.tool_use_id.clone())
                .collect(),
            _ => HashSet::new(),
        };
        let tool_uses = std::mem::take(&mut turns[index].tool_uses);
        let mut unanswered = Vec::new();
        for tool_use in tool_uses {
            if answered.contains(&tool_use.id) {
                turns[index].tool_uses.push(tool_use);
            } else {
                unanswered.push(tool_use_as_text(
                    &tool_use.name,
                    &tool_use.id,
                    &tool_use.input,
                ));
            }
        }
        for text in unanswered {
            turns[index].push_text(&text);
        }
    }
}

fn history_entry(turn: &Turn, model_id: &str) -> Value {
    match turn.role {
        TurnRole::User => {
            let mut content = turn.content_or_placeholder();
            if !turn.images.is_empty() {
                content.push_str(&format!(
                    "\n\n[{} image(s) omitted from earlier context]",
                    turn.images.len()
                ));
            }
            let mut message = Map::new();
            message.insert("content".to_string(), Value::String(content));
            message.insert("modelId".to_string(), Value::String(model_id.to_string()));
            message.insert("origin".to_string(), Value::String(ORIGIN.to_string()));
            if !turn.tool_results.is_empty() {
                message.insert(
                    "userInputMessageContext".to_string(),
                    json!({ "toolResults": tool_results_json(&turn.tool_results) }),
                );
            }
            json!({ "userInputMessage": Value::Object(message) })
        }
        TurnRole::Assistant => {
            let mut message = Map::new();
            let content = if turn.text.trim().is_empty() {
                EMPTY_PLACEHOLDER.to_string()
            } else {
                turn.text.clone()
            };
            message.insert("content".to_string(), Value::String(content));
            if !turn.tool_uses.is_empty() {
                let tool_uses = turn
                    .tool_uses
                    .iter()
                    .map(|tool_use| {
                        json!({
                            "toolUseId": tool_use.id,
                            "name": tool_use.name,
                            "input": tool_use.input,
                        })
                    })
                    .collect();
                message.insert("toolUses".to_string(), Value::Array(tool_uses));
            }
            json!({ "assistantResponseMessage": Value::Object(message) })
        }
    }
}

fn tool_results_json(results: &[ToolResult]) -> Value {
    Value::Array(
        results
            .iter()
            .map(|result| {
                let text = if result.content.trim().is_empty() {
                    EMPTY_TOOL_RESULT.to_string()
                } else {
                    truncate_middle(&result.content, TOOL_RESULT_CHAR_LIMIT)
                };
                let status = if result.is_error { "error" } else { "success" };
                json!({
                    "toolUseId": result.tool_use_id,
                    "content": [{ "text": text }],
                    "status": status,
                })
            })
            .collect(),
    )
}

fn images_json(images: &[Image]) -> Value {
    Value::Array(
        images
            .iter()
            .map(|image| json!({ "format": image.format, "source": { "bytes": image.data } }))
            .collect(),
    )
}

fn image_from_block(media_type: &str, data: &str) -> Option<Image> {
    let (media_type, data) = match data.strip_prefix("data:") {
        Some(data_url) => {
            let (header, payload) = data_url.split_once(',')?;
            let header_media_type = header.split(';').next().unwrap_or(media_type);
            (header_media_type.to_string(), payload.to_string())
        }
        None => (media_type.to_string(), data.to_string()),
    };
    if data.trim().is_empty() {
        return None;
    }
    let format = media_type
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|format| !format.is_empty())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let format = if format == "jpg" {
        "jpeg".to_string()
    } else {
        format
    };
    Some(Image { format, data })
}

fn normalize_tool_input(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => Value::Object(Map::new()),
        },
        _ => Value::Object(Map::new()),
    }
}

fn tool_use_as_text(name: &str, id: &str, input: &Value) -> String {
    format!("[Tool call: {name} ({id})]\n{input}")
}

fn tool_result_as_text(tool_use_id: &str, content: &str) -> String {
    let content = if content.trim().is_empty() {
        EMPTY_TOOL_RESULT.to_string()
    } else {
        truncate_middle(content, TOOL_RESULT_CHAR_LIMIT)
    };
    format!("[Tool result ({tool_use_id})]\n{content}")
}

fn truncate_middle(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let keep = limit / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(count - keep).collect();
    format!(
        "{head}\n... [truncated {} characters] ...\n{tail}",
        count - keep * 2
    )
}

struct ToolSpecs {
    specs: Vec<Value>,
    names: BTreeSet<String>,
    documentation: String,
    skipped: Vec<String>,
}

fn build_tool_specs(tools: &[ToolDefinition]) -> ToolSpecs {
    let mut specs = Vec::with_capacity(tools.len());
    let mut names = BTreeSet::new();
    let mut documentation_parts = Vec::new();
    let mut skipped = Vec::new();

    for tool in tools {
        let name = tool.name.trim();
        if name.is_empty() {
            continue;
        }
        if name.chars().count() > MAX_TOOL_NAME_LEN {
            skipped.push(name.to_string());
            continue;
        }
        if !names.insert(name.to_string()) {
            continue;
        }

        let description = tool.description.trim();
        let description = if description.is_empty() {
            format!("Tool: {name}")
        } else if description.chars().count() > TOOL_DESCRIPTION_CHAR_LIMIT {
            documentation_parts.push(format!("## Tool: {name}\n\n{description}"));
            format!("[Full documentation in system prompt under '## Tool: {name}']")
        } else {
            description.to_string()
        };

        let schema = match &tool.input_schema {
            Value::Object(_) => sanitize_schema(&tool.input_schema),
            _ => json!({ "type": "object", "properties": {} }),
        };

        specs.push(json!({
            "toolSpecification": {
                "name": name,
                "description": description,
                "inputSchema": { "json": schema },
            }
        }));
    }

    let documentation = if documentation_parts.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n---\n# Tool Documentation\nThe following tools have detailed documentation that couldn't fit in the tool definition.\n\n{}",
            documentation_parts.join("\n\n---\n\n")
        )
    };

    ToolSpecs {
        specs,
        names,
        documentation,
        skipped,
    }
}

fn placeholder_tool_spec(name: &str) -> Value {
    json!({
        "toolSpecification": {
            "name": name,
            "description": format!("Tool: {name} (no longer available in this session)"),
            "inputSchema": { "json": { "type": "object", "properties": {} } },
        }
    })
}

/// Drop JSON Schema keywords the Kiro API rejects (`additionalProperties`,
/// empty `required` arrays), recursively. Property names are never dropped.
fn sanitize_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if key == "additionalProperties" {
                    continue;
                }
                if key == "required" && child.as_array().is_some_and(|items| items.is_empty()) {
                    continue;
                }
                let sanitized = match (key.as_str(), child) {
                    ("properties", Value::Object(properties)) => Value::Object(
                        properties
                            .iter()
                            .map(|(name, schema)| (name.clone(), sanitize_schema(schema)))
                            .collect(),
                    ),
                    _ => sanitize_schema(child),
                };
                out.insert(key.clone(), sanitized);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}
