# Phase 0 architecture

Phase 0 contains three deliberately small modules:

- React/TypeScript presents status and local in-memory settings. It invokes one Tauri command and does no native work.
- Rust/Tauri owns orchestration. `native_status` starts the narrow native adapter, validates its JSON, emits the typed `runtime-status` event and returns typed status to the UI.
- Swift owns only the macOS-native process endpoint. Its current `status` command proves the native seam without pretending that audio, permissions, transcription or AI exists.

Communication path:

```text
React invoke("native_status")
  → Tauri command in Rust
  → intervjuare-macos-bridge status
  → JSON on stdout
  → validated Rust response
  → Tauri runtime-status event + command response
  → React event receipt + status card
```

The event architecture is intentionally minimal: one typed command request/response and one typed Rust-to-React event. The build script compiles the Swift source deterministically into Cargo's active profile directory. Rust resolves it relative to its own executable, while Tauri bundles the same binary as an external sidecar for a relocatable `.app`. No candidate profile has been created and no illustrative README facts were copied into runtime data.

# Milestones 2–5 additions

Rust gains small, separately tested modules; the Phase 1 audio pipeline is unchanged except for two hooks (every emitted transcript event is passed to the copilot, and Start records the meeting app for screenshots):

- `conversation.rs`: speaker-aware segments and turns built from `transcript-event`s (revision/final rules, session reset, bounded buffer).
- `profile.rs`: loads and validates `profile.yaml`, extracts citable ids.
- `context.rs`: job context, answer modes and the context package.
- `codex_auth.rs`: read-only discovery of the existing ChatGPT (Codex) sign-ins; exact account selection.
- `llm.rs`: direct streaming client for the ChatGPT Codex backend (Responses API over `/usr/bin/curl`), cancellation, model list.
- `suggestion.rs`: progressive parser for the `ANCHORS / ANSWER / SOURCES` contract.
- `memory.rs`: background interview memory.
- `voice_command.rs`: local "Låt oss se" / "Let's see" detection.
- `screen.rs` + Swift `screenshot` command: one in-memory screenshot per request.
- `settings.rs`: persisted local settings.
- `copilot.rs`: orchestration and Tauri commands; emits `suggestion-event`, `memory-event`, `voice-command`.

```text
STT dispatcher ──transcript-event──► React transcript
       │
       └─► copilot::on_transcript ─► Conversation ─► trigger rules ─┬─► "Låt oss se"? ─► Swift screenshot
                                                                     │
          context package (job + profile + memory + stories used + recent turns + current turn [+ image])
                                                                     │
                   generation thread ─► chatgpt.com/backend-api/codex/responses (SSE)
                                                                     │
                   suggestion::parse on every delta ─► suggestion-event ─► SuggestionPanel
                   memory thread (background) ─► memory-event ─► Interview memory
```

The hook runs on the dispatcher thread but never blocks on the network: it updates state under a short lock and spawns worker threads. See `docs/model-runtime.md`, `docs/context-management.md` and `docs/screenshot.md`.
