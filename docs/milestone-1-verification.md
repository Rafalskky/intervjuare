# Milestone 1 implementation verification

Verified on macOS 27.0 arm64 (build 26A428) with Node 26.7.0, npm 11.19.0, Apple Swift 6.4 and Rust/Cargo 1.99.0. The active developer directory was `/Library/Developer/CommandLineTools`; full Xcode was not selected.

## Implemented and exercised

- `swiftc native/macos/Sources/IntervjuareMacOSBridge/main.swift ...`: passed without diagnostics. This compiles the real ScreenCaptureKit `.audio` and `.microphone` outputs, application filter, selected microphone and native error protocol using Command Line Tools only.
- whisper.cpp v1.8.2 was built locally with Metal, Accelerate and arm64 CPU support. A multilingual `ggml-small-q5_1.bin` model (181 MB) is project-local.
- Synthetic macOS voice smoke tests were converted to 16 kHz mono WAV and passed through that exact local whisper CLI/model. Swedish output was `Kan du berätta lite om dig själv och din erfarenhet av reakt?`; English output exactly matched `Can you tell me about yourself and your experience with React?`. This proves real local inference and bilingual routing, not live interview quality.
- `cargo test --manifest-path src-tauri/Cargo.toml --lib`: the final integrated run passed 20/20 in 0.12 seconds after the lifecycle, warning/PCM, native float32-layout and 16/24/48 kHz contract corrections. Deterministic tests prove that a concurrent manual Stop waits while startup holds the runtime transaction, can observe only the fully registered session, owns and reaps its reader, and leaves no runtime; the native reader cannot execute before its handle is registered; cancellation of an unreleased reader start is bounded and joinable; concurrent cleanup takes the session exactly once; and an obsolete terminal callback cannot take a replacement session.
- The confirmed reader-registration races were corrected by keeping the runtime mutex through reader creation, `JoinHandle` registration and complete session publication, with the reader blocked on a one-shot barrier. Only after the fully owned session is visible does Start release the runtime mutex and reader. Immediate native EOF/error therefore cannot run terminal cleanup before ownership is registered, and manual Stop cannot take a partially published session or strand the reader. Registration failure drops the unpublished process/session resources and closes the barrier; their existing RAII/bounded cleanup paths reap the children and worker. The Start return remains asynchronous acceptance, not proof of live audio; only a matching native `capturing` event establishes that.
- `npm run lint`: passed after limiting the project linter to owned frontend source; vendored upstream JavaScript is not linted as application code.
- `npm run test`: 5 passed, covering event-before-command-response replay, stale-session rejection, failed/repeated-start cleanup and browser/native runtime detection.
- `npm run build`: TypeScript and Vite passed, 21 modules transformed.
- `npm run bundle:debug` is now the reproducible supported debug-bundle command. It runs `npm run tauri -- build --debug`, then `node scripts/sign-macos-bundle.mjs src-tauri/target/debug/bundle/macos/Intervjuare.app`. That script applies the intended entitlements and hardened runtime, explicitly ad-hoc signs helper identifier `com.rafal.intervjuare.macos-bridge` and app identifier `com.rafal.intervjuare`, then requires deep strict verification to pass. The installed Tauri 2.11.1 schema provides pre-build/pre-bundle hooks but no post-bundle hook, so this explicit wrapper prevents a bare successful Tauri build from being mistaken for a verified final artifact.
- Bundle inspection found the Rust executable and Swift bridge, plus whisper CLI/model/dylibs under app resources. `Info.plist` contains `NSMicrophoneUsageDescription`, `NSScreenCaptureUsageDescription` and `NSAudioCaptureUsageDescription`.
- Before the wrapper repair, the current freshly rebuilt artifact failed `codesign --verify --deep --strict --verbose=2 .../Intervjuare.app` with exit 1 and `code has no resources but signature indicates they must be present`; the earlier valid-artifact claim was therefore obsolete. After the repair, a fresh `npm run bundle:debug` exited 0 and its required verification reported `valid on disk` and `satisfies its Designated Requirement`. An independent `codesign --verify --deep --strict --verbose=4` repeat exited 0. Current identities are app `com.rafal.intervjuare`, CDHash `2f90d737f5bf4204c4772c8acd7b04a4efa965d1`, and helper `com.rafal.intervjuare.macos-bridge`, CDHash `cceb9529691cef6c2692994deea5cf06128b8930`; both are ad-hoc with no Team ID, and `security find-identity -v -p codesigning` returned `0 valid identities found`.
- The exact post-repair bundled executable remained alive for a 15-second tracked startup probe, emitted zero stdout/stderr bytes, and only that verifier-owned process was terminated. The bundled helper returned `synthetic_harness_passed` with exit 0 and bundled `whisper-cli --help` exited 0. Capture was not started. These are packaging/startup checks, not live audio evidence or permission proof.
- Correction cycle 1 made native startup transactional, discarded native stderr instead of leaving an undrained pipe, added one idempotent bounded cleanup path for manual stop/app exit, interrupts and reaps whisper children on cancellation, and emits a terminal stopped state on native EOF/error. Every state/warning/transcript event now carries a unique session ID and the frontend rejects stale sessions.
- Each whisper inference now uses a unique `0700` directory with RAII removal across all return paths. Empty/failed final inference preserves a previous valid partial as a final revision. Partial scheduling is utterance-relative: first eligible at 0.9 seconds of captured audio, then after each additional 2.4 seconds.
- `otool -L` showed only `@rpath` whisper/ggml dependencies plus system libraries. The build now strips development source-tree rpaths and adds `@executable_path`. The CLI and six dylibs were copied alone to `/Users/rafalchorazewicz/.hermes/cache/scratch/intervjuare-runtime-relocated`; `DYLD_PRINT_LIBRARIES=1 .../whisper-cli -h` exited 0 and resolved all six whisper/ggml dylibs from that copied directory, not the project tree.
- Current safe verification also compiled the exact Swift source with `xcrun swiftc` and ran its synthetic harness successfully. A separate `swift build -c debug` attempt failed while linking the Package.swift manifest because the installed Command Line Tools could not resolve a `PackageDescription.Package` symbol; this is recorded as a tooling-path failure, not hidden as a pass. `npm run tauri -- build --debug` still compiled and bundled the native bridge successfully.
- The Swift Package Manager failure was reproduced unchanged. It is isolated from the supported Tauri build: `src-tauri/build.rs` invokes `swiftc` directly on the same source and does not consume `Package.swift`. No Xcode/Command Line Tools installation or configuration was changed.
- Static inspection found no default raw-audio history path, no leftover `intervjuare-inference-*` workspace and no later-milestone Hermes/provider/credential implementation in owned source. Per-inference WAV files exist only inside the private RAII workspace and are removed with it.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` could not run because the installed stable Rust toolchain has no `rustfmt` component. No component was installed or toolchain configuration changed. Compilation and all Rust tests passed.

## Unverified and blocking the milestone gate

No real microphone or selected meeting-application audio was recorded during implementation. This avoided capturing unrelated conversations/background apps and avoided interacting with TCC prompts. Therefore these required gates remain unverified:

1. user-granted microphone and Screen & System Audio Recording permission;
2. candidate-only phrase appears only as `candidate`;
3. remote-only phrase appears only as `interviewer`;
4. simultaneous speech is retained in both streams;
5. Swedish and English real-speech quality;
6. p50/p95 partial and final latency;
7. headphone and speaker-mode echo/contamination behavior.

Also unverified with real callbacks are native EOF/error cleanup under concurrent Stop, selected-app/device loss, permission/source/STT failures, gap/contamination reporting and event ordering across repeated live sessions. Unit/synthetic coverage is preparation, not evidence for those live paths.

## Exact owner action required

1. Quit and reopen the built Intervjuare app after the already owner-authorized ScreenCapture-only TCC reset, complete the macOS consent interaction, and report whether Refresh can discover the dedicated test application. Do not repeat the reset merely because this remains blocked.
2. Open a dedicated meeting/test-audio application that contains no unrelated conversation or private app audio, and select it in Intervjuare together with the intended microphone.
3. Connect headphones for the primary separation pass.
4. Provide short Swedish and English test speech for candidate-only, remote-only and simultaneous cases. A later speaker-mode pass requires separate explicit authorization because it intentionally tests acoustic contamination.
5. Keep the test source available for repeated Start/Stop and controlled application/device-loss checks.

During that owner-controlled run, record only source/session/state/revision/final flags and timing values. Compute capture-to-first-partial and last-phoneme-to-final p50/p95 without retaining transcript text or raw audio. Mark each gate passed, failed or blocked; do not infer live success from the synthetic harness.

Compile, unit tests, bundled startup and synthetic speech are not presented as live meeting verification. Milestone 2 must not begin until the explicit short live test above is performed and accepted.

## Deferred-live permission and failure-path evidence

The owner authorized a read-only permission check while away from home, but explicitly deferred recording. A standalone CLI probe used only `AVCaptureDevice.authorizationStatus(for: .audio)` and `CGPreflightScreenCaptureAccess()`; it did not request access, call `SCShareableContent`, start capture, modify TCC or interact with prompts. It returned:

```text
process=cli_probe
microphone=not_determined
screen_capture_preflight=not_authorized_or_undetermined
```

This is evidence only for the probe process. Screen preflight does not distinguish denied from not determined. It is not evidence that the app has or lacks either permission. Bundle inspection records `CFBundleIdentifier=com.rafal.intervjuare`; the ad-hoc app executable has code-signing identifier `intervjuare-9977dac4670ef8a3`, while the bundled helper that invokes AVFoundation and ScreenCaptureKit has the separate ad-hoc identifier `intervjuare-macos-bridge`; neither has a Team ID. Exact bundled-app/helper TCC attribution remains unverified because safely proving it requires launching a permission-sensitive bundled path, which was deferred with the live test.

Focused non-recording commands and results:

- `cargo test --lib bounded_source_queues_drop_oldest_without_relabeling -- --nocapture`: 1 passed, 0 failed.
- `cargo test --lib source_assignment_comes_only_from_native_tag -- --nocapture`: 1 passed, 0 failed.
- `cargo test --lib resampling_preserves_duration_and_speaker_queues_are_independent -- --nocapture`: 1 passed, 0 failed.
- `cargo test --lib fake_failing_process_reports_exit_without_hanging -- --nocapture`: 1 passed, 0 failed in 0.14 seconds.
- `cargo test --lib fake_hanging_process_is_cancelled_and_reaped -- --nocapture`: 1 passed, 0 failed in 0.30 seconds.
- Direct `swiftc` compilation of `main.swift`, followed by the compiled binary's `synthetic-harness`: returned `synthetic_harness_passed`.
- No `intervjuare-inference-*` workspace remained after the checks.

The confirmed static gap/conversion reporting defect is corrected. Rust forwards native `warning` records while preserving the native code/message/source and attaching the current reader session ID. It validates mono PCM against the explicit 16/24/48 kHz allowlist, caps callbacks at 48,000 frames/192,000 decoded bytes, and rejects missing source/timestamp, invalid base64/alignment and non-finite samples before selecting or mutating either resampler; invalid frames emit a typed non-sensitive `invalid_pcm` warning and are dropped without terminating either healthy source. Focused commands for warning forwarding, malformed continuation, unsupported tiny/extreme rates and channels, and oversized payloads passed. The regressions preserve an existing candidate resampler's rate, fractional phase and previous sample after every rejection, leave interviewer state untouched, and prove that a subsequent valid frame continues normally. The final full `cargo test --lib` passed 20/20 in 0.12 seconds and `cargo check` exited 0. `cargo fmt -- --check` remains unavailable because `cargo-fmt` is not installed; no component was installed. This deterministic protocol evidence does not replace real native callback/gap verification in the owner-controlled live run.

The final native correction validates ScreenCaptureKit ASBD metadata and converts supported little-endian float32 interleaved and non-interleaved callback layouts to mono before crossing into Rust. It bounds callbacks at 48,000 frames and 32 channels, bounds buffer-list allocation and arithmetic before loops, preserves `candidate`/`interviewer` identity, and permits only exact native rates of 16, 24 or 48 kHz. Rust now accepts those same bounded rates and independently resamples each source to 16 kHz. Direct Swift compile plus `synthetic-harness` passed planar/interleaved fixtures at all three rates and non-finite rejection; a Rust cross-boundary test passed both sources at all three rates. The full Rust suite passed 20/20 and `cargo check` passed. This addresses the confirmed boundary mismatch behind the earlier live `invalid_pcm` observation, but the observation predates the correction and cannot count as a post-correction pass.

The confirmed terminal Stop abort was also corrected. The bridge no longer calls `FileHandle.synchronizeFile()` on stdout; JSON lines use a POSIX write loop with `SIGPIPE` ignored, bounded payloads, `EINTR` retry, and nonthrowing broken-pipe failure. Against both the freshly compiled helper and rebuilt bundled helper, an open stdout pipe produced `synthetic_harness_passed` and exit 0, while a parent-closed read end produced exit 0 with no SIGABRT/SIGPIPE. This is capture-free terminal-path evidence; it does not count as three live Start/Stop cycles.

The owner-controlled live run must use one dedicated non-private application, headphones and the selected microphone. For each Swedish and English candidate-only, remote-only and simultaneous case, collect at least five utterances. Record only session/source/state, segment/revision/final flags and timestamps. Use first partial `latencyMs` for capture-to-first-partial. Mark the last phoneme with a verifier timestamp and subtract it from final `emittedAtMs` for finalization latency. Report sample count, median (p50), nearest-rank p95 and maximum. Then perform three Start/Stop cycles plus controlled selected-application and microphone loss. Do not retain transcript text or raw audio. Speaker-mode contamination remains separately authorized because it intentionally introduces acoustic leakage.

## Consolidated acceptance matrix

| Gate | Explicit status | Supporting evidence and limit |
| --- | --- | --- |
| Bounded buffers | Passed, synthetic/harness-only | Swift `queueDepth=3`, bounded callback conversion and per-source Rust queue cap/drop-oldest tests; no live overload sample. |
| Cancellation | Passed, synthetic/harness-only; live-native blocked | Hanging decoder cancellation/reaping and transactional cleanup fixtures passed; no cancellation during real capture. |
| Repeated Start/Stop | Passed, deterministic fixture only; live-native blocked | Lifecycle ownership and closed-reader helper regressions passed; three live cycles not run. |
| Revisions | Passed, synthetic/harness-only; live-native blocked | Rust final/partial revision fixtures and frontend replacement semantics passed; real speech revisions unobserved. |
| Source assignment | Passed, synthetic/harness-only; live-native blocked | Native-tag/no-relabel and independent queue/resampler tests passed; candidate/remote isolation unobserved. |
| Event ordering | Passed, deterministic fixture only; live-native blocked | Early replay, stale-session rejection and atomic publication tests passed; live callback ordering unmeasured. |
| Permission failures | Synthetic/harness-only partial; live-native blocked | Typed partial-discovery contract passed. Owner-authorized ScreenCapture-only reset exited 0, but post-reset consent and bundled attribution are unobserved. |
| Source failures | Synthetic/harness-only partial; live-native blocked | Typed source loss/discovery fixtures exist; controlled app/device loss not run. |
| STT failures | Synthetic/harness-only partial; live-native blocked | Nonzero/empty inference fixtures and `stt_failed` path passed; real failure presentation not run. |
| Gap reporting | Synthetic/harness-only partial; live-native blocked | Typed warning and malformed PCM forwarding passed; real callback discontinuity unobserved. |
| Contamination reporting | Synthetic/static partial; live-native blocked | Contract requires observable signal and no relabeling; real headphones/speaker leakage run not performed. |
| No default raw-audio persistence | Passed, static/harness-only | Only private `0700` RAII inference workspaces contain transient WAV; checks found no leftover `intervjuare-inference-*`. Absolute crash cleanup is not proven. |
| Candidate-only microphone | Blocked, live-native | Zero post-correction controlled microphone-only samples. |
| Remote-only selected-app audio | Blocked, live-native | Zero post-correction controlled selected-app samples; post-reset discovery unobserved. |
| Simultaneous speech | Blocked, live-native | No controlled overlapping speech run. |
| Swedish latency and quality | Blocked, live-native | Zero post-correction live samples; no n/p50/p95/max. Synthetic `reakt` result proves inference routing only. |
| English latency and quality | Blocked, live-native | Zero post-correction live samples; no n/p50/p95/max. Exact synthetic phrase proves inference routing only. |
| Bundle artifact/startup | Passed, non-capture packaging evidence | `npm run bundle:debug`, independent deep strict verification, helper harness, Whisper help and 15-second startup passed. This is not a live gate. |

Latency instrumentation exists but has no live measurements. Transcript events expose `capturedAtMs`, `emittedAtMs` and `latencyMs = emittedAtMs - capturedAtMs`. For each Swedish/English source case, collect at least five utterances; use first-partial `latencyMs` for capture-to-first-partial, subtract an owner-marked last-phoneme timestamp from final `emittedAtMs`, and report n, median p50, nearest-rank p95 and maximum. Scheduler values of 0.9 seconds for first eligibility, 2.4 seconds for later windows and about 850 ms of silence for finalization are configuration, not measurements.

The project directory is not a Git repository, so historical commits, publishing, unrelated installs and an exact baseline diff cannot be independently proven. This repair intentionally adds only `bundle:debug`/`bundle:sign:debug` in `package.json`, `scripts/sign-macos-bundle.mjs`, and updates `docs/audio-pipeline.md` plus this milestone evidence file. It adds no dependency and changes no app identifier, helper identifier, model, provider, credential, capture state, permission prompt, TCC setting, owner process, commit or publication. The coordinator's earlier owner-authorized ScreenCapture-only reset is retained as external evidence and was not repeated here.

## Swedish quality and Stop-finalization correction (2026-10-09)

The first owner-controlled post-correction candidate-microphone Swedish sample reached live capture, stayed tagged as ME, and reported no `invalid_pcm`. It did not pass quality: 98 normalized rendered words versus a 106-word reference, approximate WER 32.1%, and a truncated ending. Six visible final latencies had median 1646.5 ms, minimum 1566 ms and maximum 2089 ms. No transcript text or raw audio was persisted.

Code inspection found that the transcriber loop used `while !stop`, so explicit Stop could bypass both queue draining and the final-inference branch. Even if reached, the shared flag immediately cancelled Whisper. The implementation now sends native Stop before setting the worker flag, drains already-bounded frames, runs one final decode for an in-progress utterance, and then joins/reaps workers. Only this explicit-Stop final decode ignores the shared flag, and it has a bounded eight-second deadline per source (`SERVER_FINAL_TIMEOUT`; curl `max-time` 5 s, non-final wrapper max-time + 5 s) that kills and reaps a stalled Whisper process; ordinary inference remains immediately cancellable. Because the shared server serializes the two trailing finals, worst-case serialized Stop is on the order of two 8 s finals plus native/server teardown, not eight seconds total; the trailing final fallback itself is never weakened. Added real-process fixtures prove timely completion and deadline termination/reaping, while state fixtures prove Stop with trailing speech remains processable, queued audio is drained before finalization, final revision fallback is monotonic, and candidate/interviewer revision state remains independent. The owner sample did not include an explicit Stop, so it is not evidence that this lifecycle defect caused that sample's truncated ending.

The former RMS-only frame gate admitted a deterministic sparse high-amplitude impulse representing typing-like noise. The corrected gate requires RMS above 0.008 plus at least 8% active samples at magnitude 0.018 or higher. The sparse impulse is rejected and a sustained voiced waveform is accepted. This is deterministic noise hardening, not a claim that all real typing or ambient noise is rejected.

The existing bundled multilingual small Q5_1 model remains unchanged. The only other discovered model-named files were 575,451-byte upstream test fixtures, so there was no already-available production alternative suitable for a Swedish quality/latency comparison. Explicit Swedish/English routing remains intact. A renewed owner-controlled sample is still required to measure actual Swedish WER, trailing-word retention, final latency, and live Stop behavior.
