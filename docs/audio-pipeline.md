# Phase 1 audio and transcription contract

This document is the implementation contract between the native macOS capture worker and the Rust transcription worker. It does not change the canonical architecture in `README.md` or `docs/architecture.md`.

## Decisions

- Capture with one application-scoped ScreenCaptureKit `SCStream` on macOS 13 or newer.
- The selected microphone is emitted only as `candidate`. The selected running application audio is emitted only as `interviewer`.
- Rust trusts the native source tag and never classifies, infers, swaps or relabels speakers. Missing application audio is an error or gap, never a reason to copy microphone samples into the interviewer path.
- Keep independent candidate and interviewer queues, resamplers and utterance state, but inference compute is intentionally serialized by the one shared server. Queues, resampling and utterance tracking never relabel or drop the other source; one source can add bounded inference wait to the other through the shared gate/server, never relabeling or dropping it.
- Transcribe locally with project-vendored whisper.cpp 1.8.2 and the official multilingual `large-v3-turbo-q5_0` GGML model (`assets/models/ggml-large-v3-turbo-q5_0.bin`, 574,041,195 bytes). Language is selected explicitly as `sv` or `en`; `.en` models are not valid for Swedish and no second classifier is introduced. Inference runs through exactly one persistent `whisper-server` per capture session, shared by the candidate and interviewer transcribers and launched before capture workers with loopback-only binding, an ephemeral port, a per-session request path, `--no-timestamps`, `--no-context` and `--audio-ctx 640`. Requests go over macOS `/usr/bin/curl` (argument array, no shell) as multipart file plus explicit language, `response_format=json`, `no_timestamps=true` and an empty prompt, with bounded timeouts and a transient owner-only response file. The retired `small` Q5_1 and full unquantized `large-v3` source assets are kept untouched for comparison but are no longer selected or bundled.
- Native audio and inference files are transient. No raw audio is interview history.

## Source discovery and selection

Rust calls the sidecar as `intervjuare-macos-bridge sources`. The sidecar returns exactly one JSON line:

```json
{
  "type": "sources",
  "microphones": [{ "id": "device-uid", "name": "Microphone", "bundleId": null }],
  "applications": [{ "id": "1234", "name": "Meeting App", "bundleId": "com.example.meeting" }]
}
```

Source discovery is partial rather than all-or-nothing. Microphones are enumerated independently of ScreenCaptureKit. If application enumeration fails, the response keeps the real microphone list, returns an empty `applications` array, and includes `applicationDiscoveryError` with code `screen_audio_permission_denied` and a metadata-only message. Rust forwards that structure and React keeps the microphone selection visible while showing the application-specific error. It never inserts a mock application or reports permission success.

`microphone.id` is an `AVCaptureDevice.uniqueID`. `application.id` is the currently running process ID exposed by ScreenCaptureKit. The UI must require one explicit value from each list before Start. Source IDs are snapshots: Rust must rediscover and require reselection when a device disappears or an application exits/restarts. There is no default “all system audio” source.

Source discovery may itself require Screen & System Audio Recording permission. Permission prompts and System Settings changes are user actions; code must not modify TCC or report permission success before native callbacks prove it.

## Rust-to-Swift control protocol

Control is newline-delimited JSON over stdin/stdout. Protocol names and fields are camelCase.

Start a fresh sidecar process with the `capture` argument, then write one request:

```json
{
  "command": "start",
  "microphoneId": "device-uid",
  "applicationId": "1234"
}
```

Stop by writing the literal line `stop`, closing stdin, or terminating the owned process during rollback/shutdown. Start is transactional: failure to acquire stdin/stdout or write the request must kill and reap the child. Native stderr must be inherited, drained or discarded; it must never remain an undrained pipe.

Each Start creates a unique Rust `sessionId`. The sidecar does not generate it; Rust attaches it to every UI-facing state, warning and transcript event. A Stop for no active session is an idempotent success. Starting while a session is active is rejected. A stopped session ID is never reused.

Commands and asynchronous events are independent. The caller must accept the `starting`/`capturing` event before or after the Start command response. React must install listeners before invoking Start, retain early events for the returned session, and filter all later events by the active `sessionId`. The live pipeline must not use the Phase 0 one-shot development handshake timer or assume event-before-response ordering.

## Swift-to-Rust event protocol

The sidecar emits one JSON object per stdout line. No transcript text is produced in Swift.

State:

```json
{ "type": "state", "message": "capturing" }
{ "type": "state", "message": "stopped" }
```

PCM:

```json
{
  "type": "pcm",
  "source": "candidate",
  "sampleRate": 48000,
  "channels": 1,
  "capturedAtMs": 1791367000000,
  "samples": "base64-float32-little-endian"
}
```

`source` is mandatory and is assigned only from `SCStreamOutputType`: `.microphone` becomes `candidate`; `.audio` becomes `interviewer`. Any other output type is ignored. Swift accepts only bounded little-endian float32 callbacks, caps a callback at 48,000 frames and 32 channels, converts interleaved or planar input to mono, and preserves an exact supported native rate of 16, 24 or 48 kHz. Rust accepts only those three rates and mono data, then independently resamples each source to 16 kHz. Unsupported rates, layouts, counts, overflow and non-finite samples are dropped with metadata-only warnings before source resampler state changes. `capturedAtMs` is the callback observation time on the sidecar clock and is for end-to-end age metrics; it is not a shared sample clock. Sample order within each source is authoritative. Rust maintains separate fractional resampler phase/history and separate accumulated sample counts for the two clocks.

Errors:

```json
{
  "type": "error",
  "code": "application_unavailable",
  "message": "The selected meeting application is no longer running."
}
```

Required start/runtime failure codes are:

- `microphone_permission_denied`
- `screen_audio_permission_denied`
- `microphone_unavailable`
- `application_unavailable`
- `capture_start_failed`
- `capture_failed`
- `source_discovery_failed`
- `invalid_request`
- `unsupported_macos`

An error is terminal for that capture session. Native EOF before a deliberate Stop is also terminal. Rust atomically marks the session stopping, cancels both decoder workers, sends Stop when possible, kills/reaps bounded subprocesses, clears the active runtime and emits a terminal UI state. This path must race safely with manual Stop and app/window exit.

## Buffering, gaps and contamination

