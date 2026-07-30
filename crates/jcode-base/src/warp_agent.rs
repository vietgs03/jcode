//! Warp terminal third-party CLI-agent integration.
//!
//! When jcode runs inside [Warp](https://warp.dev), it can present itself as a
//! first-class CLI coding agent: Warp shows a per-tab working/done/blocked
//! status, an agent toolbelt, and fires in-app + desktop notifications when a
//! turn finishes or needs attention.
//!
//! Warp drives all of this from a single structured terminal escape sequence,
//! OSC 777, carrying a JSON payload under the sentinel title `warp://cli-agent`:
//!
//! ```text
//! ESC ] 777 ; notify ; warp://cli-agent ; {"v":1,"agent":"pi","event":"stop",...} BEL
//! ```
//!
//! Two facts about Warp's implementation shape this module (verified against
//! `warpdotdev/warp` at `crates/warp_core/src/cli_agent_protocol.rs` and
//! `app/src/terminal/cli_agent_sessions/*`):
//!
//! 1. Warp bootstraps its agent session/listener from the OSC event alone. No
//!    command-name match (`claude`, `codex`, ...) is required.
//! 2. Warp only renders rich status + notifications for agents on its built-in
//!    supported list, resolved from the payload's `agent` field via Warp's
//!    `CLIAgent::command_prefixes`. An unrecognized `agent` string degrades to
//!    `CLIAgent::Unknown`, which gets no status listener. jcode is not on
//!    Warp's list, so it advertises a supported identity (default `pi`, an
//!    open-source CLI agent) to light up the full experience. This is
//!    configurable via `[warp] cli_agent_identity`.
//!
//! The integration is a no-op unless jcode detects it is running inside a Warp
//! build that advertises structured-notification support (the
//! `WARP_CLI_AGENT_PROTOCOL_VERSION` environment variable) and the user has not
//! disabled it via `[warp] cli_agent_integration = false`.
//!
//! This module owns a tiny amount of global state (whether the session_start
//! handshake was sent, and the last processing state) so callers can drive it
//! with a single idempotent `sync_processing` call per UI tick instead of
//! threading the previous state through the app.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

/// Sentinel title identifying structured CLI-agent events (matches Warp's
/// `CLI_AGENT_NOTIFICATION_SENTINEL`).
const SENTINEL: &str = "warp://cli-agent";

/// Schema version of the payload we emit (matches Warp's
/// `CLI_AGENT_PROTOCOL_VERSION`).
const PROTOCOL_VERSION: u32 = 1;

/// Max characters of user/assistant text we put in a payload. Warp truncates
/// notification banners aggressively, and the sequence rides the PTY, so keep
/// it tight.
const TEXT_MAX_CHARS: usize = 200;

/// Wire representation of a Warp CLI-agent notification. Field set and
/// `skip_serializing_if` mirror Warp's `CLIAgentNotification` so unknown-field
/// tolerance is never exercised.
#[derive(Debug, Clone, Serialize)]
struct WarpNotification<'a> {
    v: u32,
    agent: &'a str,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_type: Option<&'a str>,
}

/// Coarse agent state jcode reports to Warp, derived from the interactive
/// session's processing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpAgentState {
    /// A turn is actively running (model thinking / streaming / tool use).
    Working,
    /// The turn finished successfully and the agent is idle.
    Idle,
    /// The turn ended in an error.
    Failed,
    /// The agent is blocked waiting on the user (e.g. tool approval).
    Blocked,
}

#[derive(Debug, Default)]
struct EmitterState {
    /// Whether we've sent the initial `session_start` handshake this process.
    session_started: bool,
    /// The last coarse state we reported, to suppress duplicate events.
    last_state: Option<WarpAgentState>,
    /// Latch set when a turn ends in error, consumed on the next transition to
    /// idle so it is reported as `stop_failure` rather than `stop`. Cleared when
    /// a new turn starts (transition to working).
    pending_failure: bool,
    /// Latch set when the agent is blocked awaiting the user (e.g. a tool
    /// approval prompt), consumed on the next idle transition. Cleared when a
    /// new turn starts.
    pending_blocked: Option<String>,
}

