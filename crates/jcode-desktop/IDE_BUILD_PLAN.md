# jcode-desktop → IDE: Build Plan & Spec

**Role:** Principal (you) controls direction + reviews. **Slave agent** executes phases.
**Goal:** Turn the existing `jcode-desktop` GPU app into a real IDE for jcode:
dark theme, clean fonts, and rich tool/terminal display (show what bash runs +
its output), without rewriting the renderer.

## Why extend, not rebuild
`crates/jcode-desktop` is already a lightweight native GPU app:
- `wgpu` (GPU render) + `winit` (window) + `glyphon`/`cosmic-text` (GPU text)
- `arboard` clipboard → native copy (no terminal `│` problem)
- Already connects to the jcode daemon and streams session events
- Already renders tool cards, markdown, model picker, session switcher

Building a GPU terminal from scratch (à la Zed/gpui) is months of work to
re-reach this baseline. The real gaps are **aesthetic** and **tool display
depth**, which are surgical edits.

## Key files (verified)
| Concern | Location |
|---|---|
| Theme colors (all `const *_COLOR`) | `crates/jcode-desktop/src/main.rs` ~L544-610 |
| Background gradient corners | `main.rs` `BACKGROUND_TOP_LEFT`… |
| Font families | `crates/jcode-desktop/src/single_session.rs` L21-28 |
| Font loading (`include_bytes!`) | `main.rs` `create_desktop_font_system()` ~L6528 |
| Bundled fonts | `crates/jcode-desktop/assets/fonts/` |
| Tool card rendering | `crates/jcode-desktop/src/single_session.rs` (`append_tool_*`, `widget_lines`) ~L8630-8686 |
| Tool output drawer geometry | `crates/jcode-desktop/src/single_session_render.rs` ~L7121 |
| Tool name mapping / status | `crates/jcode-tui-tool-display/src/lib.rs` |
| Canonical message model | `crates/jcode-message-types/src/lib.rs` (`ContentBlock::{ToolUse,ToolResult}`) |
| Session event kinds | `single_session.rs` `SingleSessionStatus` (ToolPreparing/Using/Finished) |

## Data model (verified from real sessions)
A bash tool call in a session is:
```json
{"type":"tool_use","id":"toolu_…","name":"bash",
 "input":{"command":"which jcode; …"}}
```
Its result:
```json
{"type":"tool_result","tool_use_id":"toolu_…",
 "content":"/home/…/jcode\njcode v0.30.2\n…"}
```
So the UI always has: tool name, the exact command (`input.command`), and the
full stdout/stderr (`content`). Tool name normalization lives in
`resolve_display_tool_name` (shell_exec→bash, file_read→read, …).

## Verification tooling (use these, don't eyeball)
- Build: `cargo build -p jcode-desktop --bin jcode-desktop`  (~30s incremental)
- Tests: `cargo test -p jcode-desktop --bin jcode-desktop`   (495 tests, keep green)
- **Headless screenshots** (no window needed):
  `./target/debug/jcode-desktop --capture-gallery-screens DIR --gallery-state STATE --capture-size 1400x900`
  States: empty, markdown, tool-running, tool-success, tool-failed, tool-stack,
  streaming, error, hotkey-help, model-picker, session-info, session-switcher,
  slash-suggestions, long-transcript
- Live window (WSLg): `WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 ./target/debug/jcode-desktop`
- Headless smoke: `--headless-chat-smoke "<msg>"`

Always render the relevant `--gallery-state` to PNG and inspect it before
claiming a visual change works.

## Phases

### Phase 0 — DONE (principal PoC, branch `feat/desktop-ide-theme`)
- [x] Dark IDE background + inverted text/card colors
- [x] Unified clean monospace font (dropped Kalam/Homemade Apple handwriting)
- [x] Bundle JetBrainsMono Nerd Font into assets
- [x] 495/495 tests green; gallery PNGs verified dark

### Phase 1 — Tool/terminal display depth (PRIORITY: user's main ask)
Goal: a bash tool card should read like a terminal block.
- [ ] Header row: `▸ bash  ⟩ $ <command first line>` + status pill (running/done/failed) + duration
- [ ] Expanded body = **terminal-styled output**: dark inset, mono, preserve
      ANSI-ish layout, show last N lines while running and full on success,
      with a "X more lines" affordance.
- [ ] Distinguish stdout vs `Exit code: N` (red on nonzero — logic already in
      `parse_nonzero_exit_code_line`).
- [ ] For edit/write tools: show a diff-style preview (file + +/- lines).
- [ ] Keep collapsed default for noise tools (read/glob) but bash/edit expand.
- Verify with `--gallery-state tool-running`, `tool-success`, `tool-failed`,
  `tool-stack`, plus a live session running real bash.

### Phase 2 — IDE chrome (layout)
- [ ] Left **session sidebar** (reuse session index model) with status dots,
      title, dir, model, msg count; filter box.
- [ ] **Tab bar** for open sessions; activity bar (sessions/search/settings).
- [ ] **Status bar** (model, tokens, connection, cwd).
- [ ] Use `--workspace` multi-session mode as the base if it fits.

### Phase 3 — Interaction
- [ ] Composer input wired through the existing session backend (send prompts).
- [ ] Copy button / keybind on every code + tool-output block (arboard).
- [ ] Syntax highlighting for code blocks (lang from fence).

### Phase 4 — Polish
- [ ] Theme constants extracted into a `theme.rs` (light/dark switch).
- [ ] Config for font size / family.
- [ ] Launcher: Windows shortcut + `jcode desktop` subcommand.

## Guardrails for the slave
1. One phase per PR-style commit; run tests + render gallery before commit.
2. Never break the 495 tests; update a test only when behavior legitimately
   changed, and explain why in the commit (see Phase 0 font-assert update).
3. Prefer editing shared theme constants over scattering literals.
4. Keep the renderer architecture; do not introduce a new windowing/GPU stack.
5. Report each phase with before/after gallery PNGs.
