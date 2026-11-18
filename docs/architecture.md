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

# Milestone 2 additions

Rust gains small, separately tested modules; the Phase 1 audio pipeline is unchanged except for one hook:

- `conversation.rs` — speaker-aware segments and turns built from `transcript-event`s (revision/final rules, session reset, bounded buffer).
- `profile.rs` — loads and validates `profile.yaml`, extracts citable ids.
- `context.rs` — job context and the context package sent to Hermes.
- `hermes.rs` — Hermes API server client: config from the Hermes `.env`, streaming Chat Completions over `/usr/bin/curl`, SSE decoding, cancellation, connection check.
- `suggestion.rs` — progressive parser for the `ANCHORS / ANSWER / SOURCES` contract.
- `copilot.rs` — orchestration and Tauri commands; emits `suggestion-event`.

```text
STT dispatcher ──transcript-event──► React transcript
       │
       └─► copilot::on_transcript ─► Conversation ─► trigger rules
                                                     │
                       context package (job + profile + recent turns + current turn)
                                                     │
                     generation thread ─► Hermes /v1/chat/completions (SSE)
                                                     │
                     suggestion::parse on every delta ─► suggestion-event ─► SuggestionPanel
```

The hook runs on the dispatcher thread but never blocks on Hermes: it updates state under a short lock and spawns the generation thread. See `docs/hermes.md` and `docs/context-management.md`.