static STATE: Mutex<EmitterState> = Mutex::new(EmitterState {
    session_started: false,
    last_state: None,
    pending_failure: false,
    pending_blocked: None,
});

/// Returns true when jcode is running inside a Warp build that supports
/// structured CLI-agent notifications. Warp exports
/// `WARP_CLI_AGENT_PROTOCOL_VERSION` on the PTY precisely so agents can detect
/// this; its mere presence is the signal.
pub fn warp_supports_structured() -> bool {
    std::env::var_os("WARP_CLI_AGENT_PROTOCOL_VERSION").is_some()
}

/// Whether the integration should run: inside a capable Warp build and enabled
/// in config.
fn integration_active() -> bool {
    if !warp_supports_structured() {
        return false;
    }
    crate::config::config().warp.cli_agent_integration
}

/// The agent identity to advertise to Warp. Falls back to `pi` when the
/// configured value is blank.
fn agent_identity() -> String {
    let configured = crate::config::config().warp.cli_agent_identity.clone();
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        "pi".to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max.saturating_sub(3)).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        // No truncation needed; recollect the original (cheap, bounded).
        text.chars().take(max).collect()
    }
}

fn project_from_cwd(cwd: Option<&Path>) -> Option<String> {
    cwd.and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}

fn cwd_string(cwd: Option<&Path>) -> Option<String> {
    cwd.map(|p| p.to_string_lossy().into_owned())
}

/// Serialize a notification and write it to the terminal as an OSC 777 sequence.
///
/// Written directly to stdout (the PTY Warp reads), matching how jcode already
/// emits `SetTitle` OSC sequences during the running TUI. OSC 777 does not
/// render any cells, so it is safe to interleave with ratatui output.
fn emit(notification: &WarpNotification<'_>) {
    let Ok(json) = serde_json::to_string(notification) else {
        return;
    };
    // OSC 777: ESC ] 777 ; notify ; <title> ; <body> BEL
    let seq = format!("\x1b]777;notify;{SENTINEL};{json}\x07");

    // In tests (and for downstream integration tests via `test-support`), route
    // the raw sequence to an in-memory sink instead of the real terminal so the
    // emitted events can be asserted without a PTY.
    #[cfg(any(test, feature = "test-support"))]
    if test_sink::capture(&seq) {
        return;
    }

    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(seq.as_bytes()).is_ok() {
        let _ = stdout.flush();
    }
}

/// Test-only capture sink for emitted OSC sequences. Enabled with the
/// `test-support` feature so downstream crates' integration tests can drive the
/// real emission path and assert the wire output without a terminal.
#[cfg(any(test, feature = "test-support"))]
pub mod test_sink {
    use std::sync::Mutex;

    static SINK: Mutex<Option<Vec<String>>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<Vec<String>>> {
        match SINK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Begin capturing emitted sequences into memory. Also resets the emitter's
    /// transition state so a test starts from a clean slate.
    pub fn start() {
        *lock() = Some(Vec::new());
        super::reset_for_tests();
    }

    /// Stop capturing and return everything captured so far.
    pub fn take() -> Vec<String> {
        lock().take().unwrap_or_default()
    }

    /// Record a sequence if capture is active. Returns true when captured (so
    /// the caller skips the real terminal write).
    pub(super) fn capture(seq: &str) -> bool {
        let mut guard = lock();
        if let Some(buf) = guard.as_mut() {
            buf.push(seq.to_string());
            true
        } else {
            false
        }
    }

    /// Parse captured sequences into `(event, agent)` pairs for assertions.
    pub fn events() -> Vec<(String, String)> {
        take()
            .iter()
            .filter_map(|seq| {
                let body = seq.strip_prefix("\x1b]777;notify;warp://cli-agent;")?;
                let body = body.strip_suffix('\x07').unwrap_or(body);
                let v: serde_json::Value = serde_json::from_str(body).ok()?;
                Some((
                    v.get("event")?.as_str()?.to_string(),
                    v.get("agent").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                ))
            })
            .collect()
    }
}

/// Context passed to [`sync_processing`], describing the current turn.
#[derive(Debug, Default, Clone)]
pub struct WarpSyncContext<'a> {
    pub session_id: Option<&'a str>,
    pub cwd: Option<&'a Path>,
    /// The user's latest prompt (shown as the tab/notification title in Warp).
    pub query: Option<&'a str>,
    /// The agent's latest response text (shown in the completion notification).
    pub response: Option<&'a str>,
}

