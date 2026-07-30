//! Bridge between the interactive [`App`] state and the Warp CLI-agent emitter
//! in `jcode-base` (`crate::warp_agent`).
//!
//! The emitter itself is stateless with respect to the app: it tracks the last
//! reported processing state internally and only writes an OSC sequence on a
//! real transition. So the bridge just needs to call [`App::sync_warp_agent`]
//! once per UI tick with the current `is_processing` flag; everything else
//! (Warp detection, config gate, deduplication, session_start handshake) is
//! handled downstream. When jcode is not running inside Warp, every call is a
//! cheap early return.

use super::App;

impl App {
    /// Build the per-turn context (session id, cwd, latest prompt + response)
    /// shared by the Warp status events.
    fn warp_sync_context(&self) -> WarpOwnedContext {
        let cwd = std::env::current_dir().ok();
        let query = self.last_submitted_input.clone();
        // The most recent assistant text becomes the completion-notification
        // body in Warp. Only meaningful once a turn has produced output.
        let response = self
            .display_messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
            .map(|m| m.content.clone());
        WarpOwnedContext {
            session_id: self.active_client_session_id().map(str::to_string),
            cwd,
            query,
            response,
        }
    }

    /// Reconcile Warp's per-tab agent status with this client's processing
    /// state. Called from both the local and remote run-loop ticks. No-op
    /// outside Warp or when disabled in config.
    pub(crate) fn sync_warp_agent(&self) {
        // Replay/export instances must never write control sequences to a live
        // terminal they don't own.
        if self.is_replay {
            return;
        }
        let owned = self.warp_sync_context();
        let ctx = crate::warp_agent::WarpSyncContext {
            session_id: owned.session_id.as_deref(),
            cwd: owned.cwd.as_deref(),
            query: owned.query.as_deref(),
            response: owned.response.as_deref(),
        };
        crate::warp_agent::sync_processing(self.is_processing, &ctx);
    }

    /// Emit the one-time Warp `session_start` handshake so the agent toolbelt
    /// appears immediately on launch. No-op outside Warp or when disabled.
    pub(crate) fn announce_warp_agent_session_start(&self) {
        if self.is_replay {
            return;
        }
        let cwd = std::env::current_dir().ok();
        crate::warp_agent::announce_session_start(
            self.active_client_session_id(),
            cwd.as_deref(),
        );
    }
}

/// Owned backing store for the borrowed `WarpSyncContext`, so the App method can
/// assemble strings (cwd, cloned prompt/response) that outlive the call.
struct WarpOwnedContext {
    session_id: Option<String>,
    cwd: Option<std::path::PathBuf>,
    query: Option<String>,
    response: Option<String>,
}