Swift sets `SCStreamConfiguration.queueDepth` to 3 and serializes callback output. Rust has one bounded queue per source, each capped at 80 native frames AND 256_000 samples (16 s at 16 kHz, `PCM_QUEUE_MAX_SAMPLES`). A max legal native frame decodes to at most 48_000 samples, so the frame cap alone could hold 80 × 48_000 = 3.84M samples (~15.4 MiB f32) per source; the sample cap tightens this to 256_000 samples (~1 MiB) per source. Normal 20 ms callbacks carry ~320 samples (~25.6k samples at full depth), so the sample cap never triggers in normal operation — it only bounds adversarial max-size bursts, possibly evicting more than one oldest frame with counters for every eviction. Overflow drops the oldest frame(s) only on the affected source and emits:

```json
{
  "sessionId": "session-...",
  "code": "audio_backpressure",
  "message": "Transcription fell behind and dropped the oldest transient audio frame.",
  "source": "interviewer"
}
```

Native invalid/not-ready buffers, conversion failures, timestamp regressions, observed discontinuity attachments, queue drops and source loss must be surfaced as typed source-specific warnings rather than silently ignored. The implementation may use `audio_gap`, `audio_backpressure`, `source_lost` and `possible_contamination`; each warning includes `sessionId` and, when known, `source`.

### Temporary diagnostic `audio_backpressure` suffix (metadata-only, one live run)

This instrumentation is temporary/diagnostic. It is metadata-only and does not fix backpressure or prove live reliability.

The existing `audio_backpressure` code, message prefix (`Transcription fell behind and dropped the oldest transient audio frame.`), affected-`source` semantics, drop-oldest policy, counters, finalization, and inference order/deadlines are unchanged. No diagnostics are persisted to disk. The same session-scoped `capture-warning` path carries one compact, bounded, stable suffix for paste-up:

```text
Transcription fell behind and dropped the oldest transient audio frame. [diag src=interviewer depth=80/80 cap=80 q_ms=... in_n=... in_ms=... drop_age_ms=... drop_ms=... drop_n=... drop_total_ms=... cum_drop_n=... cum_drop_ms=... cb_delta_ms=... other_depth=... other_q_ms=... phase=... phase_ms=... other_phase=... other_phase_ms=... gate_ms=... infer_ms=... kind=... other_gate_ms=... other_infer_ms=... other_kind=...]
```

Fields are integers/enums only (`phase` is `idle`/`waiting_gate`/`inference`; `kind` is `partial`/`final`); genuinely unavailable values render as `unknown`, never invented zero. `drop_ms` is the first evicted frame only (stable identity); `drop_n` / `drop_total_ms` are the exact evicted-frame count and total duration for THIS push only, not inferred historical loss. `cum_drop_n` / `cum_drop_ms` are queue-lifetime cumulative totals after THIS push. Durations derive from normalized 16 kHz sample counts and sidecar `capturedAtMs` deltas; STT phase elapsed comes from a short Rust monotonic measurement. Queue snapshots are bounded `O(capacity)` with no audio copies, and the inference-gate/queue locks are held only briefly. The per-source STT phase always returns to `idle` via an RAII guard on success, cancellation, and errors. No transcript text, audio bytes/sample values, WAV/JSON paths, raw payloads, or candidate data are included.

Application scoping is not echo cancellation. Speaker playback can leak into the microphone, and a meeting app can emit sidetone, notifications or shared media. Headphones are the supported baseline. “Possible contamination” may be reported only from an observable signal (for example correlated overlapping source energy); absence of a warning is not proof of clean separation. Audio is never reclassified to hide contamination.

## Rust lifecycle

The UI-facing capture states are:

```text
stopped -> starting -> capturing -> stopping -> stopped
                       \-> failed -> stopped
starting ----------------^ failed -> stopped
```

The current UI schema may collapse `stopping` and `failed` into a terminal `stopped` event plus a typed warning, but Rust orchestration must still enforce those internal states. Start succeeds only by returning the new `sessionId`; `capturing` is proven only by the native state event. Stop is bounded and idempotent. A native child that does not exit within three seconds is killed and reaped. Decoder children are retained, cancelled, killed and reaped; worker joins must not wait on an unmanaged `Command::output()`.

Cancellation caused by failure or replacement discards queued, non-final work for the cancelled session. An explicit user Stop first asks native capture to stop, then each source drains its already-bounded Rust queue and finalizes any in-progress utterance before its worker exits. Every trailing request is capped by the remaining aggregate drain budget: finals may ignore the shared Stop flag only up to `min(8 s per-final, remaining drain budget)` (`SERVER_FINAL_TIMEOUT` capped by the aggregate drain deadline), and non-finals are capped at `min(10 s wrapper, remaining drain budget)` — no request outlives the budget, including one started just before expiry (kill/reap via the same managed curl path, no deadlock). Once draining, partial snapshots are skipped (never inferred — superseded work must not extend the drain) while every queued final is decoded or reported. Finite aggregate Stop (A1/B1 correction): GracefulDrain arms a 14 s dispatcher drain deadline (`STOP_DRAIN_BUDGET`) at request time. The dispatcher checks expiry before starting each new inference and reports every queued final not decoded with a typed `stt_final_lost` warning instead of silently discarding it. Total Stop is therefore bounded by ~3 s native child + ~14 s drain + ~3 s server teardown ≈ ~20 s, never 8 s × queue length (8 finals × 8 s ≈ 64 s + 3 s would have been the old unbounded worst case) and never extended by a 10 s non-final wrapper past expiry. If a final stalls past its capped deadline, Rust kills and reaps it via the same managed curl path (no deadlock), emits the typed STT warning when no prior hypothesis exists, or finalizes the last valid partial through the normal fallback, which is never weakened. Ordinary (non-draining) non-final inference remains immediately cancellable. The three-second native child bound and bounded queues remain unchanged. No event from a previous session may update a new session. Per-inference directories must be unique, owner-only (`0700`) and removed by an RAII guard on success, write/spawn/exit/read/parse failure and cancellation. Oversized server responses are rejected by metadata pre-check plus `File::take(LIMIT+1)` before JSON parse, with curl `--max-filesize 262144` as defense-in-depth; payload content is never logged.

## Decoupled ingestion and dispatch (audio_backpressure fix, 2026-10-11)

Live evidence proved at least one oldest normalized PCM frame dropped at
queue capacity (80/80, ~1600 ms) while the candidate transcriber spent
4113 ms in the inference phase. Root cause: each per-source transcriber
loop popped its PCM queue and then blocked synchronously in server
inference (WAV encode + curl), so dequeue stalled for the whole request
and the reader-side queue overflowed with drop-oldest. A second defect
compounded it: end-of-utterance silence was measured with wall-clock
`Instant::now()`, so a multi-second inference stall masqueraded as
850 ms of silence and could finalize speech prematurely.