/// Emit the one-time `session_start` handshake so Warp shows the agent toolbelt
/// as soon as jcode launches, before the first turn. Idempotent and safe to
/// call repeatedly.
pub fn announce_session_start(session_id: Option<&str>, cwd: Option<&Path>) {
    if !integration_active() {
        return;
    }
    let mut guard = lock();
    if guard.session_started {
        return;
    }
    guard.session_started = true;
    drop(guard);

    let identity = agent_identity();
    emit(&WarpNotification {
        v: PROTOCOL_VERSION,
        agent: &identity,
        event: "session_start",
        session_id,
        cwd: cwd_string(cwd),
        project: project_from_cwd(cwd),
        query: None,
        response: None,
        summary: None,
        plugin_version: Some(env!("CARGO_PKG_VERSION")),
        error_type: None,
    });
}

/// Latch that the current/just-finished turn ended in an error. Consumed on the
/// next transition to idle, which is then reported to Warp as `stop_failure`
/// instead of `stop`. Safe to call outside Warp (no-op-ish; just sets a flag).
pub fn note_turn_failed() {
    if !warp_supports_structured() {
        return;
    }
    lock().pending_failure = true;
}

/// Mark the agent as blocked awaiting the user (e.g. a tool-approval prompt),
/// with an optional human-readable summary. Emits a `permission_request` event
/// so Warp shows the tab as needing attention and fires a request notification.
pub fn note_blocked(summary: Option<&str>) {
    if !integration_active() {
        return;
    }
    // Record the reason so a later idle transition can clear cleanly, and drive
    // an immediate Blocked emission through the shared transition path.
    {
        let mut guard = lock();
        guard.pending_blocked =
            Some(summary.map(|s| s.to_string()).unwrap_or_else(|| "Waiting for your input".to_string()));
    }
    let ctx = WarpSyncContext::default();
    emit_transition(WarpAgentState::Blocked, &ctx);
}

/// Reconcile the reported Warp agent state with the interactive session's
/// processing flag. Call once per UI tick with `is_processing`; the module
/// tracks the last reported state and emits a Warp event only on a transition:
///
/// - idle -> working: `prompt_submit` (Warp shows the tab as working)
/// - working -> idle: `stop` (success) or `stop_failure` if [`note_turn_failed`]
///   was latched during the turn
///
/// This is the single call sites in both the local and remote run loops need.
pub fn sync_processing(is_processing: bool, ctx: &WarpSyncContext<'_>) {
    if !integration_active() {
        return;
    }
    let state = if is_processing {
        WarpAgentState::Working
    } else {
        WarpAgentState::Idle
    };
    emit_transition(state, ctx);
}

/// Outcome of the pure transition decision: how the emitter state should
/// advance and which status event (if any) to emit. Extracted from
/// [`emit_transition`] so the latch/dedup/phantom-stop logic is unit-testable
/// without touching global state or the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TransitionDecision {
    /// Emit the one-time `session_start` handshake before any status event.
    emit_session_start: bool,
    /// The status event to emit, or `None` to suppress (no-op repeat, or an
    /// idle report with no preceding turn).
    event: Option<&'static str>,
    /// Whether the emitted event carries `error_type: "error"` (i.e. it is a
    /// `stop_failure`).
    is_failure: bool,
    /// The emitter state after applying this transition.
    next: EmitterStateSnapshot,
}

/// The mutable fields of [`EmitterState`] as a `Copy`-friendly snapshot, so the
/// decision function is pure (no `Mutex`, no `String` cloning of the summary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct EmitterStateSnapshot {
    session_started: bool,
    last_state: Option<WarpAgentState>,
    pending_failure: bool,
    has_pending_blocked: bool,
}

