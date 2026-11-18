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