The fix separates continuous PCM drain/VAD/utterance assembly from
blocking server inference:

- The reader thread still pushes normalized PCM into the per-source
  `BoundedFrames` queue (cap stays 80 frames, not inflated) and never
  blocks on inference.
- One ingestion worker per source drains its queue continuously, runs the
  existing VAD gate and partial/final schedule in `UtteranceSegmenter`,
  and submits bounded audio snapshots to one session-scoped
  `DispatchQueue`. Ingestion never blocks on inference and never holds a
  lock during server work.
- One dispatcher worker owns all blocking inference (WAV encode + curl
  with the unchanged 5 s max-time / 8 s trailing-final bounds) and emits
  transcript events with unchanged monotonic revision, segment-ID,
  speaker-label, `sv`/`en` routing and final-immutability semantics. It is
  the sole inference submitter, matching the shared server's internal
  serialization with exactly one in-flight request.
- Silence is measured on the ingested captured clock
  (`latest_captured_at_ms - last_voice_captured_at_ms`); wall-clock stall
  time can no longer finalize speech.
- Partial snapshots for the same (speaker, segment) coalesce to the
  latest bounded entry; finals are never coalesced away and evict the
  oldest pending partial when the queue is full.

Named finite budgets (all in `src-tauri/src/lib.rs`):

```text
PCM_QUEUE_CAPACITY              80 frames per source (unchanged)
PCM_QUEUE_MAX_SAMPLES           256_000 samples per source (16 s at 16 kHz)
MAX_UTTERANCE_SAMPLES           192_000 samples (12 s at 16 kHz, utterance assembly;
                                hard-capped at +1 max frame = 240_000 in release)
DISPATCH_MAX_JOBS               8 pending snapshots, both sources shared
DISPATCH_MAX_SAMPLES            528_000 samples (33 s at 16 kHz, pending snapshots;
                                per-source max 288k + one full max final 240k)
DISPATCH_MAX_JOBS_PER_SOURCE    6 jobs per source (leaves >= 2 for the other source)
DISPATCH_MAX_SAMPLES_PER_SOURCE 288_000 samples per source (18 s; shared budget still
                                reserves one full 240k max final for the other source)
STOP_DRAIN_BUDGET               14 s dispatcher drain (3 + 14 + 3 ≈ 20 s total Stop)
IDLE_FINALIZE_MS                1_200 ms ingestion-idle fallback (850 ms gate + margin)
```

Full-system worst-case RAM (explicit, finite): ~2 × (1 MiB PCM + 0.96 MiB
utterance) + ~2.06 MiB dispatch pending + ~0.94 MiB in-flight ≈ 7 MiB of
f32 audio plus Vec overhead. Every audio-holding stage has a sample cap
enforced in release builds; no stage is frame-count-only.

When repeated prolonged stalls exceed the dispatch bound, submit fails
with typed metadata-only overload (`code: stt_overload` for shed partials
on the existing `capture-warning` path: shed kind/segment, pending jobs/ms,
no audio or transcript content). A shed ACCEPTED FINAL is never a plain
overload: ingestion records honest lost-final counters, emits
`code: stt_final_lost` with the shed segment, queue depth, and cumulative
lost finals + lost ms (metadata-only, actionable, restart guidance), and
deterministically terminally fails the session via Cancel so capture is
never presented as healthy while losing speech (M2). Partials may be
shed/coalesced when superseded; finals never vanish without terminal
failure. The React contract renders warning codes generically; the session
gate additionally lets `stt_final_lost` through after the terminal stopped
state, and the UI keeps the terminal warning sticky with its cumulative
summary instead of overwriting it (M6). Stale-session filtering by
`sessionId` is unchanged.

Fairness at capacity (M3, corrected): per-source reservation caps each source at 6
jobs / 288_000 samples, and the shared budget (528k = 288k + one full 240k
max final) additionally reserves one full max final per source even when the
other source sits at its own cap — a nominal 12 s final (192k) and a max
final (240k) are never rejected while the other source is capped. A final evicts the oldest pending partial —
preferring the flooder's own partial — to make room; finals are never
evicted for another final, and an all-finals-full queue (own or shared)
fails explicitly attributed to the flooder's own segment. A candidate flood therefore cannot shed an interviewer
final while the interviewer is under its reservation.

Utterance bound without silent loss (D correction): if assembly ever
exceeds the 240k hard cap, it splits losslessly — the capped prefix emits
as a bounded final and the remainder is retained as a new segment — instead
of truncating accepted speech. A last legal 48k frame straddling the 12 s
boundary therefore lands whole in one final; no accepted sample is silently
discarded and a split prefix is never counted as a successful full final of
the whole utterance.

Stop/Cancel ordering: graceful Stop drains queued PCM, submits the
trailing final, and the last exiting ingestion worker closes the dispatch
queue (RAII, also on panic); the dispatcher drains trailing finals within
the 14 s budget — finals capped at min(8 s, remaining), non-finals at
min(10 s, remaining), draining partials skipped, every undecoded
final reported with the session-scoped cumulative `Lost finals so far`
summary — before the owned server is terminated, preserving the
finite ~20 s total teardown bound. Drain expiry atomically closes the queue
together with the report, so a final submitted just after expiry fails
closed and is reported via the shared tracker instead of sitting
unconsumed. Cancel/EOF/app exit aborts promptly
(remaining queued finals each reported as cancelled — never mislabelled as a
Stop-budget expiry — with the same cumulative counters, non-final work
discarded as before) and reaps owned children; all workers join before a
new session can start, so no stale old-session events leak into a restart.
A saturated accepted final terminally fails the session AND stops the owned
native child, so a reader blocked on a live child with no stdout sees EOF
and runs the full idempotent terminal cleanup instead of capturing forever.

Idle fallback without callbacks (M4): ScreenCaptureKit delivers PCM only
when audio callbacks fire; sustained silence produces no callbacks, so the
captured clock cannot advance and no frame arrives to trigger the 850 ms
silence final. When the PCM queue stays continuously empty for 1_200 ms —
measured on the ingestion thread's own monotonic clock, never the
inference wall clock — ingestion forces exactly one trailing final.
Backed-up frames (non-empty queue) never trigger it, and force_final
clears the utterance so no duplicate emits after frames resume. Primary
captured-clock VAD is unchanged.

