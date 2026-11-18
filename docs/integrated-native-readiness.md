# Integrated native readiness

Scope: automated integration and bundle verification only. No audio capture was started, no permission prompt was requested or clicked, no TCC setting was changed, and no live transcript was produced. Permission-requesting code is present, but actual user grants remain unverified.

## Build artifact

- App: `/Users/rafalchorazewicz/Documents/Intervjuare/src-tauri/target/debug/bundle/macos/Intervjuare.app`
- Bundle ID / version: `com.rafal.intervjuare` / `0.1.0`
- Architecture: arm64 debug bundle
- Existing owner process: PID 22264, launched 2026-10-07 20:24:38 +0200.
- Fresh bundle timestamp was later than the running process. The owner process was not terminated or replaced, so the updated bundle requires a restart before live validation.

## Integrated checks

Passed:

- `npm run test`: 5/5 frontend tests passed, including browser-runtime rejection and native Tauri invoke detection.
- `npm run lint`: passed.
- `npm run build`: TypeScript and Vite production build passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --lib`: 19/19 passed.
- `cargo check --manifest-path src-tauri/Cargo.toml`: passed before final integration.
- Swift bridge compile plus `synthetic-harness`: passed with `synthetic_harness_passed`.
- `npm run tauri -- build --debug`: passed and produced the app path above.
- Bundled `whisper-cli --help`: ran successfully against the bundled dylibs.
- Final helper synthetic harness: passed.

## Confirmed integration corrections

Fresh bundle inspection found that the old resource destination produced only the app executable and native bridge; the Whisper model, CLI, and dylibs were absent. Corrected:

- `src-tauri/tauri.conf.json`: bundle prepared runtime files into `Contents/Resources`.
- `src-tauri/src/lib.rs`: bundled artifact lookup now checks `../Resources`.

The fresh bundle now contains:

- `Contents/Resources/ggml-small-q5_1.bin`
- `Contents/Resources/whisper-cli`
- `Contents/Resources/libwhisper.1.dylib`
- `Contents/Resources/libggml.dylib`
- `Contents/Resources/libggml-base.dylib`
- `Contents/Resources/libggml-cpu.dylib`
- `Contents/Resources/libggml-blas.dylib`
- `Contents/Resources/libggml-metal.dylib`

## Identity, entitlements, and Info.plist

Final artifact was explicitly re-signed after bundling for verification because Tauri's ad-hoc signing pass replaced the bridge's stable identifier. Verified final state:

- Outer app identifier: `com.rafal.intervjuare`.
- Native bridge identifier: `com.rafal.intervjuare.macos-bridge`.
- Native bridge signature: ad-hoc with hardened runtime.
- Native bridge entitlement: `com.apple.security.device.audio-input = true`.
- `codesign --verify --deep --strict --verbose=4 <app>`: passed after final explicit re-sign.
- `NSMicrophoneUsageDescription`: selected microphone is used for local candidate transcription.
- `NSScreenCaptureUsageDescription` and `NSAudioCaptureUsageDescription`: selected meeting-application audio is captured for local transcription.

The explicit final re-sign commands were:

- `codesign --force --sign - --identifier com.rafal.intervjuare.macos-bridge --options runtime --entitlements src-tauri/Entitlements.plist <app>/Contents/MacOS/intervjuare-macos-bridge`
- `codesign --force --sign - --identifier com.rafal.intervjuare --options runtime --entitlements src-tauri/Entitlements.plist <app>`

Packaging limitation: source configuration intentionally does not enable Tauri's automatic ad-hoc signing because that pass rewrites the bridge identifier to a hash-derived value. A clean rebuild therefore needs the two explicit re-sign commands above until bundle signing is moved into a stable post-bundle mechanism.

## Frontend runtime behavior

Browser preview was checked through the running Vite server at `[::1]:1420` without invoking native commands. The served frontend includes the runtime guard and exact warning:

`Audio capture requires the Intervjuare macOS desktop app. The Vite browser preview has no Tauri backend, so it cannot discover or capture audio sources.`

The native desktop path is separately covered by the passing test that recognizes `window.__TAURI_INTERNALS__.invoke`. The already-running bundled app had an active WebKit process and native bundle identity, but it predates this build. It was left untouched; actual refreshed WebView IPC and safe source discovery require owner restart. Source discovery was not invoked because ScreenCaptureKit enumeration can interact with macOS permission state.

## Remaining live gates

This artifact does not approve Milestone 1. Owner-controlled validation still needs:

1. Quit/restart the existing Intervjuare process and launch the fresh bundle above.
2. Confirm the refreshed desktop UI uses Tauri IPC and can discover sources.
3. On explicit Start only, respond to any microphone/system-audio permission prompt and confirm the resulting Privacy & Security attribution.
4. Connect AirPods and confirm the selected microphone appears and captures correctly.
5. With a controlled test application, confirm Chrome/Safari/Teams application-level discovery; tab isolation is not expected.
6. Run the controlled candidate/interviewer/simultaneous Swedish/English audio matrix and record latency, source assignment, revisions, repeated start/stop, cancellation, and failure behavior without storing transcript text or raw audio.

## Follow-up after first owner-controlled capture

The owner reported that the app reached `LIVE`, emitted `invalid_pcm`, and produced some low-quality Swedish transcription (`hund` was heard as `Hörn`). This is an observation, not a measured quality or latency result, and the live matrix remains unapproved.

The confirmed PCM defect was in the Swift bridge: it concatenated raw `AudioBufferList` storage while the Rust boundary deliberately accepts only little-endian float32 mono frames. ScreenCaptureKit may deliver float32 at a device-native rate, with multiple interleaved channels or one non-interleaved buffer per channel. The bridge now validates the actual ASBD, converts supported float32 interleaved/non-interleaved layouts to mono without changing source identity, publishes `channels: 1`, and leaves sample-rate conversion to the existing per-source Rust resamplers. Empty, integer, big-endian, malformed, and non-finite buffers remain rejected rather than being mislabeled.

Post-correction automated evidence:

- Swift compile and synthetic harness passed, including deterministic 48 kHz stereo interleaved-to-mono normalization.
- `cargo test --manifest-path src-tauri/Cargo.toml --lib`: 19/19 passed.
- `cargo check --manifest-path src-tauri/Cargo.toml`: passed.
- `npm run tauri -- build --debug`: passed and rebuilt the debug app.
- The final bridge and outer app were explicitly re-signed with the stable identifiers above.
- Deep strict code-sign verification passed; bundled bridge harness passed; bundled `whisper-cli --help` passed.

No capture, restart, prompt, TCC change, transcript recording, raw-audio storage, or model change was performed during this correction. A renewed owner-controlled live run is still required to establish whether `invalid_pcm` is gone and to measure Swedish/English quality and latency.
