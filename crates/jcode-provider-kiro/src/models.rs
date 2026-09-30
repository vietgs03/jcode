//! Kiro model catalog.
//!
//! Model ids are the *wire* ids accepted by `GenerateAssistantResponse`
//! (dotted versions, e.g. `claude-sonnet-4.5`). Users may also type the
//! dashed spelling used elsewhere in jcode (`claude-sonnet-4-5`);
//! [`normalize_model_id`] maps it back to the wire id.

/// Static metadata for one Kiro model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KiroModel {
    /// Wire model id sent as `userInputMessage.modelId`.
    pub id: &'static str,
    /// Context window in tokens, used for jcode's compaction budget.
    pub context_window: usize,
    /// Whether the model accepts image inputs.
    pub supports_images: bool,
}

const fn model(id: &'static str, context_window: usize, supports_images: bool) -> KiroModel {
    KiroModel {
        id,
        context_window,
        supports_images,
    }
}

/// Default model when neither `JCODE_KIRO_MODEL` nor a saved model is set.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4.5";

/// Context window used for unknown model ids.
pub const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// Known Kiro models, best-first for the model picker.
///
/// Availability depends on the account's Kiro plan and API region; the API
/// rejects models the account cannot use, so this list is a catalog, not an
/// entitlement check. Arbitrary ids can still be selected with `kiro:<id>`.
pub const KIRO_MODELS: &[KiroModel] = &[
    model("claude-sonnet-4.5", 200_000, true),
    model("claude-opus-4.8", 1_000_000, true),
    model("claude-opus-4.7", 1_000_000, true),
    model("claude-opus-4.6", 1_000_000, true),
    model("claude-opus-4.5", 200_000, true),
    model("claude-sonnet-5", 1_000_000, true),
    model("claude-opus-5", 1_000_000, true),
    model("claude-sonnet-4.6", 1_000_000, true),
    model("claude-sonnet-4.5-1m", 1_000_000, true),
    model("claude-sonnet-4", 200_000, true),
    model("claude-haiku-4.5", 200_000, true),
    model("auto", 200_000, false),
    model("deepseek-3.2", 128_000, false),
    model("glm-5", 200_000, false),
    model("minimax-m2.5", 200_000, false),
    model("minimax-m2.1", 200_000, false),
    model("qwen3-coder-next", 256_000, false),
];

/// Look up a known model by wire id.
pub fn find_model(id: &str) -> Option<&'static KiroModel> {
    KIRO_MODELS.iter().find(|model| model.id == id)
}

/// Wire ids of every known model, in picker order.
pub fn model_ids() -> Vec<&'static str> {
    KIRO_MODELS.iter().map(|model| model.id).collect()
}

/// Map user input to a wire model id.
///
/// Known ids pass through. A dashed version (`claude-sonnet-4-5`) is converted
/// to the dotted wire form when that form is a known model. Anything else is
/// returned trimmed and unchanged so new models can be used before this
/// catalog learns about them.
pub fn normalize_model_id(model: &str) -> String {
    let trimmed = model.trim();
    let trimmed = trimmed.strip_prefix("kiro:").unwrap_or(trimmed).trim();
    if find_model(trimmed).is_some() {
        return trimmed.to_string();
    }
    let dotted = dash_versions_to_dots(trimmed);
    if find_model(&dotted).is_some() {
        return dotted;
    }
    trimmed.to_string()
}

/// Context window for a model id (unknown ids get [`DEFAULT_CONTEXT_WINDOW`]).
pub fn context_window_for_model(model: &str) -> usize {
    let normalized = normalize_model_id(model);
    find_model(&normalized)
        .map(|model| model.context_window)
        .unwrap_or_else(|| {
            if normalized.ends_with("-1m") {
                1_000_000
            } else {
                DEFAULT_CONTEXT_WINDOW
            }
        })
}

/// Whether a model id accepts images (unknown ids: only Claude models).
pub fn model_supports_images(model: &str) -> bool {
    let normalized = normalize_model_id(model);
    find_model(&normalized)
        .map(|model| model.supports_images)
        .unwrap_or_else(|| normalized.starts_with("claude-"))
}

/// Replace `<digit>-<digit>` with `<digit>.<digit>` (`4-5` -> `4.5`).
fn dash_versions_to_dots(model: &str) -> String {
    let chars: Vec<char> = model.chars().collect();
    let mut out = String::with_capacity(model.len());
    for (index, ch) in chars.iter().enumerate() {
        let between_digits = index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit();
        if *ch == '-' && between_digits {
            out.push('.');
        } else {
            out.push(*ch);
        }
    }
    out
}