Honest limits (unchanged by this fix): removing the known ingestion stall
does not explain WHY one live inference lasted 4.1 s (cause unknown;
synthetic sub-second medians are not a live bound), and no zero-drop
behavior is promised under arbitrarily long stalls with bounded memory.
Live p50/p95 first-partial and last-phoneme-to-final latencies still
require the owner-controlled live gate. The `audio_backpressure` warning
is kept as the safety net, not suppressed.

## Transcript event contract

Rust emits `transcript-event`:

```json
{
  "sessionId": "session-...",
  "segmentId": "interviewer-1",
  "revision": 2,
  "speaker": "interviewer",
  "text": "Kan du berätta lite om dig själv?",
  "finalSegment": true,
  "startedAtMs": 1791367000000,
  "endedAtMs": 1791367002200,
  "capturedAtMs": 1791367002200,
  "emittedAtMs": 1791367003100,
  "latencyMs": 900
}
```

Rules:

- A source gets its own monotonically increasing utterance number. `segmentId` is stable for all hypotheses of that utterance and unique within `sessionId`.
- Revisions start at 1 and strictly increase only when an event is emitted.
- `speaker` is immutable for a segment and remains capture-derived.
- A partial has `finalSegment=false`. A final has `finalSegment=true`; no later revision may mutate it.
- A changed hypothesis replaces the previous text for the same key `(sessionId, segmentId)`; it is not appended as another turn.
- Equal text may still receive the final revision. If final decoding is empty or fails after a valid partial, finalize the last valid text. If no valid text ever existed, emit a typed STT warning rather than inventing text.
- `startedAtMs` and `endedAtMs` describe the captured utterance interval. `emittedAtMs` is wall-clock emission time. `latencyMs = emittedAtMs - capturedAtMs` is capture-to-event age, not word accuracy or end-of-turn latency.

The initial scheduler may make the first partial eligible after 0.9 seconds of captured utterance audio, then after each additional 2.4 seconds, with finalization after about 850 ms of silence or 12 seconds maximum utterance length. The 850 ms silence gate requires continuous callbacks carrying the captured clock; when the device stops delivering callbacks entirely (silence produces no SCStream callbacks), the ingestion-thread-only 1_200 ms idle fallback finalizes instead (M4). Frame admission requires both RMS above 0.008 and at least 8% of samples at magnitude 0.018 or greater. This remains a deterministic lightweight gate rather than a neural VAD, but unlike the former RMS-only gate it rejects sparse high-amplitude impulses such as the typing fixture. These are tunable implementation values, not performance claims. Measure capture-to-first-partial and last-phoneme-to-final separately.

## Model and tooling record

Current repository assets:

- whisper.cpp source/build reports version `1.8.2`; upstream provenance is `https://github.com/ggml-org/whisper.cpp`.
- `assets/models/ggml-large-v3-turbo-q5_0.bin` is the official multilingual Whisper `large-v3-turbo-q5_0` GGML conversion (574,041,195 bytes, SHA-1 `e050f7970618a659205450ad97eb95a18d69c9ee`). The asset is staged/bundled locally; no runtime download or cloud STT is required. `src-tauri/build.rs` (`MODEL_NAME`) is the single source of truth and exposes `INTERVJUARE_MODEL_NAME` to the runtime; staging uses a plain copy into `target/<profile>/intervjuare-runtime` only, never a hard link. Stale small-model and full large-v3 staging files are removed so the wildcard Tauri resources entry cannot bundle a retired model. Source assets under `assets/models` are never modified by the build. Tauri dev resource staging additionally produces a distinct-inode `target/debug` copy alongside the `intervjuare-runtime` staging; this is disk waste but not hard-link corruption (hard links remain forbidden, so no truncating write can alias the source asset). No Tauri resource-mapping redesign is made in this pass. The upcoming clean stable bundle verification must assert exactly one turbo-q5 model and no retired model under `Contents/Resources`.
- The retired `assets/models/ggml-small-q5_1.bin` source asset (190,085,487 bytes, about 181 MiB, SHA-256 `ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb`) and the full unquantized `assets/models/ggml-large-v3.bin` source asset are kept untouched for comparison. Neither is selected by owned configuration and stale staging files for both names are removed so the wildcard Tauri resources entry cannot bundle them.
- The runtime server binary and six dylibs are bundled with local `@executable_path` lookup. There is no `whisper-cli` runtime dependency; inference goes only through the shared persistent server.

### Why turbo-q5 plus one persistent server (measured rationale, 2026-10-10)

Owner-measured evidence on this M1 16 GB machine changed the earlier model choice. The full unquantized `large-v3` is accurate on synthetic Swedish/English but not fast enough under the previous process-per-inference architecture: fresh CLI inference measured about 5.84 s (Swedish) / 4.74 s (English) with roughly 4.0 GB RSS; a persistent fixed-context server still measured about 1.06 s for 0.9 s of audio and about 2.16 s for 9.8 s of audio, and concurrent requests serialized to about 4 s. The owner authorized the full 3 GB model only if it is both excellent and fast, so it is not shipped.

The verified `large-v3-turbo-q5_0` checkpoint with one persistent server, `--no-context` and `--audio-ctx 640` measured about 0.81–0.97 s sequential, with two concurrent client requests completing serialized at about 0.95/1.82 s, while synthetic final Swedish/English remained correct semantically. Two concurrent fresh CLI processes were pathological (about 1.31 s one, about 9.14 s the other). Root cause is repeated model load plus competing Metal processes, which is why exactly one shared server per capture session is required: it keeps one resident model and serializes inference internally instead of contending on Metal.

`--no-context` is set so no cross-request language or text context can leak between the interleaved candidate/interviewer requests sharing that one server. The per-session non-static request path and loopback-only ephemeral binding keep inference local to the session.

Truthful limits: these are synthetic-fixture measurements only and do not prove live meeting quality (for example live AirPods audio). Production Swedish/English quality, capture-to-first-partial and last-phoneme-to-final latencies still require the owner-controlled live gate; live p50/p95 are pending and no live quality is claimed.

The model choice is pragmatic, not a Swedish-quality claim. Synthetic macOS-voice smoke input produced exact English but rendered Swedish “React” as “reakt”. Feasibility research also found source-reported KBLab Swedish WER substantially better than stock OpenAI base/small on Swedish corpora, but a Swedish-specialized checkpoint does not establish English quality. No quality or speed improvement is claimed for `large-v3` until controlled Swedish/English fixture comparisons record quality and latency tradeoffs; a larger model increases app size, memory use, and expected first-partial latency.

### Full-model trial, test-only (owner-authorized, 2026-10-11)