/// Pure transition machine. Given the prior snapshot and the target state,
/// decide what to emit and the next snapshot. No side effects.
fn decide_transition(prev: EmitterStateSnapshot, state: WarpAgentState) -> TransitionDecision {
    let need_session_start = !prev.session_started;

    // Suppress no-op repeats. Blocked re-entry is always allowed through since
    // its summary may differ.
    if prev.last_state == Some(state)
        && !need_session_start
        && !matches!(state, WarpAgentState::Blocked)
    {
        return TransitionDecision {
            emit_session_start: false,
            event: None,
            is_failure: false,
            next: prev,
        };
    }

    // An Idle report only means "turn finished" when we were actually working
    // (or blocked). At startup, or between turns, `last_state` is None/Idle and
    // there is nothing to report -- emitting `stop` there would make Warp show a
    // phantom success before any turn ran.
    let idle_without_turn = matches!(state, WarpAgentState::Idle)
        && !matches!(
            prev.last_state,
            Some(WarpAgentState::Working) | Some(WarpAgentState::Blocked)
        );

    let (event, is_failure): (Option<&'static str>, bool) = if idle_without_turn {
        (None, false)
    } else {
        match state {
            WarpAgentState::Working => (Some("prompt_submit"), false),
            WarpAgentState::Blocked => (Some("permission_request"), false),
            WarpAgentState::Failed => (Some("stop_failure"), true),
            WarpAgentState::Idle => {
                if prev.pending_failure {
                    (Some("stop_failure"), true)
                } else {
                    (Some("stop"), false)
                }
            }
        }
    };

    // Compute next snapshot: clear latches on any turn boundary; Blocked keeps
    // its recorded summary (set separately by `note_blocked`).
    let has_pending_blocked = matches!(state, WarpAgentState::Blocked) && prev.has_pending_blocked;
    let next = EmitterStateSnapshot {
        session_started: true,
        last_state: Some(state),
        pending_failure: false,
        has_pending_blocked,
    };

    TransitionDecision {
        emit_session_start: need_session_start,
        event,
        is_failure,
        next,
    }
}

/// Shared transition machine: emits the appropriate Warp event when `state`
/// differs from the last reported state, consuming the failure/blocked latches.
fn emit_transition(state: WarpAgentState, ctx: &WarpSyncContext<'_>) {
    let mut guard = lock();
    let prev = EmitterStateSnapshot {
        session_started: guard.session_started,
        last_state: guard.last_state,
        pending_failure: guard.pending_failure,
        has_pending_blocked: guard.pending_blocked.is_some(),
    };
    let decision = decide_transition(prev, state);
    let blocked_summary = guard.pending_blocked.clone();

    // Commit the next snapshot back into the live state.
    guard.session_started = decision.next.session_started;
    guard.last_state = decision.next.last_state;
    guard.pending_failure = decision.next.pending_failure;
    if !decision.next.has_pending_blocked {
        guard.pending_blocked = None;
    }
    drop(guard);

    let identity = agent_identity();
    let cwd = cwd_string(ctx.cwd);
    let project = project_from_cwd(ctx.cwd);

    // Always emit the one-time handshake so the toolbelt shows up, even when the
    // triggering status event itself is suppressed.
    if decision.emit_session_start {
        emit(&WarpNotification {
            v: PROTOCOL_VERSION,
            agent: &identity,
            event: "session_start",
            session_id: ctx.session_id,
            cwd: cwd.clone(),
            project: project.clone(),
            query: None,
            response: None,
            summary: None,
            plugin_version: Some(env!("CARGO_PKG_VERSION")),
            error_type: None,
        });
    }

    let Some(event) = decision.event else {
        return;
    };

    let query = ctx.query.map(|q| truncate(q, TEXT_MAX_CHARS));
    let response = matches!(state, WarpAgentState::Idle | WarpAgentState::Failed)
        .then(|| ctx.response.map(|r| truncate(r, TEXT_MAX_CHARS)))
        .flatten();
    let summary = matches!(state, WarpAgentState::Blocked)
        .then_some(blocked_summary)
        .flatten();
    let error_type = decision.is_failure.then_some("error");

    emit(&WarpNotification {
        v: PROTOCOL_VERSION,
        agent: &identity,
        event,
        session_id: ctx.session_id,
        cwd,
        project,
        query,
        response,
        summary,
        plugin_version: None,
        error_type,
    });
}

