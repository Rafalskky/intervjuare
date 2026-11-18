# "Låt oss se": screenshot assistance (Milestone 5)

## Trigger

- **Voice:** a final candidate (microphone) segment whose whole content is
  "Låt oss se" or "Let's see", optionally with up to two filler words
  ("Okej, låt oss se.", "Let's see here"). Detection is local and
  deterministic (`src-tauri/src/voice_command.rs`); no model request is spent on
  deciding it. A sentence that only contains the phrase ("Låt oss se vad som
  händer om…") is normal dialogue.
- **Button:** "Capture screen" in the suggestion panel.
- The command segment is removed from the conversation, so it never reaches the
  model as dialogue. The transcript marks it as `command · screen`.

## Capture

`intervjuare-macos-bridge screenshot [pid]` (Swift, ScreenCaptureKit
`SCScreenshotManager`, macOS 14+):

- With the meeting application selected in audio setup (its process id), only
  that application's windows are captured. That is where a shared screen,
  coding task or diagram appears during an online interview.
- Otherwise, the main display is captured **excluding Intervjuare's own
  windows**.
- Longest side ≤ 1600 px, JPEG quality 0.72, no cursor.
- The image is written only to the helper's stdout. Rust keeps it in memory,
  attaches it to one model request (`input_image`) together with the full
  interview context, and drops it. Nothing is persisted (README: capture →
  analyse → discard).

## Permission

Screen capture needs **Screen & System Audio Recording**, the same permission
the interviewer audio already needs. Without it, the helper returns
`screen_permission_denied` (checked with the non-prompting preflight) and the
suggestion panel shows "Screen assistance is unavailable: …" with the System
Settings path. Transcription is unaffected.

## Model

"Model for Låt oss se" in settings (empty = the answer model). The answer
follows the normal anchors → answer → sources contract.

## Not verified yet

The real capture path (permission granted in the signed bundle, meeting window
selection, image quality with a real shared screen) has not been exercised
live. The Swift command compiles cleanly and returns the typed permission
error without prompting. The Rust side is tested with fixtures (large output,
timeout, error mapping, image attached to the request).