At the owner's explicit request a SEPARATE trial bundle may be built with the
full unquantized `large-v3` (`assets/models/ggml-large-v3.bin`, 3095033483
bytes) for a live AirPods A/B against the default q5 bundle. The default
production build is unchanged (still turbo-q5); the full model is selectable
only via the explicit `INTERVJUARE_WHISPER_MODEL_TRIAL=full-large-v3` build
override, and any other override value fails the build closed. The trial
bundle is delivered as a separately named copy under
`src-tauri/target/full-model-trial/` with the same fixed app/helper
identifiers and the same stable signing route; relocated-copy TCC persistence
is not promised.

Trial-only runtime deltas (all other behavior identical, including the 5 s
curl max-time, 8 s final wrapper, and ~20 s aggregate Stop):

- server warm-readiness bound 150 s instead of 15 s (owner-observed full warm
  readiness ~46-110 s vs ~12 s q5);
- a pending Start reserves a cancellable slot, so Stop during a slow model
  load aborts before any session is published (child killed/reaped, no stale
  events) instead of blocking behind the load.

Controlled synthetic A/B showed IDENTICAL Swedish/English text to q5 with ~2x
slower requests and ~3.4 GB model-server RSS. No quality claim is made for
the full model until the owner-controlled live A/B.

## Independent worker acceptance

Native implementation can be accepted independently when it proves source discovery, exact tag mapping, queue/callback bounds, idempotent Stop, terminal errors and no microphone-to-interviewer fallback using native callbacks or a clearly labelled synthetic harness. Compile success does not prove permissions or separation.

Rust/STT implementation can be accepted independently when it proves independent bounded queues, stateful per-source resampling, stable revision/final semantics, cancellation/reaping, guaranteed transient cleanup, missing/corrupt model failures and real whisper.cpp inference on controlled Swedish and English fixtures. Synthetic voices do not count as live meeting evidence.

Milestone 1 remains gated on a user-authorized short live meeting test after permissions are granted: candidate-only, remote-only and simultaneous speech in Swedish and English; headphones and speaker-mode contamination; app/device loss; repeated Start/Stop; and p50/p95 first-partial plus final-after-last-phoneme latency. Until those pass, the repository must describe live behavior as unverified.

## Verification status (2026-10-07)

Safe non-recording verification passed on macOS 27.0 arm64 (build 26A428) with Node 26.7.0, npm 11.19.0, Rust/Cargo 1.99.0 and Apple Swift 6.4. The active developer directory was `/Library/Developer/CommandLineTools`; full Xcode was not selected, so `xcodebuild -version` was unavailable:

- `npm run test`: 5 passed, 0 failed. This covers early event replay, stale-session rejection, failed/repeated-start gate reset and browser/native runtime detection.
- `npm run lint`: passed without warnings.
- `npm run build`: TypeScript and Vite passed; 21 modules transformed.
- `cargo test --manifest-path src-tauri/Cargo.toml --lib`: the final integrated run passed 20 tests, 0 failed in 0.12 seconds after correcting native-reader startup interleavings, warning/PCM reporting, bounded native float32 layout normalization, and the 16/24/48 kHz Swift-to-Rust rate contract.
- Start now keeps the runtime mutex through complete reader `JoinHandle` registration and publishes the session atomically, then releases the one-shot reader barrier. A concurrent manual Stop cannot observe a partially registered session, while immediate EOF/error cannot execute before ownership is registered. Focused tests passed for publication-versus-Stop serialization, immediate reader release ordering, cancelled startup, concurrent cleanup taking ownership exactly once and reaping the reader, and obsolete terminal cleanup leaving a replacement session untouched. This proves safe ownership ordering under deterministic non-recording fixtures; it does not prove live source separation or latency. The Start command still reports asynchronous acceptance; only the native `capturing` event proves live capture.
- `xcrun swiftc native/macos/Sources/IntervjuareMacOSBridge/main.swift -o $TMPDIR/intervjuare-bridge-t16 -framework AVFoundation -framework CoreMedia -framework ScreenCaptureKit` followed by `$TMPDIR/intervjuare-bridge-t16 synthetic-harness`: passed with `synthetic_harness_passed`. The harness now covers 16/24/48 kHz stereo interleaved and planar conversion plus non-finite rejection.
- `swift build -c debug` was attempted separately and failed while linking the Package.swift manifest because the installed Command Line Tools could not resolve a `PackageDescription.Package` symbol. The actual Swift source still compiled successfully with `xcrun swiftc`, and the Tauri build compiled and bundled the bridge.
- Repeating `swift build -c debug` produced the same manifest-linker failure under `/Library/Developer/CommandLineTools`. The supported Tauri path does not invoke Swift Package Manager: `src-tauri/build.rs` compiles the same `main.swift` directly with `swiftc`, and the existing Tauri debug build passed. No toolchain or package configuration was changed.
- The supported reproducible bundle command is now `npm run bundle:debug`. It runs Tauri's fresh debug build, then `node scripts/sign-macos-bundle.mjs src-tauri/target/debug/bundle/macos/Intervjuare.app`. The script explicitly ad-hoc signs the helper as `com.rafal.intervjuare.macos-bridge`, signs the outer app as `com.rafal.intervjuare`, applies `src-tauri/Entitlements.plist` with hardened runtime to both, and fails unless `codesign --verify --deep --strict --verbose=4` succeeds. This post-bundle script is necessary because the installed Tauri 2.11.1 schema exposes only `beforeBuildCommand` and `beforeBundleCommand`, not an after-bundle hook.
- The initial artifact immediately before that repair failed `codesign --verify --deep --strict --verbose=2 src-tauri/target/debug/bundle/macos/Intervjuare.app` with exit 1, `code has no resources but signature indicates they must be present`. A fresh `npm run bundle:debug` then exited 0, produced `src-tauri/target/debug/bundle/macos/Intervjuare.app`, and its in-script deep strict verification reported `valid on disk` and `satisfies its Designated Requirement`. An independent repeat of `codesign --verify --deep --strict --verbose=4` exited 0. The resulting ad-hoc identities were app `com.rafal.intervjuare`, CDHash `2f90d737f5bf4204c4772c8acd7b04a4efa965d1`, and helper `com.rafal.intervjuare.macos-bridge`, CDHash `cceb9529691cef6c2692994deea5cf06128b8930`; both have no Team ID. `security find-identity -v -p codesigning` still reported `0 valid identities found`, so this repairs artifact integrity but does not provide stable identity across rebuilds or prove TCC attribution.
- Bundled-app startup evidence is non-live: the exact freshly built executable remained running during a 15-second tracked process probe, emitted zero stdout/stderr bytes, and was then terminated by the verifier. The bundled helper returned `synthetic_harness_passed` with exit 0 and bundled `whisper-cli --help` exited 0. No capture command was invoked and no permission prompt was clicked.
- The bundled Swift sidecar synthetic harness passed. Bundle inspection found the app executable and bridge under `Contents/MacOS`, plus the local model, whisper CLI and six dylibs under `Contents/Resources`. The inspected bundle at that time contained the retired 190,085,487-byte small model (SHA-256 `ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb`); that stale bundle is not valid for the current turbo-q5 production contract and must not be treated as passing. The upcoming clean stable bundle verification must assert exactly one `ggml-large-v3-turbo-q5_0.bin` and no retired model under `Contents/Resources`.
- The bundle contains microphone, screen-capture and audio-capture usage descriptions. Static persistence inspection found only per-inference `audio.wav` creation inside the RAII-owned private workspace; no default raw-audio history path and no leftover `intervjuare-inference-*` directory were found.
- A scoped source scan found no Milestone 2 Hermes/provider/credential implementation in owned `src`, `src-tauri` or `native` code.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` was unavailable because the installed stable Rust toolchain lacks the `rustfmt` component. No installation or toolchain change was made; compilation and all tests passed.
- The confirmed Stop abort was removed: the helper no longer invokes `FileHandle.synchronizeFile()` on stdout. JSON lines use a bounded POSIX write loop, ignore `SIGPIPE`, retry only `EINTR`, and return failure rather than raising a Foundation exception on a broken reader. A capture-free subprocess regression against the freshly compiled helper and the rebuilt bundled helper passed twice: with an open pipe it emitted `synthetic_harness_passed` and exited 0; after the parent closed the read end it exited 0 without SIGABRT/SIGPIPE. This exercises pipe termination, not live capture Stop.

The live milestone acceptance gate is blocked, not failed. No real microphone or application audio was recorded, so capture-to-first-partial, last-phoneme-to-final, source separation, real revision ordering and quality have no measurements yet. The owner-observed bundle repeatedly asked for Screen & System Audio Recording access on Refresh even after the displayed Intervjuare entry was enabled. A ScreenCapture-only TCC reset for `com.rafal.intervjuare` was subsequently owner-authorized and completed by the coordinator with exit 0, but owner quit/reopen/re-consent observation has not occurred; this task did not repeat the reset or launch a permission-sensitive path. The current reproducibly rebuilt artifact has app CDHash `2f90d737f5bf4204c4772c8acd7b04a4efa965d1`, helper CDHash `cceb9529691cef6c2692994deea5cf06128b8930`, and no Team ID. `security find-identity -v -p codesigning` reported `0 valid identities found`. This proves only that the current bundle has valid ad-hoc signature integrity and stable explicit identifiers within this artifact; a rebuild may change its CDHash. It does not prove bundled TCC attribution, consent success, source discovery or live capture.

### Read-only permission and focused-path audit (2026-10-07)

- A standalone CLI probe compiled with `xcrun swiftc ... -framework AVFoundation -framework CoreGraphics` called only `AVCaptureDevice.authorizationStatus(for: .audio)` and `CGPreflightScreenCaptureAccess()`. It did not request access, enumerate ScreenCaptureKit sources, start capture, modify TCC or click a prompt. Its process-scoped results were `microphone=not_determined` and `screen_capture_preflight=not_authorized_or_undetermined`.
- Those CLI results do not establish the bundled app's permission state. `CGPreflightScreenCaptureAccess()` cannot distinguish denied from not determined, and TCC attribution is identity-specific. The bundle is `com.rafal.intervjuare`; its ad-hoc app executable identity is `intervjuare-9977dac4670ef8a3` with no Team ID. The bundled native helper is a separate ad-hoc executable identity, `intervjuare-macos-bridge`, also with no Team ID. Because capture APIs execute in the helper process, exact bundled permission attribution cannot be proven safely from the CLI probe without launching a permission-sensitive bundled path; that was deliberately deferred.
- Focused non-recording commands passed for queue overflow/drop-oldest behavior, source-tag preservation, per-source resampling/queue independence, nonzero decoder-process exit and cancellation/reaping. Each selected Rust test passed `1 passed, 0 failed`; the cancellation fixture completed in 0.30 seconds. The exact Swift source compiled and `synthetic-harness` returned `synthetic_harness_passed`. No `intervjuare-inference-*` directory remained.
- The confirmed warning/PCM reporting defect was corrected in Rust. Native `warning` events retain their code, message and source while Rust attaches the current reader session ID before forwarding them to the UI. PCM validation enforces mono at the explicit 16/24/48 kHz allowlist, caps each callback at 48,000 frames/192,000 decoded bytes, and rejects missing source/timestamp, invalid base64/alignment and non-finite samples before selecting or mutating either source resampler. Invalid frames emit the typed non-sensitive `invalid_pcm` warning and do not enter either queue. Focused deterministic tests reject tiny/extreme rates, unsupported multichannel input and oversized payloads while preserving prior candidate resampler phase/history and untouched interviewer state; a later valid frame continues normally. This is safe synthetic protocol evidence only; real callback gap delivery remains part of the deferred live gate.
- The Swift boundary now validates ScreenCaptureKit ASBD data and normalizes supported little-endian float32 interleaved and non-interleaved callbacks to mono before serialization. Frame count, channel count, buffer-list allocation and frame-byte multiplication are bounded before loops/allocations. Source tags and exact 16/24/48 kHz native rate remain unchanged; Rust explicitly accepts only those rates and independently resamples each source to 16 kHz. Swift harness fixtures cover planar/interleaved layouts at all three rates and non-finite rejection; the Rust cross-boundary fixture covers both sources at all three rates, output duration and independent state. This corrects the static native/Rust rate mismatch and the confirmed cause of the owner's earlier `invalid_pcm` observation, but only a fresh owner-controlled capture can prove the live symptom is gone.
- Source discovery no longer discards microphones when ScreenCaptureKit application enumeration fails. The Swift synthetic harness covers full and partial source responses. `cargo test --manifest-path src-tauri/Cargo.toml --lib source_discovery_preserves_microphones_when_applications_are_unavailable` passed 1/1; the final full Rust run passed 21/21 in 0.24 seconds. `cargo check`, `npm run lint`, `npm run build`, the direct Swift compile/harness, and the final `npm run bundle:debug` passed. The reproducible command rebuilt and explicitly ad-hoc signed the app/helper; deep strict verification, bundled helper harness, and bundled Whisper help passed. This validates the error contract and bundle, not actual TCC permission.
- `cargo check` passed after the correction. `cargo fmt -- --check` remains unavailable because `cargo-fmt` is not installed; no component or toolchain configuration was changed.
- Empty/error finalization retains the last valid partial, and an inference error with no valid partial emits `stt_failed`; both remain source-specific in Rust. Real callback timing, empty-final behavior and error delivery remain unverified until the owner-controlled run.

For the deferred live run, first restart the built app after granting permissions and confirm a matching native `capturing` event; command acceptance alone is not success. Use headphones and one dedicated non-private test source. Run Swedish and English candidate-only, remote-only and simultaneous cases, then three repeated Start/Stop cycles and controlled app/device loss. Record only session/source/state, segment/revision/final flags and timestamps. For each language/source case collect at least five utterances, compute capture-to-first-partial from the first partial's `latencyMs`, and compute last-phoneme-to-final from an owner-marked end-of-speech timestamp to final `emittedAtMs`; report sample count, median (p50), nearest-rank p95 and maximum. Do not retain transcript text or raw audio. Speaker-mode contamination remains a separate explicitly authorized test.

### Consolidated acceptance matrix

| Gate | Status | Evidence and limit |
| --- | --- | --- |
| Bounded native/Rust buffers | Passed, synthetic/static | Native `queueDepth=3`; per-source Rust queues cap at 80 and drop oldest only on the affected source; focused overflow test passed. No live overload measurement. |
| Cancellation and child reaping | Passed, harness-only | Focused hanging-process cancellation/reaping test and lifecycle tests passed; live cancellation remains unverified. |
| Repeated Start/Stop | Passed, deterministic lifecycle/pipe fixtures; live blocked | Atomic publication/reader ownership tests passed. Fresh and bundled helper open-pipe/closed-reader regressions exited 0 without the former pipe-sync abort. Three real capture cycles have not been run. |
| Revision handling and event ordering | Passed, frontend/Rust fixtures; live blocked | Stable segment/revision and exact-session replay/stale rejection tests passed. Real callback ordering remains unmeasured. |
| Source assignment | Passed, synthetic/static; live blocked | Native tag mapping and no-relabel tests passed. Candidate-only and selected-app-only isolation need live speech. |
| Permission/source/STT failures | Synthetic/harness-only partial; live-native blocked | Typed paths and nonzero/empty inference fixtures passed. ScreenCaptureKit discovery failure preserves microphones and exposes a typed application error through Swift/Rust/React. The owner-authorized ScreenCapture-only reset exited 0, but post-reset bundled consent/source discovery, real source loss and real STT failure presentation remain unobserved. |
| Gap and contamination reporting | Partial | Native warnings and malformed PCM are forwarded with source/session in tests. Real callback gap delivery and observable contamination remain unverified. |
| No default raw-audio persistence | Passed, static/harness | Only private `0700` per-inference workspaces contain temporary WAV data; fixture runs left no workspace. Crash/deletion failure cannot prove an absolute cleanup guarantee. |
| Candidate-only microphone | Blocked, live-native | No post-correction controlled microphone-only run. |
| Remote-only selected application | Blocked, live-native | No post-correction controlled selected-app-only run. |
| Simultaneous speech | Blocked, live-native | No controlled overlap run. |
| Swedish/English latency and quality | Blocked, live-native | No post-correction live samples, therefore no p50/p95/max. Synthetic voice proves inference routing only; the owner's earlier `hund` to `Hörn` observation is not a controlled result. |

Milestone 1 is therefore not accepted. Automated, static and synthetic evidence is retained, but no blocked row is promoted to passed from compilation, bundle startup, fixtures or the pre-correction owner observation.

## Post-correction Swedish quality investigation (2026-10-09)

The owner-controlled candidate-microphone run provided the first post-correction Swedish live sample. It confirmed candidate/ME source routing and no reported `invalid_pcm`, but quality failed: normalized output contained 98 words for a 106-word reference with approximate WER 32.1%, and the ending was truncated. Six visible finals had median latency 1646.5 ms (min 1566, max 2089). Transcript text and raw audio were not retained.

Source inspection proved one deterministic truncation path: the transcriber loop exited as soon as the shared Stop flag became true, before queued frames or an in-progress utterance could reach the existing finalization branch. Stop also cancelled the final Whisper subprocess via that same flag. The corrected path asks native capture to stop first, drains the bounded per-source queue, finalizes the in-progress utterance, permits that final decode to finish, and only then joins workers. Deterministic fixtures cover trailing speech, queued audio at Stop, monotonic final revision fallback, and independent per-source revision state.

The former frame gate was RMS-only. A deterministic sparse impulse fixture had RMS above the old threshold and would have been admitted as speech. Frame admission now also requires a bounded active-sample ratio; a sustained voiced fixture remains admitted. This narrows obvious impulse/typing false positives without adding a model or changing source labels.

No model replacement was made. The only full multilingual model already available is the existing bundled `ggml-small-q5_1.bin`. Files named `for-tests-ggml-{small,medium,large}.bin` are only 575,451-byte test fixtures, not usable alternative production models, so they cannot support a meaningful quality comparison. Explicit `-l sv`/`-l en` routing and default full-audio context remain unchanged. Deterministic trailing-finalization and impulse rejection can be verified automatically; Swedish WER and live finalization latency still require a new owner-controlled run and are not claimed improved from fixtures alone.

## Stable permissions across rebuilds (owner action required, 2026-10-10)

Root cause of repeated prompts: every rebuild re-signed the app (`com.rafal.intervjuare`)
and helper (`com.rafal.intervjuare.macos-bridge`) ad-hoc (`codesign --sign -`, no Team ID).
Ad-hoc signatures derive their identity from the content hash, so each rebuild produces a new
CDHash (observed: app CDHash changed from `2f90d737…` to `b283d0e9…` between two debug builds
with `security find-identity -v -p codesigning` reporting `0 valid identities found`). macOS TCC
treats each ad-hoc rebuild as a different identity, so the previously granted Microphone and
Screen & System Audio Recording entries no longer match and the system prompts again. A second
contributor was Refresh Sources calling ScreenCaptureKit content enumeration directly, which is a
prompting API when permission is missing.

Code/build prerequisites now in place (no TCC change, no capture, no certificate created):

- Refresh Sources (`list_audio_sources` → bridge `sources`) is passive: microphone enumeration via
  `AVCaptureDevice.DiscoverySession` always runs independently; ScreenCaptureKit enumeration runs
  only after the non-requesting `CGPreflightScreenCaptureAccess()` returns true. When preflight is
  false, the bridge returns the real microphone list with an empty `applications` array and typed
  `screen_audio_permission_denied` (same contract as before), without prompting. The requesting
  screen-capture API is never called on this path.
- Permission requests happen only on explicit owner actions, never on Refresh:
  `ensureMicrophoneAccessForExplicitStart()` (microphone `requestAccess` when not
  determined) and ScreenCaptureKit stream start on Start, plus the dedicated
  “Allow Screen & System Audio Recording” button shown only for typed
  `screen_audio_permission_denied` (Tauri `request_screen_capture_permission` →
  Swift `request-screen-access` → prompting `CGRequestScreenCaptureAccess`, then
  a safe passive Refresh). There is no open-settings affordance or TCC
  modification in owned code.
- The supported permission-test/delivered build uses one explicitly configured persistent identity
  (`INTERVJUARE_CODESIGN_IDENTITY`) for helper-then-app signing with the existing identifiers,
  entitlements (`src-tauri/Entitlements.plist`), hardened runtime, and mandatory
  `codesign --verify --deep --strict`. It FAILS CLOSED with an actionable error when no persistent
  identity is configured and never silently falls back to ad-hoc. The only ad-hoc route is the
  explicitly labeled developer-only escape hatch (`INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1` plus
  `--allow-adhoc-developer-only`), which prints a warning that the artifact will re-prompt and is
  not the stable deliverable. Build identifiers are fixed: app `com.rafal.intervjuare`, helper
  `com.rafal.intervjuare.macos-bridge`.
- Read-only verification (never signs, never touches TCC): `npm run bundle:verify` (single bundle:
  non-ad-hoc certificate signatures + expected identifiers + present DR + app/helper one-identity
  coherence; reports that a two-build comparison is still needed for continuity) or
  `node scripts/verify-macos-bundle.mjs <current.app> [<previous.app>]` (the previous bundle is
  verified through the same full single-bundle gate before any comparison; two-build stability: same
  identifiers, exact stable per-target DR, TeamIdentifier presence parity + equality, leaf-Authority
  stability; app/helper DRs are independent and CDHash equality never required). Ad-hoc app or helper
  always FAILS stable verification. Certificate signatures are detected via `Signature size=` and/or
  Authority lines; a missing signature is never accepted.
- Deterministic checks: `npm run test:signing` covers stable-identity fail-closed behavior,
  identifier coherence, helper-then-app order with entitlements/deep verification, and the
  passive-refresh contract.

Exact one-time owner steps (the agent must not perform these; no `tccutil`, no key creation by the agent):

1. Choose exactly one persistent identity option:
   - **Option A — Apple Development certificate (recommended for local-only use).** Free with an Apple ID
     via Xcode (Settings → Accounts → Manage Certificates → Apple Development). Trusted for local
     development on this Mac; sufficient to give TCC a durable TeamIdentifier + identifier. Not for
     distribution outside this Mac.
   - **Option B — Developer ID Application certificate (preferred for distribution).** Requires the paid
     Apple Developer Program. Use only if the app will be distributed/run on other Macs outside this
     machine (Gatekeeper/notarization path). For a private single-Mac tool, Option A is enough.
   - **Option C — local self-signed “Code Signing” identity (this Mac only, fallback).** In Keychain
     Access → Certificate Assistant → Create a Certificate: name it clearly (e.g. `Intervjuare Local`),
     Identity Type `Self Signed Root`, Certificate Type `Code Signing`. Then in Keychain Access → Get Info
     on that certificate → Trust → Code Signing: `Always Trust`. Implication: this trust entry exists only
     in this Mac's login/system keychain; the signature is valid only here, and other Macs (and Gatekeeper)
     will not accept it. Use only if Options A/B are unavailable.
2. Verify the identity is visible (read-only, exposes only public names, never private keys):
   `security find-identity -v -p codesigning`. Copy the exact full quoted identity string, e.g.
   `"Apple Development: Your Name (TEAMID)"`, `"Developer ID Application: Your Name (TEAMID)"`, or the
   self-signed name from Option C.
3. From the repository root, build and sign the stable deliverable with that exact identity:
   `INTERVJUARE_CODESIGN_IDENTITY="<exact identity from step 2>" npm run bundle:stable`.
   Expect failure with actionable guidance (not a silent ad-hoc bundle) if the variable is unset or `-`.
4. Verify read-only: `npm run bundle:verify` and optionally
   `node scripts/verify-macos-bundle.mjs <new.app> <old.app>`. Expect: deep strict verification passes,
   app/helper identifiers as above, non-ad-hoc signatures on both, and designated
   requirements present (single bundle) and exactly stable across the two builds. For
   Apple Development (recommended for local-only) and Developer ID (preferred for
   distribution), expect a present stable TeamIdentifier on both. For self-signed
   Option C, expect NO TeamIdentifier; stability is proven by the non-ad-hoc signature
   plus the same certificate anchor with fixed identifiers and exact stable DR across
   the two builds. CDHash equality is never required. Any ad-hoc signature FAILS.
5. Grant once: launch the newly certificate-signed bundle, grant Microphone and Screen & System Audio
   Recording in System Settings → Privacy & Security when the explicit Start prompts, then keep launching
   the exact same bundle path with the same identifier thereafter. Do not rename/move the `.app`, run it
   from a different build directory, or mix ad-hoc developer-only builds with the granted bundle — any of
   those present a different identity/path to TCC.

What remains impossible until the owner completes step 1: no stable-permission deliverable can be produced
in this environment because `security find-identity -v -p codesigning` reports `0 valid identities found`;
every build here is ad-hoc and will re-prompt by design. After the owner provides the certificate, ordinary
rebuilds with the same `INTERVJUARE_CODESIGN_IDENTITY`, identifiers, entitlements, and bundle path should
preserve the single grant.

Precise limitations (no absolute guarantee): macOS may still require a new grant after certificate
replacement/expiry, an OS or TCC reset, a bundle identifier/path/signing-requirement change, reinstalling
the app or macOS, or revoking/changing the Keychain trust entry. Those are platform behaviors, not defects
in this pipeline. Historical verification notes dated before 2026-10-10 describing `bundle:debug` as
“explicitly ad-hoc signs” refer to the pre-change pipeline; `bundle:debug` is now the same fail-closed
stable path as `bundle:stable`, and ad-hoc builds are only available via `bundle:adhoc-developer-only`.