fn lock() -> std::sync::MutexGuard<'static, EmitterState> {
    match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Reset the emitter's global state. Intended for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_tests() {
    *lock() = EmitterState::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification_json(n: &WarpNotification<'_>) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(n).unwrap()).unwrap()
    }

    #[test]
    fn payload_matches_warp_schema_and_skips_none() {
        let n = WarpNotification {
            v: PROTOCOL_VERSION,
            agent: "pi",
            event: "stop",
            session_id: Some("sess-1"),
            cwd: Some("/home/u/proj".to_string()),
            project: Some("proj".to_string()),
            query: Some("do the thing".to_string()),
            response: Some("done".to_string()),
            summary: None,
            plugin_version: None,
            error_type: None,
        };
        let v = notification_json(&n);
        assert_eq!(v["v"], 1);
        assert_eq!(v["agent"], "pi");
        assert_eq!(v["event"], "stop");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["cwd"], "/home/u/proj");
        assert_eq!(v["project"], "proj");
        assert_eq!(v["query"], "do the thing");
        assert_eq!(v["response"], "done");
        // skip_serializing_if drops None fields entirely.
        assert!(v.get("summary").is_none());
        assert!(v.get("plugin_version").is_none());
        assert!(v.get("error_type").is_none());
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_needed() {
        assert_eq!(truncate("short", 200), "short");
        let long = "x".repeat(500);
        let out = truncate(&long, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn project_is_basename_of_cwd() {
        assert_eq!(
            project_from_cwd(Some(Path::new("/a/b/myproj"))),
            Some("myproj".to_string())
        );
        assert_eq!(project_from_cwd(None), None);
    }

    #[test]
    fn event_names_match_protocol() {
        // Guard against accidental renames: these strings are Warp's wire
        // contract (v1::parse in Warp maps exactly these).
        let cases = [
            (WarpAgentState::Working, "prompt_submit"),
            (WarpAgentState::Idle, "stop"),
            (WarpAgentState::Failed, "stop_failure"),
            (WarpAgentState::Blocked, "permission_request"),
        ];
        for (state, expected) in cases {
            let event = match state {
                WarpAgentState::Working => "prompt_submit",
                WarpAgentState::Idle => "stop",
                WarpAgentState::Failed => "stop_failure",
                WarpAgentState::Blocked => "permission_request",
            };
            assert_eq!(event, expected);
        }
    }

    // ---- decide_transition: the core state machine ----

    fn fresh() -> EmitterStateSnapshot {
        EmitterStateSnapshot::default()
    }

    #[test]
    fn first_working_emits_session_start_then_prompt_submit() {
        let d = decide_transition(fresh(), WarpAgentState::Working);
        assert!(d.emit_session_start, "handshake must precede first event");
        assert_eq!(d.event, Some("prompt_submit"));
        assert!(!d.is_failure);
        assert_eq!(d.next.last_state, Some(WarpAgentState::Working));
        assert!(d.next.session_started);
    }

    #[test]
    fn idle_before_any_turn_emits_handshake_but_no_stop() {
        // Startup announce path: first observed state is Idle. Warp must not
        // show a phantom success.
        let d = decide_transition(fresh(), WarpAgentState::Idle);
        assert!(d.emit_session_start);
        assert_eq!(d.event, None, "no stop before any turn ran");
        assert_eq!(d.next.last_state, Some(WarpAgentState::Idle));
    }

    #[test]
    fn working_then_idle_emits_stop() {
        let after_start = decide_transition(fresh(), WarpAgentState::Working).next;
        let d = decide_transition(after_start, WarpAgentState::Idle);
        assert!(!d.emit_session_start, "handshake already sent");
        assert_eq!(d.event, Some("stop"));
        assert!(!d.is_failure);
    }

    #[test]
    fn failure_latch_turns_idle_into_stop_failure() {
        let working = decide_transition(fresh(), WarpAgentState::Working).next;
        // note_turn_failed() sets pending_failure while still "working".
        let latched = EmitterStateSnapshot {
            pending_failure: true,
            ..working
        };
        let d = decide_transition(latched, WarpAgentState::Idle);
        assert_eq!(d.event, Some("stop_failure"));
        assert!(d.is_failure);
        assert!(!d.next.pending_failure, "latch consumed");
    }

    #[test]
    fn duplicate_working_is_suppressed() {
        let working = decide_transition(fresh(), WarpAgentState::Working).next;
        let d = decide_transition(working, WarpAgentState::Working);
        assert!(!d.emit_session_start);
        assert_eq!(d.event, None, "no duplicate prompt_submit");
    }

    #[test]
    fn blocked_then_idle_emits_stop() {
        let working = decide_transition(fresh(), WarpAgentState::Working).next;
        let blocked = decide_transition(working, WarpAgentState::Blocked);
        assert_eq!(blocked.event, Some("permission_request"));
        let d = decide_transition(blocked.next, WarpAgentState::Idle);
        // A blocked->idle transition counts as a completed turn.
        assert_eq!(d.event, Some("stop"));
    }

    #[test]
    fn repeated_idle_between_turns_is_silent() {
        // working -> idle (stop), then idle again should not re-emit stop.
        let working = decide_transition(fresh(), WarpAgentState::Working).next;
        let idle = decide_transition(working, WarpAgentState::Idle).next;
        let d = decide_transition(idle, WarpAgentState::Idle);
        assert_eq!(d.event, None);
        assert!(!d.emit_session_start);
    }

    #[test]
    fn second_turn_after_idle_emits_prompt_submit() {
        let working = decide_transition(fresh(), WarpAgentState::Working).next;
        let idle = decide_transition(working, WarpAgentState::Idle).next;
        let d = decide_transition(idle, WarpAgentState::Working);
        assert_eq!(d.event, Some("prompt_submit"));
        assert!(!d.emit_session_start);
    }

    // ---- end-to-end through the real emit() path into the capture sink ----
    //
    // Exercised in a single #[test] because they share process-global state
    // (the WARP_* env var, the emitter STATE, and the capture SINK); Rust's
    // default parallel runner would otherwise race these against each other.
    #[test]
    fn end_to_end_emission_through_real_path() {
        // SAFETY: this is the only test touching this env var, and it runs its
        // sub-scenarios sequentially.
        unsafe {
            std::env::set_var("WARP_CLI_AGENT_PROTOCOL_VERSION", "1");
        }

        // Scenario A: successful turn lifecycle.
        test_sink::start();
        let ctx = WarpSyncContext {
            session_id: Some("sess-e2e"),
            query: Some("hello"),
            response: Some("world"),
            ..Default::default()
        };
        announce_session_start(Some("sess-e2e"), None); // session_start, no phantom stop
        sync_processing(true, &ctx); // prompt_submit
        sync_processing(false, &ctx); // stop
        let events: Vec<String> = test_sink::events().into_iter().map(|(e, _)| e).collect();
        assert_eq!(
            events,
            vec![
                "session_start".to_string(),
                "prompt_submit".to_string(),
                "stop".to_string()
            ],
            "expected handshake, then working, then success"
        );

        // Scenario B: failed turn latches into stop_failure.
        test_sink::start();
        let ctx = WarpSyncContext {
            session_id: Some("sess-fail"),
            ..Default::default()
        };
        sync_processing(true, &ctx); // session_start + prompt_submit
        note_turn_failed(); // latch failure
        sync_processing(false, &ctx); // -> stop_failure
        let events: Vec<String> = test_sink::events().into_iter().map(|(e, _)| e).collect();
        assert_eq!(
            events,
            vec![
                "session_start".to_string(),
                "prompt_submit".to_string(),
                "stop_failure".to_string()
            ]
        );

        // Scenario C: outside Warp (env var absent) nothing is emitted.
        unsafe {
            std::env::remove_var("WARP_CLI_AGENT_PROTOCOL_VERSION");
        }
        test_sink::start();
        let ctx = WarpSyncContext::default();
        announce_session_start(Some("sess-off"), None);
        sync_processing(true, &ctx);
        sync_processing(false, &ctx);
        assert!(
            test_sink::events().is_empty(),
            "no events should be emitted outside Warp"
        );
    }
}
