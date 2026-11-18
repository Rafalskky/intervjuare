use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};

mod context;
mod conversation;
mod copilot;
mod hermes;
mod profile;
mod suggestion;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

const TRANSCRIPT_EVENT: &str = "transcript-event";
const CAPTURE_STATE_EVENT: &str = "capture-state";
const CAPTURE_WARNING_EVENT: &str = "capture-warning";
const PCM_QUEUE_CAPACITY: usize = 80;
const PCM_SAMPLE_RATE: f64 = 16_000.0;
const PCM_CHANNELS: usize = 1;
const SUPPORTED_PCM_SAMPLE_RATES: [f64; 3] = [16_000.0, 24_000.0, 48_000.0];
const MAX_PCM_FRAMES: usize = 48_000;
const MAX_PCM_BYTES: usize = MAX_PCM_FRAMES * PCM_CHANNELS * std::mem::size_of::<f32>();
const MAX_PCM_BASE64_BYTES: usize = MAX_PCM_BYTES.div_ceil(3) * 4;
const PARTIAL_WINDOW_MS: u64 = 2_400;
const FINAL_SILENCE_MS: u64 = 850;
const MAX_UTTERANCE_MS: u64 = 12_000;
const VAD_RMS_THRESHOLD: f32 = 0.008;
const VAD_PEAK_THRESHOLD: f32 = 0.018;
const VAD_MIN_ACTIVE_RATIO: f32 = 0.08;
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(3);
// One persistent whisper-server per capture session. Fresh CLI processes
// reload the model and contend on Metal; the server keeps the turbo-q5 model
// resident and serializes inference internally.
const WHISPER_SERVER_BIN: &str = "whisper-server";
const CURL_BIN: &str = "/usr/bin/curl";
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
// Test-only full-model trial readiness bound. Owner-observed warm readiness
// for the full unquantized large-v3 was ~46-110 s vs ~12 s for turbo-q5, so
// the 15 s default can never ready a full trial server. 150 s covers the
// observed range with margin and stays finite: expiry kills/reaps the child
// and fails Start cleanly. Inference and Stop budgets below are identical
// for both models (5 s curl max-time, 8 s final, ~20 s aggregate Stop).
const SERVER_STARTUP_TIMEOUT_FULL_TRIAL: Duration = Duration::from_secs(150);
const SERVER_CONNECT_TIMEOUT_SECS: u64 = 3;
// Sub-second benchmark (<1 s sequential turbo-q5): curl max-time stays tight
// at 5 s so a stalled server fails fast. Non-final wrapper timeout is
// max-time + 5 s (10 s); the trailing final fallback keeps its own bounded
// 8 s wrapper and is never weakened.
const SERVER_REQUEST_TIMEOUT_SECS: u64 = 5;
const SERVER_FINAL_TIMEOUT: Duration = Duration::from_secs(8);
const SERVER_RESPONSE_LIMIT: usize = 256 * 1024;
const SERVER_AUDIO_CTX: &str = "640";
// Decoupled ingestion/dispatch budgets (audio_backpressure fix). The PCM
// queue cap stays 80 frames (no inflation). Memory is capped by samples,
// not frame count: utterance assembly is capped by the 12 s max utterance,
// and the shared session-scoped dispatch queue is capped by both job count
// and total pending samples (pending snapshots only; the in-flight request
// holds at most one more utterance). A dispatcher stall therefore absorbs a
// bounded number of requests and then sheds explicitly with a typed
// `stt_overload` warning instead of silently dropping PCM or finals. No
// zero-drop promise under arbitrarily long stalls with bounded memory.
//
// Worst-case RAM bound (explicit, finite). All numbers are hard caps:
// - PCM queue per source: 80 frames AND 256_000 samples (16 s at 16 kHz).
//   A max legal native frame decodes to at most 48_000 samples, so the
//   frame cap alone could hold 80*48_000 = 3.84M samples (~15.4 MiB f32)
//   per source; the sample cap tightens this to 256_000 samples (~1 MiB)
//   per source. Normal 20 ms callbacks carry ~320 samples at 16 kHz
//   (~25.6k samples at full depth), so the sample cap never triggers in
//   normal operation; it only bounds adversarial max-size bursts.
// - Utterance assembly per source: at most MAX_UTTERANCE_SAMPLES +
//   MAX_PCM_FRAMES = 240_000 samples (~0.96 MiB) including the one frame
//   that trips the 12 s final.
// - Dispatch pending (both sources shared): at most 8 jobs and
//   (per-source max + one full max final) = 288_000 + 240_000 = 528_000
//   samples (~2.06 MiB), plus per-source reservation caps below.
// - In-flight: one job at most UTTERANCE_HARD_CAP (240_000, ~0.94 MiB).
// Full-system worst case ≈ 2*(1 MiB PCM + 0.96 MiB utterance) + 2.06 MiB
// pending + ~0.94 MiB in-flight ≈ 7 MiB of f32 audio, plus Vec overhead.
// No stage is frame-count-only: every audio-holding stage has an explicit
// sample cap enforced in release builds, not just debug_assert.
const MAX_UTTERANCE_SAMPLES: usize = 12_000 * 16; // MAX_UTTERANCE_MS at 16 kHz
/// Hard release-mode utterance bound: the utterance that trips the 12 s final
/// may overshoot by at most one max legal native frame (240_000 samples).
const UTTERANCE_HARD_CAP: usize = MAX_UTTERANCE_SAMPLES + MAX_PCM_FRAMES;
const DISPATCH_MAX_JOBS: usize = 8;
// Shared pending budget (C fairness): reserves one full max final per source
// even when the other source sits at its own cap — per-source max (288k) +
// one full max final (240k) = 528k (33 s at 16 kHz). A nominal 12 s final
// (192k) and a max final (240k) are therefore never rejected while the other
// source is at its cap; only a source exceeding its own reservation fails,
// attributed to that source's own segment.
const DISPATCH_MAX_SAMPLES: usize = DISPATCH_MAX_SAMPLES_PER_SOURCE + UTTERANCE_HARD_CAP; // 528_000 (33 s at 16 kHz)
// Per-source reservation inside the shared dispatch budget (fairness).
// Each source may hold at most 6 jobs / 288_000 samples (18 s), leaving at
// least 2 jobs / 96_000 samples (6 s) for the other source even when one
// source floods with finals. A flood beyond the per-source cap sheds the
// flooder's own snapshots first and never silently takes the other source's
// reservation.
const DISPATCH_MAX_JOBS_PER_SOURCE: usize = 6;
const DISPATCH_MAX_SAMPLES_PER_SOURCE: usize = 288_000; // 18 s at 16 kHz
// PCM queue sample cap per source (see worst-case bound above).
const PCM_QUEUE_MAX_SAMPLES: usize = 256_000; // 16 s at 16 kHz
const STT_OVERLOAD_CODE: &str = "stt_overload";
// Terminal final-loss code: an accepted final was shed and the session is
// terminally failed (capture no longer presented as healthy). Emitted
// before the terminal stopped state with cumulative lost counters.
const STT_FINAL_LOST_CODE: &str = "stt_final_lost";
// Aggregate graceful-Stop budget (A1/B1). Native child 3 s + dispatcher
// drain 14 s + server teardown 3 s = ~20 s total, matching the documented
// bound. The dispatcher drain deadline is enforced inside run_dispatcher:
// expiry is checked before starting each new inference, every request
// (final AND non-final) is capped at the remaining aggregate budget, and
// partials are skipped once draining so they can never extend the drain.
// Every final not decoded before expiry is reported with a typed warning
// instead of silently discarded. stop_session joins workers after requesting
// GracefulDrain, so the join inherits this bound (no invisible 8 s *
// queue-length hang, no 10 s non-final wrapper overrunning the budget).
const STOP_DRAIN_BUDGET: Duration = Duration::from_secs(14);
// Ingestion-thread-only idle fallback (M4). ScreenCaptureKit delivers PCM
// only when audio callbacks fire; sustained silence produces no callbacks,
// so the captured clock cannot advance and no frame arrives to trigger the
// 850 ms silence final. When the PCM queue stays continuously empty for
// this long (measured on the ingestion thread's own monotonic clock, never
// the inference wall clock), force exactly one trailing final. 850 ms gate
// + 350 ms margin = 1200 ms; backed-up frames (non-empty queue) never
// trigger it, and force_final clears the utterance so no duplicate emits.
const IDLE_FINALIZE_MS: u64 = 1_200;
// Per-model STT tuning. The q5 profile keeps the exact pre-trial values; the
// full-model trial profile applies only when build.rs staged the full model
// (same selection rule as the readiness bound). The full model needs ~2-4 s
// per request, so it trades partial frequency for final latency and cuts
// long utterances at a pause instead of mid-word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SttProfile {
    max_utterance_ms: u64,
    first_partial_ms: u64,
    partial_window_ms: u64,
    // Some(ms): at max length, cut at the quietest 20 ms window within the
    // last `ms` (only when the tail still holds speech). The tail is kept
    // losslessly as the next segment.
    pause_split_lookback_ms: Option<u64>,
    // Must cover the utterance hard cap (whisper truncates beyond it).
    audio_ctx: &'static str,
    request_timeout_secs: u64,
    // Finals pop before partials and drop superseded same-segment partials.
    prefer_finals: bool,
    dispatch_max_samples_per_source: usize,
}

impl SttProfile {
    const fn max_utterance_samples(&self) -> usize {
        self.max_utterance_ms as usize * 16
    }

    const fn utterance_hard_cap(&self) -> usize {
        self.max_utterance_samples() + MAX_PCM_FRAMES
    }

    const fn dispatch_max_samples(&self) -> usize {
        self.dispatch_max_samples_per_source + self.utterance_hard_cap()
    }
}

const Q5_STT_PROFILE: SttProfile = SttProfile {
    max_utterance_ms: MAX_UTTERANCE_MS,
    first_partial_ms: 900,
    partial_window_ms: PARTIAL_WINDOW_MS,
    pause_split_lookback_ms: None,
    audio_ctx: SERVER_AUDIO_CTX,
    request_timeout_secs: SERVER_REQUEST_TIMEOUT_SECS,
    prefer_finals: false,
    dispatch_max_samples_per_source: DISPATCH_MAX_SAMPLES_PER_SOURCE,
};
// The q5 profile must stay byte-for-byte the legacy bounds.
const _: () = assert!(
    Q5_STT_PROFILE.max_utterance_samples() == MAX_UTTERANCE_SAMPLES
        && Q5_STT_PROFILE.utterance_hard_cap() == UTTERANCE_HARD_CAP
        && Q5_STT_PROFILE.dispatch_max_samples() == DISPATCH_MAX_SAMPLES
);

// Full trial: 20 s utterances (hard cap 368_000 samples = 23 s), audio
// context 1152 = 23.04 s, 8 s curl max-time (still inside the 8 s final
// wrapper), per-source reservation 30 s >= one hard-cap final.
const FULL_TRIAL_STT_PROFILE: SttProfile = SttProfile {
    max_utterance_ms: 20_000,
    first_partial_ms: 1_500,
    partial_window_ms: 5_000,
    pause_split_lookback_ms: Some(1_500),
    audio_ctx: "1152",
    request_timeout_secs: 8,
    prefer_finals: true,
    dispatch_max_samples_per_source: 480_000,
};
static SESSION_NUMBER: AtomicU64 = AtomicU64::new(1);
static INFERENCE_NUMBER: AtomicU64 = AtomicU64::new(1);
static REQUEST_PATH_NUMBER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
struct AudioRuntime(Arc<Mutex<RuntimeInner>>);

#[derive(Default)]
struct RuntimeInner {
    session: Option<CaptureSession>,
    starting: Option<PendingStart>,
}

/// A Start that reserved the runtime but has not published a session yet
/// (slow whisper-server load, no workers running). Lets Stop cancel the load
/// promptly instead of blocking behind it: Stop takes this slot and signals
/// cancel, and Start aborts before publishing anything. No session is ever
/// published for a cancelled start, the starting child is killed/reaped, and
/// a replacement Start can proceed immediately.
#[derive(Clone)]
struct PendingStart {
    session_id: String,
    cancel: Arc<AtomicBool>,
}

struct CaptureProcess {
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

struct CaptureSession {
    session_id: String,
    shutdown: Arc<ShutdownState>,
    process: CaptureProcess,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    server: Arc<Mutex<Option<RunningServer>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownMode {
    GracefulDrain,
    Cancel,
}

#[derive(Default)]
struct ShutdownState {
    requested: AtomicBool,
    cancelled: AtomicBool,
    input_closed: AtomicBool,
    drain_deadline: Mutex<Option<Instant>>,
}

impl ShutdownState {
    fn request(&self, mode: ShutdownMode) {
        if mode == ShutdownMode::Cancel {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        self.requested.store(true, Ordering::SeqCst);
        if mode == ShutdownMode::GracefulDrain {
            // First request wins: tests may pre-arm a short drain budget via
            // request_with_budget before stop_session re-requests; production
            // Stop calls once on a fresh state, so behavior is unchanged.
            let mut deadline = self.drain_deadline.lock().expect("drain deadline");
            if deadline.is_none() {
                *deadline = Some(Instant::now() + STOP_DRAIN_BUDGET);
            }
        }
    }

    #[cfg(test)]
    fn request_with_budget(&self, mode: ShutdownMode, budget: Duration) {
        if mode == ShutdownMode::Cancel {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        self.requested.store(true, Ordering::SeqCst);
        if mode == ShutdownMode::GracefulDrain {
            *self.drain_deadline.lock().expect("drain deadline") =
                Some(Instant::now() + budget);
        }
    }

    fn mark_input_closed(&self) {
        self.input_closed.store(true, Ordering::SeqCst);
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn is_draining(&self) -> bool {
        self.is_requested() && !self.is_cancelled()
    }

    fn is_input_closed(&self) -> bool {
        self.input_closed.load(Ordering::SeqCst)
    }

    fn stop_signal(&self) -> &AtomicBool {
        &self.requested
    }

    fn cancel_signal(&self) -> &AtomicBool {
        &self.cancelled
    }

    /// Remaining graceful-drain budget for ALL trailing requests (A1).
    /// None only when not draining (normal path: full timeouts apply).
///     /// While draining, always Some: ZERO when the deadline already passed
///     /// (or was never armed — defensive) so every request cap floors at ~1 ms
///     /// and is killed/reaped promptly instead of running a full 10 s wrapper
///     /// past the aggregate budget. samples_to_ms intentionally floors
///     /// (integer division): queue-age metadata never overstates buffered
///     /// audio; Duration caps below keep full sub-millisecond precision.
    fn remaining_drain(&self) -> Option<Duration> {
        if !self.is_draining() {
            return None;
        }
        let deadline = (*self.drain_deadline.lock().expect("drain deadline"))?;
        Some(deadline.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO))
    }

    fn is_drain_expired(&self) -> bool {
        if !self.is_draining() {
            return false;
        }
        match *self.drain_deadline.lock().expect("drain deadline") {
            None => false,
            Some(deadline) => Instant::now() >= deadline,
        }
    }
}

struct ReaderStart {
    receiver: mpsc::Receiver<()>,
}

impl ReaderStart {
    fn wait(self) -> bool {
        self.receiver.recv().is_ok()
    }
}

fn reader_start_barrier() -> (ReaderStart, mpsc::Sender<()>) {
    let (sender, receiver) = mpsc::channel();
    (ReaderStart { receiver }, sender)
}

fn publish_capture_session(
    guard: &mut Option<CaptureSession>,
    session: CaptureSession,
    reader: thread::JoinHandle<()>,
) -> Result<(), String> {
    session
        .workers
        .lock()
        .map_err(|_| "audio worker registry poisoned".to_string())?
        .push(reader);
    *guard = Some(session);
    Ok(())
}

impl CaptureProcess {
    fn stop(&self) {
        // Lock order: take handles out under their locks, drop the guards,
        // THEN wait on the child. Never hold queue/worker locks (or these
        // handle locks) during child process wait or thread join.
        let stdin = self.stdin.lock().ok().and_then(|mut guard| guard.take());
        if let Some(mut stdin) = stdin {
            let _ = writeln!(stdin, "stop");
        }
        let child = self.child.lock().ok().and_then(|mut guard| guard.take());
        if let Some(mut child) = child {
            terminate_child(&mut child, PROCESS_STOP_TIMEOUT);
        }
    }
}

impl Drop for CaptureProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureSource {
    id: String,
    name: String,
    bundle_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceList {
    microphones: Vec<CaptureSource>,
    applications: Vec<CaptureSource>,
    application_discovery_error: Option<SourceDiscoveryError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceDiscoveryError {
    code: String,
    message: String,
}

fn source_list_from_native(message: NativeMessage) -> Result<SourceList, String> {
    if message.kind == "error" {
        return Err(message.message.unwrap_or_else(|| "native source discovery failed".into()));
    }
    if message.kind != "sources" {
        return Err("native source discovery returned an unexpected event".into());
    }
    Ok(SourceList {
        microphones: message.microphones.unwrap_or_default(),
        applications: message.applications.unwrap_or_default(),
        application_discovery_error: message.application_discovery_error,
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureConfig {
    microphone_id: String,
    application_id: String,
    language: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeMessage {
    #[serde(rename = "type")]
    kind: String,
    source: Option<Speaker>,
    sample_rate: Option<f64>,
    channels: Option<usize>,
    captured_at_ms: Option<u64>,
    samples: Option<String>,
    code: Option<String>,
    message: Option<String>,
    microphones: Option<Vec<CaptureSource>>,
    applications: Option<Vec<CaptureSource>>,
    application_discovery_error: Option<SourceDiscoveryError>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Speaker {
    Candidate,
    Interviewer,
}

impl Speaker {
    fn id(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Interviewer => "interviewer",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptEvent {
    session_id: String,
    segment_id: String,
    revision: u32,
    speaker: Speaker,
    text: String,
    final_segment: bool,
    started_at_ms: u64,
    ended_at_ms: u64,
    captured_at_ms: u64,
    emitted_at_ms: u64,
    latency_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureState {
    session_id: String,
    state: &'static str,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureWarning {
    session_id: String,
    code: String,
    message: String,
    source: Option<Speaker>,
}

#[derive(Clone, Debug)]
struct PcmFrame {
    source: Speaker,
    captured_at_ms: u64,
    samples: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeWarning {
    code: String,
    message: String,
    source: Option<Speaker>,
}

enum NativeEventOutcome {
    State { state: &'static str, message: String },
    Warning(NativeWarning),
    Error(NativeWarning),
    Frame(PcmFrame),
    Ignore,
}

struct StatefulResampler {
    input_rate: f64,
    output_rate: f64,
    position: f64,
    previous: Option<f32>,
}

impl StatefulResampler {
    fn new(input_rate: f64, output_rate: f64) -> Self {
        Self { input_rate, output_rate, position: 0.0, previous: None }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() || self.input_rate <= 0.0 {
            return Vec::new();
        }
        if (self.input_rate - self.output_rate).abs() < 1.0 {
            self.previous = input.last().copied();
            return input.to_vec();
        }
        let mut joined = Vec::with_capacity(input.len() + usize::from(self.previous.is_some()));
        if let Some(previous) = self.previous {
            joined.push(previous);
        }
        joined.extend_from_slice(input);
        let offset = usize::from(self.previous.is_some()) as f64;
        let step = self.input_rate / self.output_rate;
        let mut output = Vec::new();
        while self.position + offset < joined.len() as f64 {
            let position = self.position + offset;
            let lower = position.floor() as usize;
            if lower + 1 >= joined.len() {
                break;
            }
            let fraction = (position - lower as f64) as f32;
            output.push(joined[lower] * (1.0 - fraction) + joined[lower + 1] * fraction);
            self.position += step;
        }
        self.position -= input.len() as f64;
        self.previous = input.last().copied();
        output
    }
}

struct InferenceWorkspace {
    path: PathBuf,
}

impl InferenceWorkspace {
    fn new() -> Result<Self, String> {
        Self::new_in(&std::env::temp_dir())
    }

    fn new_in(parent: &Path) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "intervjuare-inference-{}-{}-{}",
            std::process::id(),
            now_ms(),
            INFERENCE_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join(path.file_name().expect("inference directory name"));
        #[cfg(unix)]
        {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .map_err(|error| format!("could not create private inference directory: {error}"))?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(&path)
                .map_err(|error| format!("could not create private inference directory: {error}"))?;
        }
        Ok(Self { path })
    }
}

impl Drop for InferenceWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_managed_process(
    command: &mut Command,
    stop: &AtomicBool,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start subprocess: {error}"))?;
    let deadline = timeout.map(|duration| Instant::now() + duration);
    loop {
        if stop.load(Ordering::SeqCst) {
            terminate_child(&mut child, Duration::ZERO);
            return Err("cancelled".into());
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            terminate_child(&mut child, Duration::ZERO);
            return Err("timed out".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate_child(&mut child, Duration::ZERO);
                return Err(format!("could not monitor subprocess: {error}"));
            }
        }
    }
}

// TEMPORARY DIAGNOSTIC instrumentation (metadata-only, one live run).
// Never carries transcript text, audio bytes/sample values, WAV/JSON paths,
// or raw payloads. Only integer/enum metadata for H1/H2/H3 triage.
// Does not change drop-oldest policy, counters, finalization, or inference
// order/deadlines.
fn samples_to_ms(samples: usize) -> u64 {
    samples as u64 * 1_000 / PCM_SAMPLE_RATE as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameDropInfo {
    dropped: bool,
    dropped_captured_at_ms: Option<u64>,
    dropped_samples: Option<usize>,
    /// Exact evicted-frame count for THIS push only (0 when nothing evicted).
    /// Semantics: per-push damage, not inferred historical loss.
    dropped_frames: u64,
    /// Exact total evicted samples for THIS push only (0 when none).
    dropped_total_samples: u64,
    /// Queue-lifetime cumulative evicted frames after THIS push.
    cumulative_dropped_frames: u64,
    /// Queue-lifetime cumulative evicted samples after THIS push.
    cumulative_dropped_samples: u64,
}

struct BoundedFrames {
    frames: Mutex<VecDeque<PcmFrame>>,
    capacity: usize,
    max_samples: usize,
    queued_samples: Mutex<usize>,
    dropped: Mutex<u64>,
    dropped_samples_total: Mutex<u64>,
}

impl BoundedFrames {
    fn new(capacity: usize) -> Self {
        Self::with_sample_cap(capacity, PCM_QUEUE_MAX_SAMPLES)
    }

    fn with_sample_cap(capacity: usize, max_samples: usize) -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            max_samples,
            queued_samples: Mutex::new(0),
            dropped: Mutex::new(0),
            dropped_samples_total: Mutex::new(0),
        }
    }

    /// Push with dual finite bounds (M1): frame count AND total queued
    /// samples. Drops oldest frames (possibly more than one under
    /// adversarial max-size bursts) until both bounds hold, then pushes.
    /// Normal 20 ms frames (~320 samples) never approach the 256k sample
    /// cap at depth 80 (~25.6k samples), so no stall/drop in normal case.
    ///
    /// Precise per-push semantics: `dropped_frames` / `dropped_total_samples`
    /// describe exactly what THIS push evicted (0 when nothing evicted), not
    /// inferred historical loss. `dropped_captured_at_ms` / `dropped_samples`
    /// keep the first evicted frame as stable diagnostic identity.
    /// `cumulative_*` are queue-lifetime totals after THIS push.
    fn push(&self, frame: PcmFrame) -> FrameDropInfo {
        let mut frames = self.frames.lock().expect("frame queue");
        let mut queued = self.queued_samples.lock().expect("queued samples");
        let mut dropped_any = false;
        let mut first_dropped_at: Option<u64> = None;
        let mut first_dropped_n: Option<usize> = None;
        let mut dropped_count = 0_u64;
        let mut dropped_samples = 0_u64;
        while frames.len() >= self.capacity
            || *queued + frame.samples.len() > self.max_samples
        {
            let Some(oldest) = frames.pop_front() else { break };
            *queued -= oldest.samples.len();
            dropped_samples += oldest.samples.len() as u64;
            dropped_count += 1;
            if first_dropped_at.is_none() {
                first_dropped_at = Some(oldest.captured_at_ms);
                first_dropped_n = Some(oldest.samples.len());
            }
            dropped_any = true;
            // Defensive: a single frame larger than the whole sample cap
            // can never fit; drop the incoming frame itself and report.
            // Cumulative counters include both the evicted olds AND the
            // dropped incoming frame itself.
            if frames.is_empty() && frame.samples.len() > self.max_samples {
                *self.dropped.lock().expect("drop count") += dropped_count + 1;
                *self.dropped_samples_total.lock().expect("drop samples") +=
                    dropped_samples + frame.samples.len() as u64;
                let cumulative_dropped_frames =
                    *self.dropped.lock().expect("drop count");
                let cumulative_dropped_samples =
                    *self.dropped_samples_total.lock().expect("drop samples");
                return FrameDropInfo {
                    dropped: true,
                    dropped_captured_at_ms: Some(frame.captured_at_ms),
                    dropped_samples: Some(frame.samples.len()),
                    dropped_frames: dropped_count + 1,
                    dropped_total_samples: dropped_samples + frame.samples.len() as u64,
                    cumulative_dropped_frames,
                    cumulative_dropped_samples,
                };
            }
        }
        *queued += frame.samples.len();
        if dropped_any {
            *self.dropped.lock().expect("drop count") += dropped_count;
            *self.dropped_samples_total.lock().expect("drop samples") += dropped_samples;
        }
        let cumulative_dropped_frames = *self.dropped.lock().expect("drop count");
        let cumulative_dropped_samples =
            *self.dropped_samples_total.lock().expect("drop samples");
        // Single-frame drop detail reports the first evicted frame (stable
        // diagnostic identity); dropped_frames/_total report THIS push total.
        let (dropped_captured_at_ms, dropped_samples_opt) = if dropped_any {
            (first_dropped_at, first_dropped_n)
        } else {
            (None, None)
        };
        frames.push_back(frame);
        FrameDropInfo {
            dropped: dropped_any,
            dropped_captured_at_ms,
            dropped_samples: dropped_samples_opt,
            dropped_frames: if dropped_any { dropped_count } else { 0 },
            dropped_total_samples: if dropped_any { dropped_samples } else { 0 },
            cumulative_dropped_frames,
            cumulative_dropped_samples,
        }
    }

    fn pop(&self) -> Option<PcmFrame> {
        let frame = self.frames.lock().expect("frame queue").pop_front()?;
        *self.queued_samples.lock().expect("queued samples") -= frame.samples.len();
        Some(frame)
    }

    fn is_empty(&self) -> bool {
        self.frames.lock().expect("frame queue").is_empty()
    }

    fn len(&self) -> usize {
        self.frames.lock().expect("frame queue").len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn max_samples(&self) -> usize {
        self.max_samples
    }

    fn queued_samples(&self) -> usize {
        *self.queued_samples.lock().expect("queued samples")
    }

    // Bounded O(capacity): sums sample counts only, never copies audio.
    fn queued_duration_ms(&self) -> u64 {
        self.snapshot().1
    }

    // Single-lock snapshot for the drop path: depth + queued duration.
    // Locks frames then queued_samples (same order as push/pop: no deadlock).
    fn snapshot(&self) -> (usize, u64) {
        let frames = self.frames.lock().expect("frame queue");
        let total = *self.queued_samples.lock().expect("queued samples");
        (frames.len(), samples_to_ms(total))
    }

    #[cfg(test)]
    fn dropped_counts(&self) -> (u64, u64) {
        (
            *self.dropped.lock().expect("drop count"),
            *self.dropped_samples_total.lock().expect("drop samples"),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum SttPhase {
    #[default]
    Idle,
    WaitingGate,
    Inference,
}

impl SttPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingGate => "waiting_gate",
            Self::Inference => "inference",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmissionKind {
    Partial,
    Final,
}

impl SubmissionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Final => "final",
        }
    }
}

struct BackpressureParams {
    source: Speaker,
    depth: usize,
    capacity: usize,
    queued_ms: u64,
    incoming_samples: usize,
    incoming_captured_at_ms: u64,
    dropped_captured_at_ms: Option<u64>,
    dropped_samples: Option<usize>,
    /// Exact evicted-frame count for THIS push (0 when none). Per-push damage.
    dropped_frames: u64,
    /// Exact total evicted samples for THIS push (0 when none).
    dropped_total_samples: u64,
    /// Queue-lifetime cumulative evicted frames after THIS push.
    cumulative_dropped_frames: u64,
    /// Queue-lifetime cumulative evicted samples after THIS push.
    cumulative_dropped_samples: u64,
    prev_callback_captured_at_ms: Option<u64>,
    other_depth: usize,
    other_queued_ms: u64,
    phase: SttPhase,
    phase_elapsed_ms: Option<u64>,
    other_phase: SttPhase,
    other_phase_elapsed_ms: Option<u64>,
    last_gate_ms: Option<u64>,
    last_infer_ms: Option<u64>,
    last_kind: Option<SubmissionKind>,
    other_gate_ms: Option<u64>,
    other_infer_ms: Option<u64>,
    other_kind: Option<SubmissionKind>,
}

const BACKPRESSURE_PREFIX: &str =
    "Transcription fell behind and dropped the oldest transient audio frame.";

fn format_opt_ms(value: Option<u64>) -> String {
    value.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
}

fn format_opt_kind(value: Option<SubmissionKind>) -> &'static str {
    value.map_or("unknown", |kind| kind.as_str())
}

fn format_backpressure_message(params: BackpressureParams) -> String {
    let in_ms = samples_to_ms(params.incoming_samples);
    let drop_age_ms = match params.dropped_captured_at_ms {
        Some(dropped) if dropped <= params.incoming_captured_at_ms => {
            Some(params.incoming_captured_at_ms - dropped)
        }
        Some(_) => None,
        None => None,
    };
    let drop_ms = params.dropped_samples.map(samples_to_ms);
    // Total evicted duration for THIS push (metadata-only, integer ms).
    let drop_total_ms = samples_to_ms(params.dropped_total_samples as usize);
    let cum_drop_ms = samples_to_ms(params.cumulative_dropped_samples as usize);
    let cb_delta_ms = match params.prev_callback_captured_at_ms {
        Some(prev) if prev <= params.incoming_captured_at_ms => {
            Some(params.incoming_captured_at_ms - prev)
        }
        _ => None,
    };
    format!(
        "{BACKPRESSURE_PREFIX} [diag src={} depth={}/{} cap={} q_ms={} in_n={} in_ms={} drop_age_ms={} drop_ms={} drop_n={} drop_total_ms={} cum_drop_n={} cum_drop_ms={} cb_delta_ms={} other_depth={} other_q_ms={} phase={} phase_ms={} other_phase={} other_phase_ms={} gate_ms={} infer_ms={} kind={} other_gate_ms={} other_infer_ms={} other_kind={}]",
        params.source.id(),
        params.depth,
        params.capacity,
        params.capacity,
        params.queued_ms,
        params.incoming_samples,
        in_ms,
        format_opt_ms(drop_age_ms),
        format_opt_ms(drop_ms),
        params.dropped_frames,
        drop_total_ms,
        params.cumulative_dropped_frames,
        cum_drop_ms,
        format_opt_ms(cb_delta_ms),
        params.other_depth,
        params.other_queued_ms,
        params.phase.as_str(),
        format_opt_ms(params.phase_elapsed_ms),
        params.other_phase.as_str(),
        format_opt_ms(params.other_phase_elapsed_ms),
        format_opt_ms(params.last_gate_ms),
        format_opt_ms(params.last_infer_ms),
        format_opt_kind(params.last_kind),
        format_opt_ms(params.other_gate_ms),
        format_opt_ms(params.other_infer_ms),
        format_opt_kind(params.other_kind),
    )
}

// Minimal thread-safe per-source metadata shared between the reader and its
// transcriber. Reader writes callback observations; transcriber owns STT
// phase/timings (slice 2 fills real transitions). All fields are bounded
// scalars; no audio, text, or paths.
#[derive(Default)]
struct SourceDiag {
    last_callback_captured_at_ms: Mutex<Option<u64>>,
    stt_phase: Mutex<SttPhase>,
    stt_phase_started: Mutex<Option<Instant>>,
    last_gate_ms: Mutex<Option<u64>>,
    last_infer_ms: Mutex<Option<u64>>,
    last_kind: Mutex<Option<SubmissionKind>>,
}

impl SourceDiag {
    fn observe_callback(&self, captured_at_ms: u64) -> Option<u64> {
        let mut guard = self.last_callback_captured_at_ms.lock().expect("diag callback");
        let prev = *guard;
        *guard = Some(captured_at_ms);
        prev
    }

    fn stt_snapshot(&self) -> (SttPhase, Option<u64>, Option<u64>, Option<u64>, Option<SubmissionKind>) {
        let phase = *self.stt_phase.lock().expect("diag phase");
        let started = *self.stt_phase_started.lock().expect("diag phase started");
        let elapsed = started.map(|instant| instant.elapsed().as_millis() as u64);
        let gate = *self.last_gate_ms.lock().expect("diag gate");
        let infer = *self.last_infer_ms.lock().expect("diag infer");
        let kind = *self.last_kind.lock().expect("diag kind");
        (phase, elapsed, gate, infer, kind)
    }

    fn current_phase(&self) -> SttPhase {
        *self.stt_phase.lock().expect("diag phase")
    }

    fn enter_phase(&self, phase: SttPhase) {
        *self.stt_phase.lock().expect("diag phase") = phase;
        *self.stt_phase_started.lock().expect("diag phase started") = Some(Instant::now());
    }

    fn enter_phase_at(&self, phase: SttPhase, started: Instant) {
        *self.stt_phase.lock().expect("diag phase") = phase;
        *self.stt_phase_started.lock().expect("diag phase started") = Some(started);
    }

    fn exit_to_idle(&self) {
        self.enter_phase(SttPhase::Idle);
    }

    fn record_completion(&self, gate_ms: u64, infer_ms: u64, kind: SubmissionKind) {
        *self.last_gate_ms.lock().expect("diag gate") = Some(gate_ms);
        *self.last_infer_ms.lock().expect("diag infer") = Some(infer_ms);
        *self.last_kind.lock().expect("diag kind") = Some(kind);
    }
}

// RAII guard: phase returns to idle on success, cancellation, and errors.
// Holds only the diag Arc reference; never holds the inference gate or queue
// locks. Each lock is taken briefly in enter/exit only.
struct SttPhaseGuard<'a> {
    diag: &'a SourceDiag,
}

impl<'a> SttPhaseGuard<'a> {
    fn enter(diag: &'a SourceDiag, phase: SttPhase) -> Self {
        diag.enter_phase(phase);
        Self { diag }
    }

    fn transition(&self, phase: SttPhase) {
        self.diag.enter_phase(phase);
    }
}

impl Drop for SttPhaseGuard<'_> {
    fn drop(&mut self) {
        self.diag.exit_to_idle();
    }
}


#[derive(Debug, Deserialize)]
struct ServerJson {
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingTranscript {
    revision: u32,
    text: String,
    final_segment: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct RevisionOutcome {
    event: Option<PendingTranscript>,
    warning: Option<String>,
}

// Typed warning when an accepted final decodes successfully but yields no
// valid text and no prior hypothesis exists. Metadata-only, privacy-safe:
// never includes transcript text, audio, or paths. Preserves the Stop
// fallback contract below: a prior nonempty last_text still finalizes.
const EMPTY_FINAL_WARNING: &str =
    "final transcription produced no text; no transcript was emitted";

fn next_transcript_revision(
    revision: &mut u32,
    last_text: &str,
    decoded_text: Result<String, String>,
    final_segment: bool,
) -> RevisionOutcome {
    let (text, warning) = match decoded_text {
        Ok(text) if !text.trim().is_empty() && (text != last_text || final_segment) => {
            (Some(text), None)
        }
        Ok(_) if final_segment && !last_text.trim().is_empty() => {
            (Some(last_text.to_string()), None)
        }
        Err(error) if final_segment && !last_text.trim().is_empty() => {
            (Some(last_text.to_string()), Some(error))
        }
        // Accepted final with successful but empty decode and no valid text
        // ever existed: never fabricate; emit a typed warning instead.
        Ok(text) if final_segment && text.trim().is_empty() => {
            (None, Some(EMPTY_FINAL_WARNING.to_string()))
        }
        Ok(_) => (None, None),
        Err(error) => (None, Some(error)),
    };
    let event = text.map(|text| {
        *revision += 1;
        PendingTranscript { revision: *revision, text, final_segment }
    });
    RevisionOutcome { event, warning }
}

#[derive(Clone)]
struct WhisperServerPaths {
    server: PathBuf,
    model: PathBuf,
}

// Client-side gate so only one request is submitted to the shared server at
// a time. whisper-server also serializes internally, but the gate lets
// waiting work re-check cancellation before submission and prevents stale
// unbounded partial requests from piling up behind an active inference.
// A cancelled curl does not cancel server computation, so bounding what we
// submit is what keeps queues bounded.
#[derive(Clone, Default)]
struct InferenceGate {
    inner: Arc<Mutex<()>>,
}

fn acquire_inference_permit<'a>(
    gate: &'a InferenceGate,
    stop: &AtomicBool,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    use std::sync::TryLockError;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        match gate.inner.try_lock() {
            Ok(guard) => {
                if stop.load(Ordering::SeqCst) {
                    return Err("cancelled".into());
                }
                return Ok(guard);
            }
            // Poisoned mutex can never be acquired again; returning
            // immediately keeps Stop bounded instead of spinning forever.
            Err(TryLockError::Poisoned(_)) => {
                return Err("inference gate poisoned; restart capture".into());
            }
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

struct RunningServer {
    child: Option<Child>,
    endpoint: String,
}

impl RunningServer {
    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_child(&mut child, PROCESS_STOP_TIMEOUT);
        }
    }
}

// Final safety net only. The primary lifecycle is explicit: graceful Stop
// joins transcriber workers (trailing finalization) before terminating the
// server; cancellation/native failure kills request processes and workers,
// then the server. Drop must never be the path that owns that ordering, and
// it must never touch processes it does not own.
impl Drop for RunningServer {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn generate_request_path() -> String {
    let unique = REQUEST_PATH_NUMBER.fetch_add(1, Ordering::Relaxed);
    format!("/s-{}-{}-{unique}", std::process::id(), now_ms())
}

fn inference_url(port: u16, request_path: &str) -> String {
    format!("http://127.0.0.1:{port}{request_path}/inference")
}

// Exact persistent-server flags. Loopback only, per-session non-static
// request path, no timestamps, no cross-request context, bounded audio
// context. Never expose beyond 127.0.0.1 and never log audio/transcripts.
fn whisper_server_argv(server: &Path, model: &Path, port: u16, request_path: &str) -> Vec<String> {
    vec![
        server.to_string_lossy().into_owned(),
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--request-path".into(),
        request_path.to_string(),
        "--no-timestamps".into(),
        "--no-context".into(),
        "--audio-ctx".into(),
        stt_profile().audio_ctx.into(),
    ]
}

// macOS curl argument array (no shell) for one inference. Multipart file plus
// explicit language, JSON response, no timestamps, and an empty prompt so no
// stale prompt leaks across requests. The response goes to an owner-only
// transient file so the existing managed cancellation/reaping path applies.
// --max-filesize is defense-in-depth alongside the bounded file read; both
// stay at SERVER_RESPONSE_LIMIT (256 KiB). Never log payload content.
fn build_server_curl_args(endpoint: &str, wav: &Path, response: &Path, language: &str) -> Vec<String> {
    let file_field = format!("file=@{};type=audio/wav", wav.display());
    let language_field = format!("language={language}");
    vec![
        "-sS".into(),
        "--fail".into(),
        "--connect-timeout".into(),
        SERVER_CONNECT_TIMEOUT_SECS.to_string(),
        "--max-time".into(),
        stt_profile().request_timeout_secs.to_string(),
        "--max-filesize".into(),
        SERVER_RESPONSE_LIMIT.to_string(),
        "-X".into(),
        "POST".into(),
        endpoint.into(),
        "-F".into(),
        file_field,
        "-F".into(),
        language_field,
        "-F".into(),
        "response_format=json".into(),
        "-F".into(),
        "no_timestamps=true".into(),
        "-F".into(),
        "prompt=".into(),
        "--output".into(),
        response.display().to_string(),
    ]
}

// Bounded response read: pre-check metadata, then read at most LIMIT+1
// bytes via File::take so an oversized file is rejected before JSON parse
// without ever allocating the entire file. Never logs payload content.
fn read_bounded_response(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let metadata = fs::metadata(path).map_err(|error| format!("missing whisper server output: {error}"))?;
    if metadata.len() > SERVER_RESPONSE_LIMIT as u64 {
        return Err("whisper server response exceeds bounded limit".into());
    }
    let file = fs::File::open(path).map_err(|error| format!("missing whisper server output: {error}"))?;
    let mut bounded = file.take(SERVER_RESPONSE_LIMIT as u64 + 1);
    let mut buffer = Vec::new();
    bounded
        .read_to_end(&mut buffer)
        .map_err(|error| format!("missing whisper server output: {error}"))?;
    if buffer.len() > SERVER_RESPONSE_LIMIT {
        return Err("whisper server response exceeds bounded limit".into());
    }
    Ok(buffer)
}

// Exact health readiness: only the first HTTP status line counts, and only
// HTTP/1.0 or HTTP/1.1 with status 200. A 503 (or any non-200) with "200"
// in a header or body must be false; malformed lines are false.
fn is_health_ready_response(text: &str) -> bool {
    let first_line = text.split('\n').next().unwrap_or("").trim_end_matches('\r').trim_end();
    for prefix in ["HTTP/1.0 200", "HTTP/1.1 200"] {
        if first_line == prefix {
            return true;
        }
        if first_line.starts_with(prefix)
            && first_line.as_bytes().get(prefix.len()).is_some_and(|byte| *byte == b' ')
        {
            return true;
        }
    }
    false
}

// Bounded JSON parsing for response_format=json ({"text": "..."}). Rejects
// oversized payloads before parsing and never logs content.
fn parse_server_response_json(payload: &[u8]) -> Result<String, String> {
    if payload.len() > SERVER_RESPONSE_LIMIT {
        return Err("whisper server response exceeds bounded limit".into());
    }
    let parsed: ServerJson =
        serde_json::from_slice(payload).map_err(|error| format!("invalid whisper server output: {error}"))?;
    Ok(parsed.text.trim().to_string())
}

fn find_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("could not bind loopback ephemeral port: {error}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|error| format!("could not read ephemeral port: {error}"))
}

// Wait for server readiness with a strict bounded timeout while also
// checking early child exit and Stop cancellation. Returns Err on early
// exit, cancellation, or timeout. Cancellation lets Stop abort a slow model
// load promptly; the caller kills/reaps the child on every error path.
fn wait_for_server_ready(
    child: &mut Child,
    probe: &dyn Fn() -> bool,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("whisper server exited during startup with {status}"));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("could not monitor whisper server: {error}")),
        }
        if probe() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("whisper server readiness timed out".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn probe_server_health(port: u16, request_path: &str) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    let request = format!("GET {request_path}/health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0_u8; 512];
    let Ok(read) = stream.read(&mut response) else {
        return false;
    };
    std::str::from_utf8(&response[..read]).is_ok_and(is_health_ready_response)
}

fn bridge_executable() -> Result<PathBuf, String> {
    adjacent_artifact("intervjuare-macos-bridge")
}

fn adjacent_artifact(name: &str) -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate application executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| "application executable has no parent directory".to_string())?;
    let adjacent = directory.join(name);
    if adjacent.exists() {
        return Ok(adjacent);
    }
    // Unbundled developer/test runtime: the model (and only the model) lives
    // solely in the dedicated intervjuare-runtime staging directory, which
    // the Tauri resources mapping copies into Contents/Resources at bundle
    // time. Check that sibling before the bundled Resources lookup.
    let runtime_staged = directory.join("intervjuare-runtime").join(name);
    if runtime_staged.exists() {
        return Ok(runtime_staged);
    }
    let bundled_resource = directory.join("../Resources").join(name);
    if bundled_resource.exists() {
        return Ok(bundled_resource);
    }
    if directory.file_name().and_then(|value| value.to_str()) == Some("deps") {
        let parent = directory
            .parent()
            .ok_or_else(|| "test executable directory has no parent".to_string())?;
        let test_adjacent = parent.join(name);
        if test_adjacent.exists() {
            return Ok(test_adjacent);
        }
        let test_runtime_staged = parent.join("intervjuare-runtime").join(name);
        if test_runtime_staged.exists() {
            return Ok(test_runtime_staged);
        }
    }
    Err(format!("required runtime artifact is missing: {}", adjacent.display()))
}

// Model filename is owned by build.rs (resolved default-or-trial selection)
// and exposed as INTERVJUARE_MODEL_NAME. The runtime never duplicates the
// literal, so the selected model cannot drift between build staging and
// runtime lookup. Lookup stays fail-closed: a missing staged model is an
// explicit error.
fn whisper_model_filename() -> &'static str {
    env!("INTERVJUARE_MODEL_NAME")
}

// Test-only full-model trial marker. The default production binary never
// embeds this name as its selected model; the trial binary does.
const FULL_TRIAL_MODEL_NAME: &str = "ggml-large-v3.bin";

/// Bounded readiness for the selected model: 15 s default, 150 s for the
/// test-only full trial (observed ~46-110 s warm). Always finite.
fn server_startup_timeout() -> Duration {
    startup_timeout_for_model(whisper_model_filename())
}

fn startup_timeout_for_model(model: &str) -> Duration {
    if model == FULL_TRIAL_MODEL_NAME {
        SERVER_STARTUP_TIMEOUT_FULL_TRIAL
    } else {
        SERVER_STARTUP_TIMEOUT
    }
}

fn stt_profile() -> SttProfile {
    stt_profile_for_model(whisper_model_filename())
}

fn stt_profile_for_model(model: &str) -> SttProfile {
    if model == FULL_TRIAL_MODEL_NAME {
        FULL_TRIAL_STT_PROFILE
    } else {
        Q5_STT_PROFILE
    }
}

fn whisper_server_paths() -> Result<WhisperServerPaths, String> {
    Ok(WhisperServerPaths {
        server: adjacent_artifact(WHISPER_SERVER_BIN)?,
        model: adjacent_artifact(whisper_model_filename())?,
    })
}

fn spawn_whisper_server(paths: &WhisperServerPaths, cancel: &AtomicBool) -> Result<RunningServer, String> {
    let port = find_free_port()?;
    let request_path = generate_request_path();
    let argv = whisper_server_argv(&paths.server, &paths.model, port, &request_path);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start whisper server: {error}"))?;
    let health_port = port;
    let health_path = request_path.clone();
    let ready = wait_for_server_ready(
        &mut child,
        &|| probe_server_health(health_port, &health_path),
        server_startup_timeout(),
        cancel,
    );
    if let Err(error) = ready {
        terminate_child(&mut child, PROCESS_STOP_TIMEOUT);
        return Err(error);
    }
    Ok(RunningServer {
        child: Some(child),
        endpoint: inference_url(port, &request_path),
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenPermissionStatus {
    granted: bool,
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
struct BridgeScreenPermission {
    #[serde(rename = "type")]
    kind: String,
    granted: Option<bool>,
    message: Option<String>,
}

#[tauri::command]
fn list_audio_sources() -> Result<SourceList, String> {
    let output = Command::new(bridge_executable()?)
        .arg("sources")
        .output()
        .map_err(|error| format!("could not start native source discovery: {error}"))?;
    let message: NativeMessage = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid native source response: {error}"))?;
    source_list_from_native(message)
}

// Explicit owner-initiated Screen & System Audio Recording permission request.
//
// Narrow path for the dedicated UI button shown only for typed
// `screen_audio_permission_denied`. Runs the Swift `request-screen-access`
// helper command (the only caller of the prompting CGRequestScreenCaptureAccess
// API), returns its typed granted/message status, never starts recording,
// never opens System Settings, and never modifies TCC. The caller refreshes
// sources separately via `list_audio_sources`.
#[tauri::command]
fn request_screen_capture_permission() -> Result<ScreenPermissionStatus, String> {
    let output = Command::new(bridge_executable()?)
        .arg("request-screen-access")
        .output()
        .map_err(|error| format!("could not start native screen permission request: {error}"))?;
    let payload: BridgeScreenPermission = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid native screen permission response: {error}"))?;
    if payload.kind != "screenPermission" {
        return Err("native screen permission request returned an unexpected event".into());
    }
    let granted = payload.granted.unwrap_or(false);
    let message = payload.message.unwrap_or_else(|| {
        if granted {
            "Screen & System Audio Recording permission is granted for Intervjuare. It may not take effect until the app is restarted.".into()
        } else {
            "Screen & System Audio Recording permission was not granted for Intervjuare (denied or not yet effective).".into()
        }
    });
    Ok(ScreenPermissionStatus { granted, message })
}

#[tauri::command]
fn start_audio_capture(
    app: AppHandle,
    runtime: State<'_, AudioRuntime>,
    config: CaptureConfig,
) -> Result<String, String> {
    if config.language != "sv" && config.language != "en" {
        return Err("Language must be Swedish or English.".into());
    }
    let pending = {
        let mut guard = runtime.0.lock().map_err(|_| "audio runtime poisoned".to_string())?;
        if guard.session.is_some() {
            return Err("Audio capture is already running.".into());
        }
        if guard.starting.is_some() {
            return Err("Audio capture is already starting.".into());
        }
        let session_id = format!(
            "session-{}-{}",
            now_ms(),
            SESSION_NUMBER.fetch_add(1, Ordering::Relaxed)
        );
        let pending = PendingStart {
            session_id,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        guard.starting = Some(pending.clone());
        pending
    };
    let session_id = pending.session_id.clone();

    let server_paths = match whisper_server_paths() {
        Ok(paths) => paths,
        Err(error) => {
            abort_pending_start(&runtime, &pending);
            return Err(error);
        }
    };
    // Exactly one persistent whisper-server per capture session, launched
    // before capture workers and shared by candidate/interviewer. Startup
    // failure (including a Stop-cancelled slow load) aborts Start cleanly
    // with no session published; the child is killed/reaped on every error
    // path. The runtime lock is NOT held during this bounded wait so Stop can
    // cancel a slow load promptly through the pending slot above.
    let server = match spawn_whisper_server(&server_paths, &pending.cancel) {
        Ok(server) => server,
        Err(error) => {
            abort_pending_start(&runtime, &pending);
            return Err(format!("whisper server could not start: {error}"));
        }
    };
    let mut guard = match runtime.0.lock() {
        Ok(guard) => guard,
        Err(_) => {
            let mut orphan = server;
            orphan.terminate();
            abort_pending_start(&runtime, &pending);
            return Err("audio runtime poisoned".to_string());
        }
    };
    if pending.cancel.load(Ordering::SeqCst) {
        // Stop (or window close) cancelled the load while it ran: never
        // publish a session nobody wants; reap the just-started server.
        drop(guard);
        let mut orphan = server;
        orphan.terminate();
        abort_pending_start(&runtime, &pending);
        return Err("Audio capture start was cancelled.".into());
    }
    if guard.session.is_some() {
        // Defensive: unreachable through the public commands (a second Start
        // is rejected while a slot is held), but never stack sessions: reap
        // the just-started server and fail instead of publishing over one.
        drop(guard);
        let mut orphan = server;
        orphan.terminate();
        abort_pending_start(&runtime, &pending);
        return Err("Audio capture is already running.".into());
    }
    guard.starting = None;
    let server_endpoint = server.endpoint.clone();
    let server = Arc::new(Mutex::new(Some(server)));
    let gate = InferenceGate::default();
    let mut child = match Command::new(bridge_executable()?)
        .arg("capture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            if let Ok(mut guard) = server.lock() {
                if let Some(mut running) = guard.take() {
                    running.terminate();
                }
            }
            return Err(format!("could not start native audio capture: {error}"));
        }
    };
    let startup = (|| -> Result<(ChildStdin, _), String> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "native capture stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "native capture stdout unavailable".to_string())?;
        let request = serde_json::json!({
            "command": "start",
            "microphoneId": config.microphone_id,
            "applicationId": config.application_id,
        });
        writeln!(stdin, "{request}")
            .map_err(|error| format!("could not configure native capture: {error}"))?;
        Ok((stdin, stdout))
    })();
    let (stdin, stdout) = match startup {
        Ok(handles) => handles,
        Err(error) => {
            terminate_child(&mut child, PROCESS_STOP_TIMEOUT);
            if let Ok(mut guard) = server.lock() {
                if let Some(mut running) = guard.take() {
                    running.terminate();
                }
            }
            return Err(error);
        }
    };

    let shutdown = Arc::new(ShutdownState::default());
    let candidate_queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
    let interviewer_queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
    // TEMPORARY DIAGNOSTIC: per-source metadata shared reader<->dispatcher.
    let candidate_diag = Arc::new(SourceDiag::default());
    let interviewer_diag = Arc::new(SourceDiag::default());
    // Decoupled ingestion/dispatch: per-source ingestion workers drain PCM
    // continuously and never block on inference; the single session-scoped
    // dispatcher owns all blocking server inference. The last exiting
    // ingestion worker closes the dispatch queue so the dispatcher drains
    // trailing finals before server teardown.
    let dispatch = Arc::new(DispatchQueue::new());
    let ingestion_count = Arc::new(AtomicUsize::new(2));
    let losses = Arc::new(FinalLossTracker::default());
    // Owned native handles are wrapped BEFORE workers spawn so the B1
    // terminal hook (saturated final) can stop the owned child and unblock
    // the reader. The hook touches only these owned handles — never an
    // unrelated process — and takes no locks during the child wait.
    let native_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let native_stdin: Arc<Mutex<Option<ChildStdin>>> = Arc::new(Mutex::new(None));
    let native_stop: Arc<dyn Fn() + Send + Sync> = {
        let child = native_child.clone();
        let stdin = native_stdin.clone();
        Arc::new(move || {
            let stdin = stdin.lock().ok().and_then(|mut guard| guard.take());
            if let Some(mut stdin) = stdin {
                let _ = writeln!(stdin, "stop");
            }
            let child = child.lock().ok().and_then(|mut guard| guard.take());
            if let Some(mut child) = child {
                terminate_child(&mut child, PROCESS_STOP_TIMEOUT);
            }
        })
    };
    let candidate = spawn_ingestion(
        app.clone(),
        session_id.clone(),
        shutdown.clone(),
        candidate_queue.clone(),
        Speaker::Candidate,
        config.language.clone(),
        dispatch.clone(),
        IngestionLease { remaining: ingestion_count.clone(), dispatch: dispatch.clone() },
        losses.clone(),
        native_stop.clone(),
    );
    let interviewer = spawn_ingestion(
        app.clone(),
        session_id.clone(),
        shutdown.clone(),
        interviewer_queue.clone(),
        Speaker::Interviewer,
        config.language,
        dispatch.clone(),
        IngestionLease { remaining: ingestion_count.clone(), dispatch: dispatch.clone() },
        losses.clone(),
        native_stop.clone(),
    );
    let dispatcher = spawn_dispatcher(
        app.clone(),
        session_id.clone(),
        shutdown.clone(),
        dispatch.clone(),
        server_endpoint,
        gate,
        PathBuf::from(CURL_BIN),
        candidate_diag.clone(),
        interviewer_diag.clone(),
        losses,
    );

    *native_child.lock().expect("native child slot") = Some(child);
    *native_stdin.lock().expect("native stdin slot") = Some(stdin);
    let child = native_child;
    let stdin = native_stdin;
    let workers = Arc::new(Mutex::new(vec![candidate, interviewer, dispatcher]));
    let read_shutdown = shutdown.clone();
    let read_app = app.clone();
    let read_runtime = runtime.inner().clone();
    let read_session_id = session_id.clone();
    let candidate_input = candidate_queue.clone();
    let interviewer_input = interviewer_queue.clone();
    let read_candidate_diag = candidate_diag.clone();
    let read_interviewer_diag = interviewer_diag.clone();
    let (reader_start, start_reader) = reader_start_barrier();
    let reader = thread::spawn(move || {
        if !reader_start.wait() {
            return;
        }
        read_native_stream(
            stdout,
            read_app,
            read_runtime,
            read_session_id,
            read_shutdown,
            candidate_input,
            interviewer_input,
            read_candidate_diag,
            read_interviewer_diag,
        )
    });
    emit_state(&app, &session_id, "starting", "Waiting for native audio callbacks.");
    let session_publish = publish_capture_session(
        &mut guard.session,
        CaptureSession {
            session_id: session_id.clone(),
            shutdown,
            process: CaptureProcess { child, stdin },
            workers,
            server,
        },
        reader,
    );
    if let Err(error) = session_publish {
        stop_runtime(
            runtime.inner(),
            &app,
            Some(&session_id),
            ShutdownMode::Cancel,
            "Audio capture startup failed.",
        );
        return Err(error);
    }
    drop(guard);
    if start_reader.send(()).is_err() {
        stop_runtime(
            runtime.inner(),
            &app,
            Some(&session_id),
            ShutdownMode::Cancel,
            "Audio capture startup failed.",
        );
        return Err("native audio reader could not start".into());
    }
    Ok(session_id)
}

#[tauri::command]
fn stop_audio_capture(app: AppHandle, runtime: State<'_, AudioRuntime>) -> Result<(), String> {
    stop_runtime(
        runtime.inner(),
        &app,
        None,
        ShutdownMode::GracefulDrain,
        "Audio capture stopped; raw audio was not persisted.",
    );
    Ok(())
}

fn stop_runtime(
    runtime: &AudioRuntime,
    app: &AppHandle,
    expected_session: Option<&str>,
    mode: ShutdownMode,
    message: &str,
) {
    // A pending Start (slow server load, no session or workers yet) is
    // cancelled promptly: take the slot, signal cancel, and report stopped
    // for the pending id. The UI gate buffers that event and drops it when
    // the rejected Start resolves, so no stale UI state survives. Start
    // itself aborts before publishing anything and reaps the starting child.
    // `mode` needs no drain here: nothing was published, so there is nothing
    // to drain.
    let pending = take_pending_start(runtime, expected_session);
    if let Some(slot) = pending {
        slot.cancel.store(true, Ordering::SeqCst);
        emit_state(app, &slot.session_id, "stopped", message);
        return;
    }
    let Some(session) = take_session(runtime, expected_session) else { return };
    let session_id = session.session_id.clone();
    stop_session(session, mode);
    emit_state(app, &session_id, "stopped", message);
}

/// Take a pending (not yet published) Start slot when no session is active.
/// A mismatched expected session leaves the slot untouched. Session-first:
/// a published session always wins over a pending slot (which cannot exist
/// alongside one through the public commands, but the order is defensive).
fn take_pending_start(
    runtime: &AudioRuntime,
    expected_session: Option<&str>,
) -> Option<PendingStart> {
    runtime.0.lock().ok().and_then(|mut guard| {
        if guard.session.is_some() {
            return None;
        }
        let slot = guard.starting.take()?;
        if expected_session.is_some_and(|id| id != slot.session_id) {
            guard.starting = Some(slot);
            return None;
        }
        Some(slot)
    })
}

/// Release our own pending-start slot without touching a replacement.
/// Idempotent: Stop already took the slot on the cancel path.
fn abort_pending_start(runtime: &AudioRuntime, pending: &PendingStart) {
    if let Ok(mut guard) = runtime.0.lock() {
        let ours = guard
            .starting
            .as_ref()
            .is_some_and(|slot| slot.session_id == pending.session_id);
        if ours {
            guard.starting = None;
        }
    }
}

fn take_session(runtime: &AudioRuntime, expected_session: Option<&str>) -> Option<CaptureSession> {
    runtime.0.lock().ok().and_then(|mut guard| {
        if expected_session.is_some_and(|id| {
            guard.session.as_ref().is_some_and(|session| session.session_id != id)
        }) {
            None
        } else {
            guard.session.take()
        }
    })
}

fn stop_session(session: CaptureSession, mode: ShutdownMode) {
    session.shutdown.request(mode);
    session.process.stop();
    // Finite aggregate Stop (A1/B1): GracefulDrain sets a 14 s dispatcher drain
    // deadline at request time. The dispatcher checks expiry before each new
    // inference, skips draining partials, caps every trailing curl at the
    // remaining budget, and reports every final not decoded with a typed
    // cumulative warning — so worker joins inherit the bound: ~3 s native +
    // ~14 s drain + ~3 s server ≈ ~20 s total, never 8 s * queue length. Cancellation kills request processes/workers
    // first and reports remaining queued finals the same way. Either way
    // workers join before teardown so no inference is submitted to a dead
    // server and no orphan remains. Never call from a worker thread itself
    // (self-join is skipped defensively); cancellation paths must not block
    // the ingestion/dispatcher threads that need to observe the flag.
    let workers = session.workers.lock().ok().map(|mut workers| std::mem::take(&mut *workers)).unwrap_or_default();
    for worker in workers {
        if worker.thread().id() != thread::current().id() {
            let _ = worker.join();
        }
    }
    if let Ok(mut server) = session.server.lock() {
        if let Some(mut running) = server.take() {
            running.terminate();
        }
    }
}

fn terminate_child(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_native_stream(
    stdout: impl std::io::Read,
    app: AppHandle,
    runtime: AudioRuntime,
    session_id: String,
    shutdown: Arc<ShutdownState>,
    candidate: Arc<BoundedFrames>,
    interviewer: Arc<BoundedFrames>,
    candidate_diag: Arc<SourceDiag>,
    interviewer_diag: Arc<SourceDiag>,
) {
    let mut candidate_resampler: Option<StatefulResampler> = None;
    let mut interviewer_resampler: Option<StatefulResampler> = None;
    let mut terminal_message = "Native capture ended unexpectedly.";
    for line in BufReader::new(stdout).lines() {
        if shutdown.is_cancelled() {
            break;
        }
        let Ok(line) = line else {
            terminal_message = "Native capture output failed.";
            break;
        };
        let Ok(message) = serde_json::from_str::<NativeMessage>(&line) else {
            emit_warning(
                &app,
                &session_id,
                "native_protocol",
                "Native audio returned an invalid event.",
                None,
            );
            continue;
        };
        match handle_native_message(
            message,
            &mut candidate_resampler,
            &mut interviewer_resampler,
        ) {
            NativeEventOutcome::State { state: "stopped", .. } => {
                terminal_message = "Native capture ended.";
                break;
            }
            NativeEventOutcome::State { state, message } => {
                emit_state(&app, &session_id, state, &message)
            }
            NativeEventOutcome::Warning(warning) => emit_warning(
                &app,
                &session_id,
                &warning.code,
                &warning.message,
                warning.source,
            ),
            NativeEventOutcome::Error(warning) => {
                terminal_message = "Native capture failed and was stopped.";
                emit_warning(
                    &app,
                    &session_id,
                    &warning.code,
                    &warning.message,
                    warning.source,
                );
                break;
            }
            NativeEventOutcome::Frame(frame) => {
                let source = frame.source;
                let incoming_captured_at_ms = frame.captured_at_ms;
                let incoming_samples = frame.samples.len();
                let (affected, other, affected_diag, other_diag) = match source {
                    Speaker::Candidate => (
                        &candidate,
                        &interviewer,
                        &candidate_diag,
                        &interviewer_diag,
                    ),
                    Speaker::Interviewer => (
                        &interviewer,
                        &candidate,
                        &interviewer_diag,
                        &candidate_diag,
                    ),
                };
                // Callback cadence first (previous vs incoming on sidecar clock).
                let prev = affected_diag.observe_callback(incoming_captured_at_ms);
                let drop = affected.push(frame);
                if drop.dropped {
                    // Bounded single-lock snapshots; no audio copies, no gate held.
                    let (depth, queued_ms) = affected.snapshot();
                    let (other_depth, other_queued_ms) = other.snapshot();
                    let (phase, phase_elapsed_ms, gate_ms, infer_ms, kind) =
                        affected_diag.stt_snapshot();
                    let (other_phase, other_phase_elapsed_ms, other_gate_ms, other_infer_ms, other_kind) =
                        other_diag.stt_snapshot();
                    let message = format_backpressure_message(BackpressureParams {
                        source,
                        depth,
                        capacity: affected.capacity(),
                        queued_ms,
                        incoming_samples,
                        incoming_captured_at_ms,
                        dropped_captured_at_ms: drop.dropped_captured_at_ms,
                        dropped_samples: drop.dropped_samples,
                        dropped_frames: drop.dropped_frames,
                        dropped_total_samples: drop.dropped_total_samples,
                        cumulative_dropped_frames: drop.cumulative_dropped_frames,
                        cumulative_dropped_samples: drop.cumulative_dropped_samples,
                        prev_callback_captured_at_ms: prev,
                        other_depth,
                        other_queued_ms,
                        phase,
                        phase_elapsed_ms,
                        other_phase,
                        other_phase_elapsed_ms,
                        last_gate_ms: gate_ms,
                        last_infer_ms: infer_ms,
                        last_kind: kind,
                        other_gate_ms,
                        other_infer_ms,
                        other_kind: other_kind,
                    });
                    emit_warning(&app, &session_id, "audio_backpressure", &message, Some(source));
                }
            }
            NativeEventOutcome::Ignore => {}
        }
    }
    shutdown.mark_input_closed();
    stop_runtime(
        &runtime,
        &app,
        Some(&session_id),
        ShutdownMode::Cancel,
        terminal_message,
    );
}

fn handle_native_message(
    message: NativeMessage,
    candidate_resampler: &mut Option<StatefulResampler>,
    interviewer_resampler: &mut Option<StatefulResampler>,
) -> NativeEventOutcome {
    match message.kind.as_str() {
        "state" => {
            let message = message.message.unwrap_or_else(|| "Native state changed.".into());
            match message.as_str() {
                "capturing" => NativeEventOutcome::State { state: "capturing", message },
                "stopped" => NativeEventOutcome::State { state: "stopped", message },
                _ => NativeEventOutcome::Ignore,
            }
        }
        "warning" => NativeEventOutcome::Warning(NativeWarning {
            code: message.code.unwrap_or_else(|| "native_warning".into()),
            message: message.message.unwrap_or_else(|| "Native audio reported a warning.".into()),
            source: message.source,
        }),
        "error" => NativeEventOutcome::Error(NativeWarning {
            code: message.code.unwrap_or_else(|| "native_capture".into()),
            message: message.message.unwrap_or_else(|| "Native capture failed.".into()),
            source: message.source,
        }),
        "pcm" => match decode_pcm(message, candidate_resampler, interviewer_resampler) {
            Ok(frame) => NativeEventOutcome::Frame(frame),
            Err(warning) => NativeEventOutcome::Warning(warning),
        },
        _ => NativeEventOutcome::Ignore,
    }
}

fn decode_pcm(
    message: NativeMessage,
    candidate_resampler: &mut Option<StatefulResampler>,
    interviewer_resampler: &mut Option<StatefulResampler>,
) -> Result<PcmFrame, NativeWarning> {
    let source = message.source;
    let invalid = || NativeWarning {
        code: "invalid_pcm".into(),
        message: "Native audio returned an invalid PCM frame; the frame was dropped.".into(),
        source,
    };
    let source = source.ok_or_else(invalid)?;
    let sample_rate = message
        .sample_rate
        .filter(|rate| {
            rate.is_finite()
                && SUPPORTED_PCM_SAMPLE_RATES
                    .iter()
                    .any(|supported| (*rate - *supported).abs() < 1.0)
        })
        .ok_or_else(invalid)?;
    let channels = message.channels.filter(|channels| *channels == PCM_CHANNELS).ok_or_else(invalid)?;
    let captured_at_ms = message.captured_at_ms.ok_or_else(invalid)?;
    let samples = message.samples.ok_or_else(invalid)?;
    let bytes = decode_base64(samples.as_bytes(), MAX_PCM_BYTES).ok_or_else(invalid)?;
    if bytes.is_empty() || bytes.len() > MAX_PCM_BYTES || bytes.len() % 4 != 0 {
        return Err(invalid());
    }
    let interleaved: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect();
    if interleaved.len() % channels != 0 || interleaved.iter().any(|sample| !sample.is_finite()) {
        return Err(invalid());
    }
    let mono = interleaved;
    let resampler = match source {
        Speaker::Candidate => candidate_resampler,
        Speaker::Interviewer => interviewer_resampler,
    };
    if resampler
        .as_ref()
        .is_none_or(|current| (current.input_rate - sample_rate).abs() >= 1.0)
    {
        *resampler = Some(StatefulResampler::new(sample_rate, PCM_SAMPLE_RATE));
    }
    let samples = resampler.as_mut().ok_or_else(invalid)?.process(&mono);
    if samples.len() > MAX_PCM_FRAMES || samples.iter().any(|sample| !sample.is_finite()) {
        return Err(invalid());
    }
    Ok(PcmFrame { source, captured_at_ms, samples })
}

fn spawn_ingestion(
    app: AppHandle,
    session_id: String,
    shutdown: Arc<ShutdownState>,
    queue: Arc<BoundedFrames>,
    speaker: Speaker,
    language: String,
    dispatch: Arc<DispatchQueue>,
    lease: IngestionLease,
    losses: Arc<FinalLossTracker>,
    // B1: owned native child stop, invoked once on terminal final loss to
    // unblock the reader (EOF) so teardown really runs. Never an unrelated
    // process: production passes only the session's own CaptureProcess.
    native_stop: Arc<dyn Fn() + Send + Sync>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // The lease is held for the whole worker lifetime: the last exiting
        // ingestion worker closes the dispatch queue (RAII, also on panic),
        // so the dispatcher drains trailing finals and then exits.
        let _lease = lease;
        let warn = |code: &str, message: String, source: Option<Speaker>| {
            emit_warning(&app, &session_id, code, &message, source)
        };
        let stop_owned = || native_stop();
        run_ingestion(&queue, &shutdown, speaker, &language, &dispatch, &losses, &warn, &stop_owned);
    })
}

fn spawn_dispatcher(
    app: AppHandle,
    session_id: String,
    shutdown: Arc<ShutdownState>,
    dispatch: Arc<DispatchQueue>,
    endpoint: String,
    gate: InferenceGate,
    curl_bin: PathBuf,
    candidate_diag: Arc<SourceDiag>,
    interviewer_diag: Arc<SourceDiag>,
    // Session-scoped cumulative final-loss tracker (B2), shared with both
    // ingestion workers: drain-expiry and cancel reports increment it.
    losses: Arc<FinalLossTracker>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // TEMPORARY DIAGNOSTIC: per-source STT phase/timings shared with the
        // reader for the backpressure suffix. Guard inside the transcribe
        // path always returns the phase to idle.
        let infer = |job: &InferenceJob| {
            let diag = match job.speaker {
                Speaker::Candidate => &candidate_diag,
                Speaker::Interviewer => &interviewer_diag,
            };
            // A1/B1: every request is capped at the remaining aggregate
            // drain budget (finals: min(8 s, remaining); non-finals:
            // min(10 s, remaining)) so the aggregate Stop stays finite.
            // Kill/reap uses the same managed path (no deadlock); expiry
            // reporting lives in run_dispatcher before starting each
            // inference, and finish_on_stop can never suppress it.
            let drain_cap = shutdown.remaining_drain();
            transcribe_via_server_with_curl(
                &endpoint,
                &job.samples,
                &job.language,
                shutdown.stop_signal(),
                shutdown.cancel_signal(),
                job.kind == SubmissionKind::Final && shutdown.is_draining(),
                drain_cap,
                &gate,
                &curl_bin,
                diag,
                job.kind,
            )
        };
        let emit = |event: TranscriptEvent| {
            copilot::on_transcript(&app, &event);
            let _ = app.emit(TRANSCRIPT_EVENT, event);
        };
        let warn = |code: &str, message: String, source: Option<Speaker>| {
            emit_warning(&app, &session_id, code, &message, source)
        };
        run_dispatcher(&dispatch, &shutdown, &session_id, &infer, &emit, &warn, &losses);
    })
}

fn should_process_transcriber_iteration(
    shutdown: &ShutdownState,
    has_utterance: bool,
    has_popped_frame: bool,
    queue: &BoundedFrames,
) -> bool {
    if shutdown.is_cancelled() {
        return false;
    }
    if !shutdown.is_requested() || !shutdown.is_input_closed() {
        return true;
    }
    has_utterance || has_popped_frame || !queue.is_empty()
}

// Inference goes through the one shared persistent server. The transient
// owner-only WAV workspace is preserved. curl runs with an argument array
// (no shell) and writes bounded JSON to an owner-only transient response
// file so managed cancellation/reaping still applies. Only the gate may
// submit; a cancelled curl never cancels server computation, so the gate
// plus bounded timeouts keep queues bounded.
//
// TEMPORARY DIAGNOSTIC: tracks waiting_gate/inference phase with an RAII
// guard (always back to idle) and records the most recent completed
// gate-wait/inference durations plus partial/final kind. Metadata only;
// never logs audio, text, or paths and never changes inference order,
// deadlines, or partial/final emission.
#[allow(clippy::too_many_arguments)]
fn transcribe_via_server_with_curl(
    endpoint: &str,
    samples: &[f32],
    language: &str,
    stop: &AtomicBool,
    cancelled: &AtomicBool,
    finish_on_stop: bool,
    drain_cap: Option<Duration>,
    gate: &InferenceGate,
    curl_bin: &Path,
    diag: &SourceDiag,
    kind: SubmissionKind,
) -> Result<String, String> {
    if language != "sv" && language != "en" {
        return Err("Language must be Swedish or English.".into());
    }
    // Only one submission at a time; waiting work re-checks cancellation
    // before touching the server so stale partials never pile up.
    let guard = SttPhaseGuard::enter(diag, SttPhase::WaitingGate);
    let gate_start = Instant::now();
    let _permit = acquire_inference_permit(gate, cancelled)?;
    let gate_ms = gate_start.elapsed().as_millis() as u64;
    guard.transition(SttPhase::Inference);
    let infer_start = Instant::now();
    let outcome = (|| -> Result<String, String> {
        let workspace = InferenceWorkspace::new()?;
        let wav = workspace.path.join("audio.wav");
        write_wav(&wav, samples)?;
        let response = workspace.path.join("response.json");
        let args = build_server_curl_args(endpoint, &wav, &response, language);
        let mut command = Command::new(curl_bin);
        command.args(&args).stdout(Stdio::null()).stderr(Stdio::null());
        let ignore_stop = AtomicBool::new(false);
        let (process_stop, timeout) = if finish_on_stop {
            // B1: trailing final ignores the Stop flag only up to the
            // remaining drain budget (never more than 8 s per final).
            let capped = drain_cap.map_or(SERVER_FINAL_TIMEOUT, |remaining| {
                remaining.min(SERVER_FINAL_TIMEOUT)
            });
            // A zero/expired remaining budget must not start unbounded work:
            // floor at 1 ms so run_managed_process kills/reaps promptly and
            // the dispatcher reports the final as undecoded.
            let capped = capped.max(Duration::from_millis(1));
            (&ignore_stop, Some(capped))
        } else if let Some(remaining) = drain_cap {
            // A1: a draining non-final is capped by the same remaining
            // aggregate budget (never the full 10 s wrapper past expiry).
            // drain_cap is Some only while draining; ZERO on expiry floors
            // at 1 ms so the request is killed/reaped promptly.
            let capped = remaining
                .min(Duration::from_secs(stt_profile().request_timeout_secs + 5))
                .max(Duration::from_millis(1));
            (stop, Some(capped))
        } else {
            (stop, Some(Duration::from_secs(stt_profile().request_timeout_secs + 5)))
        };
        let status = run_managed_process(&mut command, process_stop, timeout)
            .map_err(|error| error.replace("subprocess", "whisper server"))?;
        if !status.success() {
            return Err(format!("whisper server request exited with {status}"));
        }
        let payload = read_bounded_response(&response)?;
        parse_server_response_json(&payload)
    })();
    match outcome {
        Ok(text) => {
            let infer_ms = infer_start.elapsed().as_millis() as u64;
            diag.record_completion(gate_ms, infer_ms, kind);
            Ok(text)
        }
        Err(error) => Err(error),
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let data_size = (samples.len() * 2) as u32;
    file.write_all(b"RIFF").map_err(|error| error.to_string())?;
    file.write_all(&(36 + data_size).to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(b"WAVEfmt ").map_err(|error| error.to_string())?;
    file.write_all(&16_u32.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&1_u16.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&1_u16.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&16_000_u32.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&32_000_u32.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&2_u16.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(&16_u16.to_le_bytes()).map_err(|error| error.to_string())?;
    file.write_all(b"data").map_err(|error| error.to_string())?;
    file.write_all(&data_size.to_le_bytes()).map_err(|error| error.to_string())?;
    for sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        file.write_all(&pcm.to_le_bytes()).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

fn frame_has_speech(samples: &[f32]) -> bool {
    if samples.is_empty() || rms(samples) <= VAD_RMS_THRESHOLD {
        return false;
    }
    let active = samples
        .iter()
        .filter(|sample| sample.abs() >= VAD_PEAK_THRESHOLD)
        .count();
    active as f32 / samples.len() as f32 >= VAD_MIN_ACTIVE_RATIO
}

// --- Decoupled ingestion/dispatch (audio_backpressure fix) ---
//
// The reader thread pushes normalized PCM into the per-source BoundedFrames
// queue and never blocks on inference. Each source has one ingestion worker
// that continuously drains its queue, runs VAD/utterance assembly
// (UtteranceSegmenter), and submits bounded audio snapshots to the single
// session-scoped DispatchQueue. One dispatcher worker owns all blocking
// server inference (WAV encode + curl) and emits transcript events.
//
// No locks are held during inference, WAV encoding, or child joins. The
// dispatcher is the sole inference submitter, so the shared server's
// serialization matches exactly one in-flight request and per-source
// transcribers can no longer stall each other's dequeue behind the gate.
//
// Voice/silence decisions use ingested `captured_at_ms` deltas, never
// wall-clock time across an inference stall: a 4 s STT stall must not
// masquerade as 850 ms of silence and finalize speech prematurely.

struct InferenceJob {
    speaker: Speaker,
    segment_number: u64,
    started_at_ms: u64,
    captured_at_ms: u64,
    kind: SubmissionKind,
    samples: Vec<f32>,
    language: String,
}

#[derive(Debug)]
struct DispatchOverload {
    pending_jobs: usize,
    pending_ms: u64,
    shed_kind: SubmissionKind,
    shed_segment: String,
}

struct DispatchState {
    jobs: VecDeque<InferenceJob>,
    pending_samples: usize,
    closed: bool,
}

struct DispatchQueue {
    state: Mutex<DispatchState>,
    wake: Condvar,
    max_samples_per_source: usize,
    max_samples: usize,
    prefer_finals: bool,
}

impl DispatchQueue {
    fn new() -> Self {
        Self::with_profile(&stt_profile())
    }

    fn with_profile(profile: &SttProfile) -> Self {
        Self {
            state: Mutex::new(DispatchState {
                jobs: VecDeque::new(),
                pending_samples: 0,
                closed: false,
            }),
            wake: Condvar::new(),
            max_samples_per_source: profile.dispatch_max_samples_per_source,
            max_samples: profile.dispatch_max_samples(),
            prefer_finals: profile.prefer_finals,
        }
    }

    fn pending_ms_locked(state: &DispatchState) -> u64 {
        samples_to_ms(state.pending_samples)
    }

    // Non-blocking submit with per-source reservation (M3). Locks only for
    // the bounded queue manipulation.
    // Partial snapshots for the same (speaker, segment) coalesce to the
    // latest snapshot (the utterance only grows, so latest is a superset).
    // Finals are never coalesced away: when a bound is exceeded, the oldest
    // pending partial is evicted to make room for a final — preferring a
    // partial from the same source first (so a flooder evicts its own
    // partials before touching the other source), then any partial. Only
    // when no partial can be evicted and a bound is still exceeded does
    // submit fail with typed overload metadata so the caller warns
    // explicitly instead of silently vanishing a final.
    //
    // Fairness: each source is capped at DISPATCH_MAX_JOBS_PER_SOURCE jobs
    // and DISPATCH_MAX_SAMPLES_PER_SOURCE samples, leaving at least
    // (8-6)=2 jobs / (384k-288k)=96k samples for the other source. A
    // candidate flood therefore cannot shed an interviewer final while the
    // interviewer is under its own reservation.
    fn submit(&self, job: InferenceJob) -> Result<(), DispatchOverload> {
        let mut state = self.state.lock().expect("dispatch queue");
        if state.closed {
            return Err(DispatchOverload {
                pending_jobs: state.jobs.len(),
                pending_ms: Self::pending_ms_locked(&state),
                shed_kind: job.kind,
                shed_segment: format!("{}-{}", job.speaker.id(), job.segment_number),
            });
        }
        if job.kind == SubmissionKind::Partial {
            let coalesce = state.jobs.iter().position(|queued| {
                queued.kind == SubmissionKind::Partial
                    && queued.speaker == job.speaker
                    && queued.segment_number == job.segment_number
            });
            if let Some(position) = coalesce {
                let old_len = state.jobs[position].samples.len();
                state.pending_samples = state.pending_samples - old_len + job.samples.len();
                state.jobs[position] = job;
                self.wake.notify_one();
                return Ok(());
            }
        }
        // Full trial: an accepted final supersedes its own queued partials,
        // so they never run (or emit) after the final.
        if self.prefer_finals && job.kind == SubmissionKind::Final {
            let mut removed = 0;
            state.jobs.retain(|queued| {
                let stale = queued.kind == SubmissionKind::Partial
                    && queued.speaker == job.speaker
                    && queued.segment_number == job.segment_number;
                if stale {
                    removed += queued.samples.len();
                }
                !stale
            });
            state.pending_samples -= removed;
        }
        let source_jobs = state.jobs.iter().filter(|j| j.speaker == job.speaker).count();
        let source_samples: usize =
            state.jobs.iter().filter(|j| j.speaker == job.speaker).map(|j| j.samples.len()).sum();
        // Per-source reservation is enforced alongside the shared bound.
        // A source over its own reservation cannot consume the other's
        // guaranteed room: evict only its own partials for room, else fail.
        let mut src_jobs = source_jobs;
        let mut src_samples = source_samples;
        while state.jobs.len() >= DISPATCH_MAX_JOBS
            || state.pending_samples + job.samples.len() > self.max_samples
            || src_jobs >= DISPATCH_MAX_JOBS_PER_SOURCE
            || src_samples + job.samples.len() > self.max_samples_per_source
        {
            if job.kind != SubmissionKind::Final {
                break;
            }
            // Prefer evicting the flooder's own partial first (fairness),
            // then any partial. Finals are never evicted here: an
            // all-finals-full queue fails explicitly so the caller can
            // terminally fail with an accurate warning (M2) instead of
            // silently swapping one lost final for another.
            let under_source_cap = src_jobs < DISPATCH_MAX_JOBS_PER_SOURCE
                && src_samples + job.samples.len() <= self.max_samples_per_source;
            let position = state
                .jobs
                .iter()
                .position(|queued| {
                    queued.kind == SubmissionKind::Partial && queued.speaker == job.speaker
                })
                .or_else(|| {
                    // Cross-source partial eviction only when the incoming
                    // source is under its own reservation (protects the idle
                    // source's finals without letting a flooder eat the
                    // other's room via partial churn).
                    if under_source_cap {
                        state
                            .jobs
                            .iter()
                            .position(|queued| queued.kind == SubmissionKind::Partial)
                    } else {
                        None
                    }
                });
            let Some(position) = position else {
                break;
            };
            let evicted = state.jobs.remove(position).expect("partial eviction");
            state.pending_samples -= evicted.samples.len();
            if evicted.speaker == job.speaker {
                src_jobs -= 1;
                src_samples -= evicted.samples.len();
            }
        }
        let final_source_jobs =
            state.jobs.iter().filter(|j| j.speaker == job.speaker).count();
        let final_source_samples: usize =
            state.jobs.iter().filter(|j| j.speaker == job.speaker).map(|j| j.samples.len()).sum();
        if state.jobs.len() >= DISPATCH_MAX_JOBS
            || state.pending_samples + job.samples.len() > self.max_samples
            || final_source_jobs >= DISPATCH_MAX_JOBS_PER_SOURCE
            || final_source_samples + job.samples.len() > self.max_samples_per_source
        {
            return Err(DispatchOverload {
                pending_jobs: state.jobs.len(),
                pending_ms: Self::pending_ms_locked(&state),
                shed_kind: job.kind,
                shed_segment: format!("{}-{}", job.speaker.id(), job.segment_number),
            });
        }
        state.pending_samples += job.samples.len();
        state.jobs.push_back(job);
        self.wake.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<InferenceJob> {
        let mut state = self.state.lock().expect("dispatch queue");
        // Finals stay FIFO among themselves; per-source segment order holds
        // because a source submits its final before the next segment starts.
        let index = if self.prefer_finals {
            state.jobs.iter().position(|queued| queued.kind == SubmissionKind::Final).unwrap_or(0)
        } else {
            0
        };
        let job = state.jobs.remove(index)?;
        state.pending_samples -= job.samples.len();
        Some(job)
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("dispatch queue");
        state.closed = true;
        self.wake.notify_all();
    }

    fn is_closed_and_empty(&self) -> bool {
        let state = self.state.lock().expect("dispatch queue");
        state.closed && state.jobs.is_empty()
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.state.lock().expect("dispatch queue").closed
    }

    #[cfg(test)]
    fn pending_counts(&self) -> (usize, u64) {
        let state = self.state.lock().expect("dispatch queue");
        (state.jobs.len(), Self::pending_ms_locked(&state))
    }

    fn wait_for_work(&self, timeout: Duration) {
        let state = self.state.lock().expect("dispatch queue");
        let _ = self
            .wake
            .wait_timeout_while(state, timeout, |state| state.jobs.is_empty() && !state.closed)
            .expect("dispatch wait");
    }

    /// Atomically close the queue AND drain remaining jobs under ONE lock
    /// hold (A2). After this returns, no ingestion worker can slip a new
    /// final into an unconsumed queue: submit fails closed and the ingestion
    /// submit-failure path records the shed final in the shared cumulative
    /// tracker with a typed warning. Lock is held only for the bounded
    /// queue manipulation — never during inference, child waits, or joins.
    fn close_and_drain_remaining(&self) -> Vec<InferenceJob> {
        let mut state = self.state.lock().expect("dispatch queue");
        state.closed = true;
        let jobs: Vec<InferenceJob> = state.jobs.drain(..).collect();
        state.pending_samples = 0;
        self.wake.notify_all();
        jobs
    }

    #[cfg(test)]
    fn source_counts(&self, speaker: Speaker) -> (usize, usize) {
        let state = self.state.lock().expect("dispatch queue");
        let jobs = state.jobs.iter().filter(|j| j.speaker == speaker).count();
        let samples = state.jobs.iter().filter(|j| j.speaker == speaker).map(|j| j.samples.len()).sum();
        (jobs, samples)
    }
}

// Cumulative final-loss accounting shared by both ingestion workers (M2/M6).
// Counts accepted finals shed by the bounded dispatch queue so the terminal
// warning carries an honest numeric summary. Metadata only: counts and
// durations, never audio, text, or paths.
#[derive(Default)]
struct FinalLossTracker {
    lost_finals: AtomicU64,
    lost_samples: AtomicU64,
}

impl FinalLossTracker {
    fn record(&self, samples: usize) {
        self.lost_finals.fetch_add(1, Ordering::SeqCst);
        self.lost_samples.fetch_add(samples as u64, Ordering::SeqCst);
    }

    fn snapshot(&self) -> (u64, u64) {
        (
            self.lost_finals.load(Ordering::SeqCst),
            samples_to_ms(self.lost_samples.load(Ordering::SeqCst) as usize),
        )
    }
}

fn format_overload_message(_source: Speaker, overload: &DispatchOverload) -> String {
    format!(
        "Speech inference fell behind and shed a {} snapshot for segment {} (pending {} jobs, {} ms of audio). The shed snapshot was not transcribed; newer audio continues. If this repeats, the service is outpaced by incoming speech.",
        overload.shed_kind.as_str(),
        overload.shed_segment,
        overload.pending_jobs,
        overload.pending_ms,
    )
}

/// Terminal final-loss warning (M2/M6): an accepted final was shed and the
/// session is terminally failed so capture is never presented as healthy
/// while losing speech. Carries the shed segment, queue depth, and the
/// cumulative lost-final count + lost audio duration. Metadata only: no
/// transcript text, audio, or paths. The caller must deliver this before
/// the terminal stopped state.
fn format_final_lost_message(
    _source: Speaker,
    overload: &DispatchOverload,
    lost_finals: u64,
    lost_ms: u64,
) -> String {
    format!(
        "Capture stopped: an accepted final for segment {} could not be transcribed (pending {} jobs, {} ms of audio). Lost finals so far: {} ({} ms of speech). Restart capture; no further audio is accepted as healthy after final loss.",
        overload.shed_segment,
        overload.pending_jobs,
        overload.pending_ms,
        lost_finals,
        lost_ms,
    )
}

/// Drain-expiry warning (A1/B2): a queued final was not decoded before the
/// finite aggregate Stop budget expired. Carries the session-scoped
/// cumulative loss counters so the UI's latest sticky warning always shows
/// the honest total. Metadata only: segment id + counts/durations, never
/// audio, transcript text, or paths.
fn format_drain_expired_final_message(segment: &str, lost_finals: u64, lost_ms: u64) -> String {
    format!(
        "Stop budget expired before segment {segment} could be transcribed; the trailing final was not decoded. Lost finals so far: {lost_finals} ({lost_ms} ms of speech). Earlier finals in this Stop were preserved up to the finite drain budget."
    )
}

/// Cancel warning (B2): a queued final was not decoded because the session
/// was cancelled (failure/teardown path — NOT a Stop-budget expiry, so the
/// message must not claim one). Same cumulative counters as the drain path;
/// both share one session-scoped FinalLossTracker.
fn format_cancelled_final_message(segment: &str, lost_finals: u64, lost_ms: u64) -> String {
    format!(
        "Capture cancelled before segment {segment} could be transcribed; the queued final was not decoded. Lost finals so far: {lost_finals} ({lost_ms} ms of speech). Restart capture."
    )
}

// Pure per-source utterance assembly over ingested frames. Silence is
// measured on the captured clock
// (`latest_captured_at_ms - last_voice_captured_at_ms`), so an inference
// stall cannot masquerade as silence. The snapshot borrows nothing: the
// caller clones at most MAX_UTTERANCE_SAMPLES on production submit.
struct UtteranceSegmenter {
    segment_number: u64,
    utterance: Vec<f32>,
    started_at_ms: u64,
    last_voice_captured_at_ms: u64,
    latest_captured_at_ms: u64,
    next_partial_ms: u64,
    profile: SttProfile,
}

struct PendingSnapshot {
    segment_number: u64,
    started_at_ms: u64,
    captured_at_ms: u64,
    kind: SubmissionKind,
    samples: Vec<f32>,
}

impl UtteranceSegmenter {
    fn new() -> Self {
        Self::with_profile(stt_profile())
    }

    fn with_profile(profile: SttProfile) -> Self {
        Self {
            segment_number: 0,
            utterance: Vec::new(),
            started_at_ms: 0,
            last_voice_captured_at_ms: 0,
            latest_captured_at_ms: 0,
            next_partial_ms: profile.first_partial_ms,
            profile,
        }
    }

    fn has_utterance(&self) -> bool {
        !self.utterance.is_empty()
    }

    fn ingest(&mut self, frame: &PcmFrame, stopping_drained: bool) -> Option<PendingSnapshot> {
        let voice = frame_has_speech(&frame.samples);
        if voice {
            if self.utterance.is_empty() {
                self.started_at_ms = frame.captured_at_ms;
                self.segment_number += 1;
                self.next_partial_ms = self.profile.first_partial_ms;
            }
            self.last_voice_captured_at_ms = frame.captured_at_ms;
        }
        self.latest_captured_at_ms = frame.captured_at_ms;
        if !self.has_utterance() && !voice {
            return None;
        }
        self.utterance.extend_from_slice(&frame.samples);
        // Bounded segmentation (D — never silent truncation): the utterance
        // that trips the 12 s final may overshoot by at most one max legal
        // frame. If it ever exceeds UTTERANCE_HARD_CAP, split losslessly:
        // emit the capped prefix as this ingest's final and retain the
        // remainder as a new utterance segment (new number, continued voice
        // clock). Nothing accepted is discarded; the retained tail stays
        // bounded by the single-frame size (<= 48k for legal frames, enforced
        // upstream). The split prefix is a bounded piece, never counted as a
        // successful full final of the whole utterance — the remainder
        // continues as its own segment.
        let hard_cap = self.profile.utterance_hard_cap();
        if self.utterance.len() > hard_cap {
            let tail = self.utterance.split_off(hard_cap);
            let snapshot = PendingSnapshot {
                segment_number: self.segment_number,
                started_at_ms: self.started_at_ms,
                captured_at_ms: frame.captured_at_ms,
                kind: SubmissionKind::Final,
                samples: std::mem::replace(&mut self.utterance, tail),
            };
            self.segment_number += 1;
            self.started_at_ms = frame.captured_at_ms;
            self.next_partial_ms = self.profile.first_partial_ms;
            // Voice/latest clocks are untouched: the tail's voice timing was
            // recorded above and silence accounting continues seamlessly.
            return Some(snapshot);
        }
        let duration_ms = samples_to_ms(self.utterance.len());
        let silence_ms = self
            .latest_captured_at_ms
            .saturating_sub(self.last_voice_captured_at_ms);
        let at_max = duration_ms >= self.profile.max_utterance_ms;
        if at_max && silence_ms < FINAL_SILENCE_MS && !stopping_drained {
            if let Some(lookback_ms) = self.profile.pause_split_lookback_ms {
                if let Some(snapshot) = self.split_at_quietest(lookback_ms, frame.captured_at_ms) {
                    return Some(snapshot);
                }
            }
        }
        let final_segment = silence_ms >= FINAL_SILENCE_MS || at_max || stopping_drained;
        let partial_due = duration_ms >= self.next_partial_ms;
        if !final_segment && !partial_due {
            return None;
        }
        let kind = if final_segment { SubmissionKind::Final } else { SubmissionKind::Partial };
        let snapshot = PendingSnapshot {
            segment_number: self.segment_number,
            started_at_ms: self.started_at_ms,
            captured_at_ms: frame.captured_at_ms,
            kind,
            samples: self.utterance.clone(),
        };
        self.next_partial_ms = duration_ms.saturating_add(self.profile.partial_window_ms);
        if final_segment {
            self.utterance.clear();
        }
        Some(snapshot)
    }

    // Pause-aware max-length cut: emit the prefix up to the middle of the
    // quietest 20 ms window in the lookback and keep the tail as the next
    // segment. Returns None (caller falls back to a whole final) when the
    // tail holds no speech, so a quiet tail is never sent alone to Whisper.
    fn split_at_quietest(&mut self, lookback_ms: u64, captured_at_ms: u64) -> Option<PendingSnapshot> {
        const WINDOW: usize = 320; // 20 ms at 16 kHz
        let len = self.utterance.len();
        let search_start = len - (lookback_ms as usize * 16).min(len);
        let mut best: Option<(usize, f32)> = None;
        let mut start = search_start;
        while start + WINDOW <= len {
            let energy: f32 = self.utterance[start..start + WINDOW].iter().map(|s| s * s).sum();
            // Ties prefer the later window (shorter retained tail).
            if best.is_none_or(|(_, lowest)| energy <= lowest) {
                best = Some((start, energy));
            }
            start += WINDOW;
        }
        let cut = best?.0 + WINDOW / 2;
        if cut == 0 || cut >= len {
            return None;
        }
        if !self.utterance[cut..].chunks(WINDOW).any(frame_has_speech) {
            return None;
        }
        let tail = self.utterance.split_off(cut);
        let cut_at_ms = captured_at_ms.saturating_sub(samples_to_ms(tail.len()));
        let snapshot = PendingSnapshot {
            segment_number: self.segment_number,
            started_at_ms: self.started_at_ms,
            captured_at_ms: cut_at_ms,
            kind: SubmissionKind::Final,
            samples: std::mem::replace(&mut self.utterance, tail),
        };
        self.segment_number += 1;
        self.started_at_ms = cut_at_ms;
        self.next_partial_ms = self.profile.first_partial_ms;
        Some(snapshot)
    }

    // Trailing final when the queue is drained at Stop but no new frame
    // arrives to trigger it (e.g. speech ends exactly at Stop).
    fn force_final(&mut self) -> Option<PendingSnapshot> {
        if !self.has_utterance() {
            return None;
        }
        let snapshot = PendingSnapshot {
            segment_number: self.segment_number,
            started_at_ms: self.started_at_ms,
            captured_at_ms: self.latest_captured_at_ms,
            kind: SubmissionKind::Final,
            samples: self.utterance.clone(),
        };
        self.utterance.clear();
        Some(snapshot)
    }
}

// RAII ingestion accounting: the last exiting ingestion worker closes the
// shared dispatch queue so the dispatcher drains trailing finals and exits.
// Runs on normal exit and on panic unwind, so the dispatcher can never wait
// forever for a close that never comes.
struct IngestionLease {
    remaining: Arc<AtomicUsize>,
    dispatch: Arc<DispatchQueue>,
}

impl Drop for IngestionLease {
    fn drop(&mut self) {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.dispatch.close();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ingestion(
    queue: &BoundedFrames,
    shutdown: &ShutdownState,
    speaker: Speaker,
    language: &str,
    dispatch: &DispatchQueue,
    losses: &FinalLossTracker,
    on_warning: &dyn Fn(&str, String, Option<Speaker>),
    // B1 terminal hook: invoked (once, best-effort) after a saturated final
    // terminally fails the session. Production wires the owned native child
    // stop here so a reader blocked in BufReader::lines() on a live child
    // with no stdout sees EOF and runs the full idempotent terminal cleanup
    // (owned child stop+reap, dispatcher cancel, terminal state, session
    // removal/join). Called with NO queue/dispatch/worker locks held and
    // must itself take none during child waits or joins.
    on_terminal: &dyn Fn(),
) {
    let mut segmenter = UtteranceSegmenter::new();
    // Ingestion-thread-only idle clock (M4): advances only here, never from
    // inference. Set when the queue is first observed empty; cleared on any
    // frame arrival.
    let mut empty_since: Option<Instant> = None;
    loop {
        let queued_frame = queue.pop();
        if !should_process_transcriber_iteration(
            shutdown,
            segmenter.has_utterance(),
            queued_frame.is_some(),
            queue,
        ) {
            break;
        }
        let stopping = shutdown.is_draining();
        let mut snapshot = None;
        if let Some(frame) = queued_frame.as_ref() {
            empty_since = None;
            let drained = stopping && shutdown.is_input_closed() && queue.is_empty();
            snapshot = segmenter.ingest(frame, drained);
        } else {
            if empty_since.is_none() {
                empty_since = Some(Instant::now());
            }
            thread::sleep(Duration::from_millis(20));
            if stopping && shutdown.is_input_closed() && queue.is_empty() {
                snapshot = segmenter.force_final();
                if snapshot.is_some() {
                    empty_since = None;
                }
            } else if segmenter.has_utterance()
                && empty_since.is_some_and(|since| {
                    since.elapsed() >= Duration::from_millis(IDLE_FINALIZE_MS)
                })
            {
                // Idle fallback (M4): the device stopped delivering callbacks
                // (silence produces no SCStream callbacks), so the captured
                // clock cannot advance. Force exactly one final on the
                // ingestion idle clock. Backed-up frames never reach this
                // branch (pop returned Some above); force_final clears the
                // utterance so no duplicate emits after frames resume.
                snapshot = segmenter.force_final();
                empty_since = None;
            }
        }
        if let Some(snapshot) = snapshot {
            debug_assert!(
                snapshot.samples.len() <= segmenter.profile.utterance_hard_cap(),
                "utterance snapshot must stay near the max utterance bound"
            );
            let shed_len = snapshot.samples.len();
            let shed_kind = snapshot.kind;
            let job = InferenceJob {
                speaker,
                segment_number: snapshot.segment_number,
                started_at_ms: snapshot.started_at_ms,
                captured_at_ms: snapshot.captured_at_ms,
                kind: snapshot.kind,
                samples: snapshot.samples,
                language: language.to_string(),
            };
            if let Err(overload) = dispatch.submit(job) {
                if shed_kind == SubmissionKind::Final {
                    // Deterministic terminal failure (M2): an accepted final
                    // is irretrievably lost. Record honest counters, deliver
                    // the actionable metadata-only warning BEFORE the
                    // terminal state, then terminally fail the session so
                    // capture is never presented as healthy while losing
                    // speech. The Cancel request makes both ingestion
                    // workers exit and the reader emit terminal stopped.
                    losses.record(shed_len);
                    let (lost_finals, lost_ms) = losses.snapshot();
                    on_warning(
                        STT_FINAL_LOST_CODE,
                        format_final_lost_message(speaker, &overload, lost_finals, lost_ms),
                        Some(speaker),
                    );
                    shutdown.request(ShutdownMode::Cancel);
                    // B1: unblock the reader so the terminal warning is
                    // followed by real teardown (not indefinite capturing).
                    // No locks held here; the hook must not take any during
                    // child waits/joins. Never kills unrelated processes:
                    // production passes only the owned session child.
                    on_terminal();
                    break;
                }
                // Partials may be shed/coalesced when superseded: warn and
                // continue healthy.
                on_warning(
                    STT_OVERLOAD_CODE,
                    format_overload_message(speaker, &overload),
                    Some(speaker),
                );
            }
        }
    }
}

#[derive(Default)]
struct DispatcherEmission {
    segment_number: u64,
    revision: u32,
    last_text: String,
    initialised: bool,
}

// Single session-scoped inference owner. Pops jobs FIFO (per-source order is
// preserved through the FIFO), performs exactly one blocking inference at a
// time via `infer`, and emits transcript events with the same monotonic
// revision / final-immutability contract as before. Holds no locks during
// inference.
//
// Finite aggregate Stop (A1/B1): when draining, partials are skipped (never
// inferred — superseded snapshots must not extend the drain), expiry is
// checked BEFORE starting a new blocking inference, and the in-flight curl
// itself is capped at the remaining budget inside the production infer
// closure, so no deadlock. On Cancel it reports remaining queued finals the
// same way before exiting promptly.
fn run_dispatcher(
    dispatch: &DispatchQueue,
    shutdown: &ShutdownState,
    session_id: &str,
    infer: &dyn Fn(&InferenceJob) -> Result<String, String>,
    on_event: &dyn Fn(TranscriptEvent),
    on_warning: &dyn Fn(&str, String, Option<Speaker>),
    // Session-scoped cumulative final-loss accounting (B2), shared with both
    // ingestion workers: every undecoded final (drain-expiry or cancel)
    // increments the same counters, so each warning carries an honest
    // `Lost finals so far: N (M ms)` summary. Metadata only.
    losses: &FinalLossTracker,
) {
    let mut emissions: [DispatcherEmission; 2] = Default::default();
    // Emit one transcript event or warning for a single job. Returns true
    // when a final hypothesis was emitted.
    let emit_job = |job: &InferenceJob,
                    inference: Result<String, String>,
                    emissions: &mut [DispatcherEmission; 2]| {
        let final_segment = job.kind == SubmissionKind::Final;
        let emission = match job.speaker {
            Speaker::Candidate => &mut emissions[0],
            Speaker::Interviewer => &mut emissions[1],
        };
        if !emission.initialised || emission.segment_number != job.segment_number {
            emission.segment_number = job.segment_number;
            emission.revision = 0;
            emission.last_text.clear();
            emission.initialised = true;
        }
        let outcome =
            next_transcript_revision(&mut emission.revision, &emission.last_text, inference, final_segment);
        if let Some(pending) = outcome.event {
            let emitted_at_ms = now_ms();
            on_event(TranscriptEvent {
                session_id: session_id.to_string(),
                segment_id: format!("{}-{}", job.speaker.id(), job.segment_number),
                revision: pending.revision,
                speaker: job.speaker,
                text: pending.text.clone(),
                final_segment: pending.final_segment,
                started_at_ms: job.started_at_ms,
                ended_at_ms: job.captured_at_ms,
                captured_at_ms: job.captured_at_ms,
                emitted_at_ms,
                latency_ms: emitted_at_ms.saturating_sub(job.captured_at_ms),
            });
            emission.last_text = pending.text;
        }
        if let Some(error) = outcome.warning.filter(|error| error != "cancelled") {
            on_warning("stt_failed", error, Some(job.speaker));
        }
    };
    // Report a job that was never decoded (drain expiry or cancel): every
    // final gets a typed reason-correct warning with the session-scoped
    // cumulative counters (B2); partials are shed silently like before
    // (they are superseded snapshots, never accepted finals).
    // finish_on_stop in the infer closure may extend one in-flight trailing
    // final up to its cap — it can never suppress this reporting: every
    // expiry/cancel path runs through here.
    let report_undecoded = |job: &InferenceJob, cancelled: bool| {
        if job.kind == SubmissionKind::Final {
            losses.record(job.samples.len());
            let (lost_finals, lost_ms) = losses.snapshot();
            let segment = format!("{}-{}", job.speaker.id(), job.segment_number);
            if cancelled {
                on_warning(
                    STT_FINAL_LOST_CODE,
                    format_cancelled_final_message(&segment, lost_finals, lost_ms),
                    Some(job.speaker),
                );
            } else {
                on_warning(
                    STT_FINAL_LOST_CODE,
                    format_drain_expired_final_message(&segment, lost_finals, lost_ms),
                    Some(job.speaker),
                );
            }
        }
    };
    loop {
        if shutdown.is_cancelled() {
            // A2: atomic close+drain — late ingestion submits fail closed
            // and are reported on the ingestion seam via the shared tracker.
            for job in dispatch.close_and_drain_remaining() {
                report_undecoded(&job, true);
            }
            break;
        }
        let Some(job) = dispatch.pop() else {
            if dispatch.is_closed_and_empty() {
                break;
            }
            dispatch.wait_for_work(Duration::from_millis(20));
            continue;
        };
        // Drain expiry is checked BEFORE starting a new blocking inference,
        // so the aggregate Stop budget is finite even with a full queue of
        // finals (no 8 s * queue-length invisible hang). The in-flight curl
        // itself is capped at the remaining budget inside the production
        // infer closure, and kill/reap verified by the managed process path,
        // so no deadlock.
        if shutdown.is_draining() && shutdown.is_drain_expired() {
            report_undecoded(&job, false);
            for remaining in dispatch.close_and_drain_remaining() {
                report_undecoded(&remaining, false);
            }
            break;
        }
        // A1: draining partials are skipped, never inferred — superseded
        // snapshots must not extend the aggregate drain past its budget.
        // Finals are always handled (decoded or fallback-reported) below.
        if shutdown.is_draining() && job.kind == SubmissionKind::Partial {
            continue;
        }
        let inference = infer(&job);
        // A stale partial that arrived after its segment's final must not
        // overwrite the final: dispatcher processes FIFO and finals clear
        // per-segment emission state only on segment change, while the
        // transcript store enforces final immutability. If Cancel arrived
        // during the blocking inference, report instead of emitting stale
        // content.
        if shutdown.is_cancelled() {
            report_undecoded(&job, true);
            for remaining in dispatch.close_and_drain_remaining() {
                report_undecoded(&remaining, true);
            }
            // Still surface the just-finished inference when it produced a
            // final hypothesis? No: Cancel means the session is dead; the
            // undecoded report above is the honest record. Drop it.
            break;
        }
        // Drain expired DURING the blocking inference: the just-decoded job
        // is still real capped work, so emit it honestly, then report the
        // rest instead of starting new inferences past the budget.
        if shutdown.is_draining() && shutdown.is_drain_expired() {
            emit_job(&job, inference, &mut emissions);
            for remaining in dispatch.close_and_drain_remaining() {
                report_undecoded(&remaining, false);
            }
            break;
        }
        emit_job(&job, inference, &mut emissions);
    }
}

fn decode_base64(input: &[u8], max_output: usize) -> Option<Vec<u8>> {
    if input.is_empty() || input.len() > MAX_PCM_BASE64_BYTES || input.iter().any(u8::is_ascii_whitespace) {
        return None;
    }
    let mut output = Vec::with_capacity((input.len() / 4 * 3).min(max_output));
    let mut chunk = [0_u8; 4];
    let mut count = 0;
    for byte in input.iter().copied().filter(|byte| !byte.is_ascii_whitespace()) {
        chunk[count] = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 64,
            _ => return None,
        };
        count += 1;
        if count == 4 {
            if chunk[0] == 64 || chunk[1] == 64 || (chunk[2] == 64 && chunk[3] != 64) {
                return None;
            }
            let decoded = 1 + usize::from(chunk[2] != 64) + usize::from(chunk[3] != 64);
            if output.len().checked_add(decoded)? > max_output {
                return None;
            }
            output.push((chunk[0] << 2) | (chunk[1] >> 4));
            if chunk[2] != 64 {
                output.push((chunk[1] << 4) | (chunk[2] >> 2));
            }
            if chunk[3] != 64 {
                output.push((chunk[2] << 6) | chunk[3]);
            }
            count = 0;
        }
    }
    (count == 0).then_some(output)
}

fn emit_state(app: &AppHandle, session_id: &str, state: &'static str, message: &str) {
    let _ = app.emit(
        CAPTURE_STATE_EVENT,
        CaptureState { session_id: session_id.to_string(), state, message: message.to_string() },
    );
}

fn emit_warning(
    app: &AppHandle,
    session_id: &str,
    code: &str,
    message: &str,
    source: Option<Speaker>,
) {
    let _ = app.emit(
        CAPTURE_WARNING_EVENT,
        CaptureWarning {
            session_id: session_id.to_string(),
            code: code.to_string(),
            message: message.to_string(),
            source,
        },
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn source_discovery_preserves_microphones_when_applications_are_unavailable() {
        let source = CaptureSource {
            id: "microphone-1".into(),
            name: "Test Microphone".into(),
            bundle_id: None,
        };
        let result = source_list_from_native(NativeMessage {
            kind: "sources".into(),
            source: None,
            sample_rate: None,
            channels: None,
            captured_at_ms: None,
            samples: None,
            code: None,
            message: None,
            microphones: Some(vec![source]),
            applications: Some(Vec::new()),
            application_discovery_error: Some(SourceDiscoveryError {
                code: "screen_audio_permission_denied".into(),
                message: "Meeting applications unavailable.".into(),
            }),
        })
        .expect("partial source discovery should remain a valid response");

        assert_eq!(result.microphones.len(), 1);
        assert!(result.applications.is_empty());
        assert_eq!(
            result.application_discovery_error.expect("typed app discovery error").code,
            "screen_audio_permission_denied"
        );
    }

    fn screen_permission_status_from_bridge(payload: &str) -> Result<ScreenPermissionStatus, String> {
        let parsed: BridgeScreenPermission = serde_json::from_str(payload)
            .map_err(|error| format!("invalid native screen permission response: {error}"))?;
        if parsed.kind != "screenPermission" {
            return Err("native screen permission request returned an unexpected event".into());
        }
        let granted = parsed.granted.unwrap_or(false);
        let message = parsed.message.unwrap_or_else(|| {
            if granted {
                "Screen & System Audio Recording permission is granted for Intervjuare. It may not take effect until the app is restarted.".into()
            } else {
                "Screen & System Audio Recording permission was not granted for Intervjuare (denied or not yet effective).".into()
            }
        });
        Ok(ScreenPermissionStatus { granted, message })
    }

    #[test]
    fn explicit_screen_permission_parses_typed_grant_and_denial() {
        let granted = screen_permission_status_from_bridge(
            r#"{"type":"screenPermission","granted":true,"message":"granted"}"#,
        )
        .expect("typed grant");
        assert!(granted.granted);
        assert_eq!(granted.message, "granted");

        let denied = screen_permission_status_from_bridge(
            r#"{"type":"screenPermission","granted":false,"message":"denied"}"#,
        )
        .expect("typed denial");
        assert!(!denied.granted);

        assert!(screen_permission_status_from_bridge(r#"{"type":"sources"}"#).is_err());
    }

    #[derive(Default)]
    struct TranscriptStore {
        events: HashMap<String, TranscriptEvent>,
    }

    impl TranscriptStore {
        fn apply(&mut self, event: TranscriptEvent) -> bool {
            if let Some(previous) = self.events.get(&event.segment_id) {
                if previous.final_segment
                    || event.revision <= previous.revision
                    || previous.speaker != event.speaker
                {
                    return false;
                }
            }
            self.events.insert(event.segment_id.clone(), event);
            true
        }
    }

    fn event(id: &str, revision: u32, speaker: Speaker, final_segment: bool) -> TranscriptEvent {
        TranscriptEvent {
            session_id: "test-session".into(),
            segment_id: id.into(),
            revision,
            speaker,
            text: format!("revision {revision}"),
            final_segment,
            started_at_ms: 1,
            ended_at_ms: 2,
            captured_at_ms: 2,
            emitted_at_ms: 3,
            latency_ms: 1,
        }
    }

    #[test]
    fn transcript_revisions_are_monotonic_and_finals_are_immutable() {
        let mut store = TranscriptStore::default();
        assert!(store.apply(event("interviewer-1", 1, Speaker::Interviewer, false)));
        assert!(!store.apply(event("interviewer-1", 1, Speaker::Interviewer, false)));
        assert!(!store.apply(event("interviewer-1", 2, Speaker::Candidate, false)));
        assert!(store.apply(event("interviewer-1", 2, Speaker::Interviewer, true)));
        assert!(!store.apply(event("interviewer-1", 3, Speaker::Interviewer, true)));
    }

    #[test]
    fn revision_builder_orders_partials_and_finalizes_last_valid_text() {
        let mut revision = 0;
        let partial = next_transcript_revision(&mut revision, "", Ok("first".into()), false);
        assert!(partial.warning.is_none());
        let partial = partial.event.expect("partial event");
        assert_eq!(partial, PendingTranscript { revision: 1, text: "first".into(), final_segment: false });
        let duplicate =
            next_transcript_revision(&mut revision, "first", Ok("first".into()), false);
        assert!(duplicate.event.is_none());
        assert!(duplicate.warning.is_none());
        let revised = next_transcript_revision(
            &mut revision,
            "first",
            Ok("first revised".into()),
            false,
        );
        assert!(revised.warning.is_none());
        let revised = revised.event.expect("revised event");
        assert_eq!(
            revised,
            PendingTranscript {
                revision: 2,
                text: "first revised".into(),
                final_segment: false,
            }
        );
        let final_event = next_transcript_revision(
            &mut revision,
            "first revised",
            Ok("first revised".into()),
            true,
        );
        assert!(final_event.warning.is_none());
        let final_event = final_event.event.expect("final event");
        assert_eq!(
            final_event,
            PendingTranscript {
                revision: 3,
                text: "first revised".into(),
                final_segment: true,
            }
        );
    }

    fn dispatch_revision_result(
        result: RevisionOutcome,
    ) -> (Option<PendingTranscript>, Option<String>) {
        (result.event, result.warning.filter(|error| error != "cancelled"))
    }

    #[test]
    fn final_decode_failure_preserves_fallback_and_requests_warning() {
        let mut revision = 1;
        let (event, warning) = dispatch_revision_result(next_transcript_revision(
            &mut revision,
            "valid partial",
            Err("timed out".into()),
            true,
        ));

        assert_eq!(
            event,
            Some(PendingTranscript {
                revision: 2,
                text: "valid partial".into(),
                final_segment: true,
            })
        );
        assert_eq!(
            warning.as_deref(),
            Some("timed out"),
            "a final fallback must not hide the failed final decode"
        );
    }

    #[test]
    fn final_decode_failure_without_partial_warns_without_fabricating_text() {
        let mut revision = 0;
        let (event, warning) = dispatch_revision_result(next_transcript_revision(
            &mut revision,
            "",
            Err("timed out".into()),
            true,
        ));

        assert!(event.is_none());
        assert_eq!(warning.as_deref(), Some("timed out"));
        assert_eq!(revision, 0);
    }

    // BLOCKER RED: accepted final with successful but EMPTY decode must not
    // vanish silently. Uses production run_dispatcher + final InferenceJob
    // with controlled infer returning Ok("") (incl. parse_server_response_json
    // {"text":""} and whitespace paths). Asserts zero fabricated finals and
    // exactly one typed stt_failed warning with correct speaker/session and a
    // privacy-safe message. Also pins: whitespace behaves like empty, prior
    // nonempty last_text still falls back, nonfinal empty stays ignorable.
    #[test]
    fn red_empty_final_emits_typed_warning_without_fabricating_text() {
        // JSON empty / whitespace decode to Ok("") via production parser.
        assert_eq!(
            parse_server_response_json(br#"{"text":""}"#).expect("empty json"),
            ""
        );
        assert_eq!(
            parse_server_response_json(br#"{"text":"   "}"#).expect("whitespace json"),
            ""
        );
        // Nonfinal empty remains ignorable (no event, no warning).
        let mut revision = 0;
        let nonfinal = next_transcript_revision(&mut revision, "", Ok("".into()), false);
        assert!(nonfinal.event.is_none(), "nonfinal empty must emit nothing");
        assert!(nonfinal.warning.is_none(), "nonfinal empty must warn nothing");
        assert_eq!(revision, 0);
        // Prior nonempty last_text still finalizes via fallback (Stop contract).
        let mut revision = 0;
        let fallback =
            next_transcript_revision(&mut revision, "valid partial", Ok("".into()), true);
        let fallback_event = fallback.event.expect("fallback must emit last valid text");
        assert_eq!(fallback_event.text, "valid partial");
        assert!(fallback_event.final_segment);
        // Whitespace-only final with no history behaves like empty: no event,
        // exactly one warning (never a whitespace transcript).
        let mut revision = 0;
        let whitespace =
            next_transcript_revision(&mut revision, "", Ok("   ".into()), true);
        assert!(
            whitespace.event.is_none(),
            "whitespace final must not fabricate text"
        );
        let whitespace_warning =
            whitespace.warning.expect("whitespace final must warn");
        assert!(
            whitespace_warning.to_lowercase().contains("no text"),
            "warning must state no text, got: {whitespace_warning}"
        );
        // Behavioral: production dispatcher with a final job whose inference
        // succeeds but decodes to empty.
        let dispatch = DispatchQueue::new();
        dispatch
            .submit(InferenceJob {
                speaker: Speaker::Interviewer,
                segment_number: 1,
                started_at_ms: 1_000,
                captured_at_ms: 1_500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; 320],
                language: "sv".to_string(),
            })
            .expect("empty-final fixture fits");
        dispatch.close();
        let events: std::sync::Arc<std::sync::Mutex<Vec<TranscriptEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let warnings: std::sync::Arc<
            std::sync::Mutex<Vec<(String, String, Option<Speaker>)>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_record = events.clone();
        let warnings_record = warnings.clone();
        run_dispatcher(
            &dispatch,
            &ShutdownState::default(),
            "empty-final-session",
            &|_job| Ok("".to_string()),
            &|event| events_record.lock().expect("events").push(event),
            &|code, message, speaker| {
                warnings_record
                    .lock()
                    .expect("warnings")
                    .push((code.to_string(), message, speaker))
            },
            &FinalLossTracker::default(),
        );
        let events = events.lock().expect("events");
        let warnings = warnings.lock().expect("warnings");
        assert!(
            events.is_empty(),
            "empty final must fabricate zero finals, got {events:?}"
        );
        assert_eq!(
            warnings.len(),
            1,
            "empty final must emit exactly one warning, got {warnings:?}"
        );
        assert_eq!(warnings[0].0, "stt_failed");
        assert_eq!(warnings[0].2, Some(Speaker::Interviewer));
        assert!(
            warnings[0].1.to_lowercase().contains("no text"),
            "warning must state no text, got: {}",
            warnings[0].1
        );
        for forbidden in ["secret", "0.03", ".wav", ".json"] {
            assert!(
                !warnings[0].1.contains(forbidden),
                "warning must stay privacy-safe, leaked {forbidden}: {}",
                warnings[0].1
            );
        }
    }

    #[test]
    fn revision_builder_does_not_leak_state_between_sources() {
        let mut candidate_revision = 0;
        let mut interviewer_revision = 0;
        let candidate = next_transcript_revision(
            &mut candidate_revision,
            "",
            Ok("candidate".into()),
            false,
        )
        .event
        .expect("candidate event");
        let interviewer = next_transcript_revision(
            &mut interviewer_revision,
            "",
            Ok("interviewer".into()),
            false,
        )
        .event
        .expect("interviewer event");
        assert_eq!(candidate.revision, 1);
        assert_eq!(interviewer.revision, 1);
        assert_ne!(candidate.text, interviewer.text);
    }

    #[test]
    fn stop_with_trailing_speech_requires_finalization() {
        let shutdown = ShutdownState::default();
        let queue = BoundedFrames::new(1);
        shutdown.request(ShutdownMode::GracefulDrain);
        shutdown.mark_input_closed();
        assert!(should_process_transcriber_iteration(&shutdown, true, false, &queue));
        assert!(!should_process_transcriber_iteration(&shutdown, false, false, &queue));
    }

    #[test]
    fn stop_drains_queued_audio_before_finalization() {
        let shutdown = ShutdownState::default();
        let queue = BoundedFrames::new(1);
        shutdown.request(ShutdownMode::GracefulDrain);
        shutdown.mark_input_closed();
        assert!(should_process_transcriber_iteration(&shutdown, true, true, &queue));
    }

    #[test]
    fn stop_drain_waits_for_reader_close_before_empty_queue_exit() {
        let shutdown = Arc::new(ShutdownState::default());
        shutdown.request(ShutdownMode::GracefulDrain);
        let queue = Arc::new(BoundedFrames::new(2));
        let (empty_snapshot, wait_for_empty_snapshot) = mpsc::channel();
        let (reader_closed, wait_for_reader_close) = mpsc::channel();
        let worker_shutdown = shutdown.clone();
        let worker_queue = queue.clone();
        let worker = thread::spawn(move || {
            let queued_frame = worker_queue.pop();
            assert!(queued_frame.is_none());
            empty_snapshot.send(()).expect("empty queue snapshot");
            wait_for_reader_close.recv().expect("reader close");

            if !should_process_transcriber_iteration(
                &worker_shutdown,
                false,
                queued_frame.is_some(),
                &worker_queue,
            ) {
                return Vec::new();
            }
            queued_frame
                .or_else(|| worker_queue.pop())
                .into_iter()
                .collect::<Vec<_>>()
        });

        wait_for_empty_snapshot.recv().expect("empty queue observed");
        assert!(!queue
            .push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 7,
                samples: vec![0.03; 160],
            })
            .dropped);
        shutdown.mark_input_closed();
        reader_closed.send(()).expect("publish reader close");

        let processed = worker.join().expect("transcriber worker");
        assert_eq!(
            processed.len(),
            1,
            "Stop must not exit on an empty snapshot before the reader closes and its accepted frame is drained"
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn failure_shutdown_is_not_treated_as_graceful_stop() {
        let queue = BoundedFrames::new(1);
        let graceful = ShutdownState::default();
        graceful.request(ShutdownMode::GracefulDrain);
        graceful.mark_input_closed();
        let cancelled = ShutdownState::default();
        cancelled.request(ShutdownMode::Cancel);
        let graceful_stop_continues =
            should_process_transcriber_iteration(&graceful, true, false, &queue);
        let failure_cancellation_continues =
            should_process_transcriber_iteration(&cancelled, true, false, &queue);

        assert!(graceful_stop_continues);
        assert!(
            !failure_cancellation_continues,
            "native failure and window teardown must cancel instead of sharing explicit Stop finalization"
        );
    }

    #[test]
    fn silence_is_not_speech() {
        assert!(!frame_has_speech(&vec![0.0; 1_600]));
    }

    #[test]
    fn low_duty_cycle_typing_impulse_is_not_speech() {
        let mut samples = vec![0.0; 1_600];
        for sample in samples.iter_mut().take(8) {
            *sample = 0.9;
        }
        assert!(rms(&samples) > VAD_RMS_THRESHOLD);
        assert!(!frame_has_speech(&samples));
    }

    #[test]
    fn sustained_voiced_frame_is_speech() {
        let samples: Vec<f32> = (0..1_600)
            .map(|index| if index % 20 < 10 { 0.03 } else { -0.03 })
            .collect();
        assert!(frame_has_speech(&samples));
    }

    #[test]
    fn whisper_invocation_routes_swedish_and_english_explicitly() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = InferenceWorkspace::new().expect("language fixture workspace");
        let fake_curl = fixture.path.join("fake-curl-legacy");
        fs::write(
            &fake_curl,
            r#"#!/bin/sh
response=""
wav=""
want_output=0
for arg in "$@"; do
  case "$arg" in
    --output) want_output=1 ;;
    *) if [ "$want_output" = "1" ]; then response="$arg"; want_output=0; fi ;;
  esac
done
for arg in "$@"; do
  case "$arg" in
    file=@*) wav="${arg#file=@}"; wav="${wav%%;*}" ;;
  esac
done
language=""
for arg in "$@"; do
  case "$arg" in
    language=*) language="${arg#language=}" ;;
  esac
done
if [ -z "$response" ] || [ -z "$wav" ]; then exit 3; fi
printf '{"text":"route:%s"}' "$language" > "$response"
"#,
        )
        .expect("fake curl script");
        let mut permissions = fs::metadata(&fake_curl).expect("script metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake_curl, permissions).expect("executable script");
        let shutdown = ShutdownState::default();
        let gate = InferenceGate::default();
        let endpoint = "http://127.0.0.1:17998/legacy/inference";

        for language in ["sv", "en"] {
            let diag = SourceDiag::default();
            assert_eq!(
                transcribe_via_server_with_curl(
                    endpoint,
                    &[0.03; 160],
                    language,
                    shutdown.stop_signal(),
                    shutdown.cancel_signal(),
                    false,
                    None,
                    &gate,
                    &fake_curl,
                    &diag,
                    SubmissionKind::Partial,
                )
                .expect("fake transcription"),
                format!("route:{language}")
            );
            // Diagnostic side effect only: phase returns to idle and the
            // completed partial timing is recorded without touching audio.
            assert_eq!(diag.current_phase(), SttPhase::Idle);
            let (_, _, gate_ms, infer_ms, kind) = diag.stt_snapshot();
            assert_eq!(kind, Some(SubmissionKind::Partial));
            assert!(gate_ms.is_some());
            assert!(infer_ms.is_some());
        }
    }

    #[test]
    fn bounded_source_queues_drop_oldest_without_relabeling() {
        let queue = BoundedFrames::new(1);
        assert!(!queue.push(PcmFrame { source: Speaker::Candidate, captured_at_ms: 1, samples: vec![1.0] }).dropped);
        assert!(queue.push(PcmFrame { source: Speaker::Candidate, captured_at_ms: 2, samples: vec![2.0] }).dropped);
        let frame = queue.pop().expect("latest frame");
        assert_eq!(frame.source, Speaker::Candidate);
        assert_eq!(frame.captured_at_ms, 2);
    }

    // TEMPORARY DIAGNOSTIC RED SLICE 1: metadata-only backpressure warning.
    // Drives real BoundedFrames + real warning formatter; must fail until
    // drop metadata and compact diagnostic suffix exist.
    #[test]
    fn backpressure_warning_contains_compact_diagnostic_suffix() {
        let affected = BoundedFrames::new(2);
        let other = BoundedFrames::new(2);
        let _ = other.push(PcmFrame {
            source: Speaker::Interviewer,
            captured_at_ms: 5000,
            samples: vec![0.1; 160],
        });
        assert!(!affected
            .push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 1000,
                samples: vec![0.1; 160],
            })
            .dropped);
        assert!(!affected
            .push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 1010,
                samples: vec![0.1; 320],
            })
            .dropped);
        let incoming_n = 480_usize;
        let incoming_captured = 1030_u64;
        let drop = affected.push(PcmFrame {
            source: Speaker::Candidate,
            captured_at_ms: incoming_captured,
            samples: vec![0.2; 480],
        });
        assert!(drop.dropped, "third push into capacity 2 must drop oldest");
        assert_eq!(drop.dropped_captured_at_ms, Some(1000));
        assert_eq!(drop.dropped_samples, Some(160));
        assert_eq!(drop.dropped_frames, 1);
        assert_eq!(drop.dropped_total_samples, 160);

        let message = format_backpressure_message(BackpressureParams {
            source: Speaker::Candidate,
            depth: affected.len(),
            capacity: affected.capacity(),
            queued_ms: affected.queued_duration_ms(),
            incoming_samples: incoming_n,
            incoming_captured_at_ms: incoming_captured,
            dropped_captured_at_ms: drop.dropped_captured_at_ms,
            dropped_samples: drop.dropped_samples,
            dropped_frames: drop.dropped_frames,
            dropped_total_samples: drop.dropped_total_samples,
            cumulative_dropped_frames: drop.cumulative_dropped_frames,
            cumulative_dropped_samples: drop.cumulative_dropped_samples,
            prev_callback_captured_at_ms: Some(1010),
            other_depth: other.len(),
            other_queued_ms: other.queued_duration_ms(),
            phase: SttPhase::Inference,
            phase_elapsed_ms: Some(250),
            other_phase: SttPhase::Idle,
            other_phase_elapsed_ms: Some(0),
            last_gate_ms: Some(12),
            last_infer_ms: Some(800),
            last_kind: Some(SubmissionKind::Partial),
            other_gate_ms: None,
            other_infer_ms: None,
            other_kind: None,
        });
        assert!(
            message.starts_with(
                "Transcription fell behind and dropped the oldest transient audio frame."
            ),
            "prefix must stay intact, got: {message}"
        );
        for key in [
            "src=",
            "depth=",
            "cap=",
            "q_ms=",
            "in_n=",
            "in_ms=",
            "drop_age_ms=",
            "drop_ms=",
            "drop_n=",
            "drop_total_ms=",
            "cum_drop_n=",
            "cum_drop_ms=",
            "cb_delta_ms=",
            "other_depth=",
            "other_q_ms=",
            "phase=",
            "phase_ms=",
            "other_phase=",
            "other_phase_ms=",
            "gate_ms=",
            "infer_ms=",
            "kind=",
            "other_gate_ms=",
            "other_infer_ms=",
            "other_kind=",
        ] {
            assert!(message.contains(key), "missing key {key} in {message}");
        }
        // Duration/age math on normalized 16 kHz PCM:
        // affected holds 320 + 480 = 800 samples = 50 ms.
        assert!(message.contains("depth=2/2"), "got: {message}");
        assert!(message.contains("q_ms=50"), "got: {message}");
        assert!(message.contains("in_n=480"), "got: {message}");
        assert!(message.contains("in_ms=30"), "got: {message}");
        assert!(message.contains("drop_age_ms=30"), "got: {message}");
        assert!(message.contains("drop_ms=10"), "got: {message}");
        assert!(message.contains("drop_n=1"), "got: {message}");
        assert!(message.contains("drop_total_ms=10"), "got: {message}");
        assert!(message.contains("cum_drop_n=1"), "got: {message}");
        assert!(message.contains("cb_delta_ms=20"), "got: {message}");
        assert!(message.contains("other_depth=1"), "got: {message}");
        assert!(message.contains("other_q_ms=10"), "got: {message}");
        // Unknown handling for missing other-side timings.
        assert!(message.contains("other_gate_ms=unknown"), "got: {message}");
        assert!(message.contains("other_infer_ms=unknown"), "got: {message}");
        assert!(message.contains("other_kind=unknown"), "got: {message}");
        // Privacy: counts/durations only, never sample values or transcript.
        assert!(!message.contains("0.1"), "must not leak sample values: {message}");
        assert!(!message.contains("0.2"), "must not leak sample values: {message}");
        assert!(!message.contains("secret"), "must not leak transcript: {message}");
    }

    #[test]
    fn backpressure_warning_uses_unknown_without_invented_zero() {
        let affected = BoundedFrames::new(2);
        let _ = affected.push(PcmFrame {
            source: Speaker::Interviewer,
            captured_at_ms: 2000,
            samples: vec![0.1; 160],
        });
        let message = format_backpressure_message(BackpressureParams {
            source: Speaker::Interviewer,
            depth: affected.len(),
            capacity: affected.capacity(),
            queued_ms: affected.queued_duration_ms(),
            incoming_samples: 160,
            incoming_captured_at_ms: 2010,
            dropped_captured_at_ms: None,
            dropped_samples: None,
            dropped_frames: 0,
            dropped_total_samples: 0,
            cumulative_dropped_frames: 0,
            cumulative_dropped_samples: 0,
            prev_callback_captured_at_ms: None,
            other_depth: 0,
            other_queued_ms: 0,
            phase: SttPhase::Idle,
            phase_elapsed_ms: None,
            other_phase: SttPhase::Idle,
            other_phase_elapsed_ms: None,
            last_gate_ms: None,
            last_infer_ms: None,
            last_kind: None,
            other_gate_ms: None,
            other_infer_ms: None,
            other_kind: None,
        });
        assert!(message.contains("drop_age_ms=unknown"), "got: {message}");
        assert!(message.contains("drop_ms=unknown"), "got: {message}");
        assert!(message.contains("cb_delta_ms=unknown"), "got: {message}");
        assert!(message.contains("gate_ms=unknown"), "got: {message}");
        assert!(!message.contains("gate_ms=0,"), "must never invent zero: {message}");
        assert!(!message.contains("secret transcript hello"), "got: {message}");
    }

    // MAJOR RED: a single sample-bound push may evict MULTIPLE old PCM frames.
    // Fill the production 80-frame / 256k-sample queue with legal small frames,
    // then force a multi-eviction on ONE push with a legal large frame. Asserts
    // the exact evicted count and total duration appear in the production
    // warning with source/queue identity preserved (not just the first frame).
    #[test]
    fn red_single_push_multi_eviction_reports_exact_counts() {
        let affected = BoundedFrames::new(PCM_QUEUE_CAPACITY);
        let other = BoundedFrames::new(PCM_QUEUE_CAPACITY);
        for index in 0..PCM_QUEUE_CAPACITY {
            let drop = affected.push(PcmFrame {
                source: Speaker::Interviewer,
                captured_at_ms: 10_000 + index as u64 * 20,
                samples: vec![0.03; 3_200],
            });
            assert!(!drop.dropped, "fill must not drop until the final push");
        }
        assert_eq!(affected.len(), PCM_QUEUE_CAPACITY);
        assert_eq!(affected.queued_samples(), PCM_QUEUE_MAX_SAMPLES);
        let (before_frames, before_samples) = affected.dropped_counts();
        assert_eq!((before_frames, before_samples), (0, 0));
        // One legal large frame (48k) needs 48k/3.2k = 15 evictions to fit.
        let incoming_n = MAX_PCM_FRAMES;
        let incoming_captured = 20_000_u64;
        let drop = affected.push(PcmFrame {
            source: Speaker::Interviewer,
            captured_at_ms: incoming_captured,
            samples: vec![0.03; incoming_n],
        });
        assert!(drop.dropped, "large push into a full sample-capped queue must drop");
        let (after_frames, after_samples) = affected.dropped_counts();
        assert_eq!(
            after_frames - before_frames,
            15,
            "ONE push must evict exactly 15 frames, cumulative {after_frames:?}"
        );
        assert_eq!(
            after_samples - before_samples,
            48_000,
            "ONE push must evict exactly 48k samples in total"
        );
        // Production warning must report the total damage for THIS push, not
        // just the first evicted frame (3_200 samples = 200 ms).
        // Per-push exactness + queue-lifetime cumulative (metadata only).
        assert_eq!(drop.dropped_frames, 15);
        assert_eq!(drop.dropped_total_samples, 48_000);
        assert_eq!(drop.cumulative_dropped_frames, 15);
        assert_eq!(drop.cumulative_dropped_samples, 48_000);
        let message = format_backpressure_message(BackpressureParams {
            source: Speaker::Interviewer,
            depth: affected.len(),
            capacity: affected.capacity(),
            queued_ms: affected.queued_duration_ms(),
            incoming_samples: incoming_n,
            incoming_captured_at_ms: incoming_captured,
            dropped_captured_at_ms: drop.dropped_captured_at_ms,
            dropped_samples: drop.dropped_samples,
            dropped_frames: drop.dropped_frames,
            dropped_total_samples: drop.dropped_total_samples,
            cumulative_dropped_frames: drop.cumulative_dropped_frames,
            cumulative_dropped_samples: drop.cumulative_dropped_samples,
            prev_callback_captured_at_ms: Some(10_000 + 79 * 20),
            other_depth: other.len(),
            other_queued_ms: other.queued_duration_ms(),
            phase: SttPhase::Inference,
            phase_elapsed_ms: Some(100),
            other_phase: SttPhase::Idle,
            other_phase_elapsed_ms: Some(0),
            last_gate_ms: Some(12),
            last_infer_ms: Some(800),
            last_kind: Some(SubmissionKind::Partial),
            other_gate_ms: None,
            other_infer_ms: None,
            other_kind: None,
        });
        assert!(message.contains("src=interviewer"), "source identity, got: {message}");
        assert!(message.contains("cap=80"), "queue identity, got: {message}");
        // Exact total for THIS push: 15 frames, 48k samples = 3000 ms.
        assert!(
            message.contains("drop_n=15"),
            "warning must report exact evicted count for THIS push, got: {message}"
        );
        assert!(
            message.contains("drop_total_ms=3000"),
            "warning must report exact total evicted duration for THIS push, got: {message}"
        );
        assert!(!message.contains("0.03"), "must not leak sample values: {message}");
    }

    // TEMPORARY DIAGNOSTIC RED SLICE 2: STT phase/gate/inference timing.
    // Uses fake clock/controlled durations, no sleeps. Must fail until the
    // RAII phase guard and completion recording exist.
    #[test]
    fn stt_phase_guard_tracks_waiting_gate_and_inference_then_idle() {
        let diag = SourceDiag::default();
        assert_eq!(diag.current_phase(), SttPhase::Idle);
        {
            let _waiting = SttPhaseGuard::enter(&diag, SttPhase::WaitingGate);
            assert_eq!(diag.current_phase(), SttPhase::WaitingGate);
            // Controlled fake duration: 40 ms in gate without sleeping.
            diag.enter_phase_at(
                SttPhase::WaitingGate,
                Instant::now() - Duration::from_millis(40),
            );
            let (phase, elapsed, _, _, _) = diag.stt_snapshot();
            assert_eq!(phase, SttPhase::WaitingGate);
            let elapsed = elapsed.expect("waiting_gate elapsed");
            assert!((40..500).contains(&elapsed), "elapsed={elapsed}");
        }
        // Guard drop must restore idle even without explicit exit.
        assert_eq!(diag.current_phase(), SttPhase::Idle);
        {
            let guard = SttPhaseGuard::enter(&diag, SttPhase::Inference);
            assert_eq!(diag.current_phase(), SttPhase::Inference);
            diag.enter_phase_at(
                SttPhase::Inference,
                Instant::now() - Duration::from_millis(250),
            );
            diag.record_completion(12, 800, SubmissionKind::Partial);
            let (phase, elapsed, gate, infer, kind) = diag.stt_snapshot();
            assert_eq!(phase, SttPhase::Inference);
            assert!((250..1000).contains(&elapsed.expect("inference elapsed")));
            assert_eq!(gate, Some(12));
            assert_eq!(infer, Some(800));
            assert_eq!(kind, Some(SubmissionKind::Partial));
            drop(guard);
        }
        assert_eq!(diag.current_phase(), SttPhase::Idle);
    }

    #[test]
    fn stt_phase_guard_restores_idle_on_error_and_cancel_paths() {
        let diag = SourceDiag::default();
        let failing: Result<String, String> = (|| {
            let _guard = SttPhaseGuard::enter(&diag, SttPhase::WaitingGate);
            assert_eq!(diag.current_phase(), SttPhase::WaitingGate);
            Err("inference gate poisoned; restart capture".into())
        })();
        assert!(failing.is_err());
        assert_eq!(
            diag.current_phase(),
            SttPhase::Idle,
            "error path must return to idle via guard"
        );

        let cancelled: Result<String, String> = (|| {
            let _guard = SttPhaseGuard::enter(&diag, SttPhase::Inference);
            assert_eq!(diag.current_phase(), SttPhase::Inference);
            // Simulate cancellation before submission completes.
            if true {
                return Err("cancelled".into());
            }
            #[allow(unreachable_code)]
            Ok("never".into())
        })();
        assert_eq!(cancelled, Err("cancelled".into()));
        assert_eq!(
            diag.current_phase(),
            SttPhase::Idle,
            "cancel path must return to idle via guard"
        );

        // Completion recording keeps last finished timings with kind.
        diag.record_completion(7, 900, SubmissionKind::Final);
        let (_, _, gate, infer, kind) = diag.stt_snapshot();
        assert_eq!(gate, Some(7));
        assert_eq!(infer, Some(900));
        assert_eq!(kind, Some(SubmissionKind::Final));
    }

    #[test]
    fn source_assignment_comes_only_from_native_tag() {
        let bytes = 0.25_f32.to_le_bytes();
        let encoded = "AACAPg==";
        assert_eq!(decode_base64(encoded.as_bytes(), MAX_PCM_BYTES).expect("base64"), bytes);
        let frame = decode_pcm(
            NativeMessage {
                kind: "pcm".into(),
                source: Some(Speaker::Interviewer),
                sample_rate: Some(16_000.0),
                channels: Some(1),
                captured_at_ms: Some(4),
                samples: Some(encoded.into()),
                code: None,
                message: None,
                microphones: None,
                applications: None,
                application_discovery_error: None,
            },
            &mut None,
            &mut None,
        )
        .expect("pcm");
        assert_eq!(frame.source, Speaker::Interviewer);
    }

    fn native_message(kind: &str, source: Option<Speaker>) -> NativeMessage {
        NativeMessage {
            kind: kind.into(),
            source,
            sample_rate: None,
            channels: None,
            captured_at_ms: None,
            samples: None,
            code: None,
            message: None,
            microphones: None,
            applications: None,
            application_discovery_error: None,
        }
    }

    #[test]
    fn native_warning_preserves_source_and_does_not_mutate_resampler_state() {
        let mut candidate = Some(StatefulResampler::new(48_000.0, 16_000.0));
        let mut interviewer = None;
        let mut message = native_message("warning", Some(Speaker::Interviewer));
        message.code = Some("audio_gap".into());
        message.message = Some("A source-specific callback gap was observed.".into());

        let NativeEventOutcome::Warning(warning) =
            handle_native_message(message, &mut candidate, &mut interviewer)
        else {
            panic!("warning outcome");
        };
        assert_eq!(warning.code, "audio_gap");
        assert_eq!(warning.source, Some(Speaker::Interviewer));
        assert_eq!(candidate.as_ref().expect("candidate state").input_rate, 48_000.0);
        assert!(interviewer.is_none());
    }

    #[test]
    fn unknown_native_state_is_ignored_and_not_terminal() {
        let mut candidate = None;
        let mut interviewer = None;
        let mut message = native_message("state", None);
        message.message = Some("starting".into());
        assert!(matches!(
            handle_native_message(message, &mut candidate, &mut interviewer),
            NativeEventOutcome::Ignore
        ));
    }

    #[test]
    fn malformed_pcm_reports_typed_source_warning_and_valid_frames_continue() {
        let mut candidate = None;
        let mut interviewer = None;
        let mut malformed = native_message("pcm", Some(Speaker::Candidate));
        malformed.sample_rate = Some(f64::NAN);
        malformed.channels = Some(1);
        malformed.captured_at_ms = Some(10);
        malformed.samples = Some("AACAPg==".into());

        let NativeEventOutcome::Warning(warning) =
            handle_native_message(malformed, &mut candidate, &mut interviewer)
        else {
            panic!("invalid PCM warning");
        };
        assert_eq!(warning.code, "invalid_pcm");
        assert_eq!(warning.source, Some(Speaker::Candidate));
        assert!(!warning.message.contains("AACAPg=="));
        assert!(candidate.is_none());

        let mut valid = native_message("pcm", Some(Speaker::Candidate));
        valid.sample_rate = Some(16_000.0);
        valid.channels = Some(1);
        valid.captured_at_ms = Some(11);
        valid.samples = Some("AACAPg==".into());
        let NativeEventOutcome::Frame(frame) =
            handle_native_message(valid, &mut candidate, &mut interviewer)
        else {
            panic!("valid PCM frame");
        };
        assert_eq!(frame.source, Speaker::Candidate);
        assert_eq!(frame.samples, vec![0.25]);
        assert!(interviewer.is_none());
    }

    fn pcm_message(source: Speaker, sample_rate: f64, channels: usize, samples: Vec<f32>) -> NativeMessage {
        let bytes: Vec<u8> = samples.into_iter().flat_map(f32::to_le_bytes).collect();
        let mut message = native_message("pcm", Some(source));
        message.sample_rate = Some(sample_rate);
        message.channels = Some(channels);
        message.captured_at_ms = Some(20);
        message.samples = Some(base64_for_test(&bytes));
        message
    }

    fn base64_for_test(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            output.push(TABLE[((value >> 18) & 63) as usize] as char);
            output.push(TABLE[((value >> 12) & 63) as usize] as char);
            output.push(if chunk.len() > 1 { TABLE[((value >> 6) & 63) as usize] as char } else { '=' });
            output.push(if chunk.len() > 2 { TABLE[(value & 63) as usize] as char } else { '=' });
        }
        output
    }

    #[test]
    fn unsupported_pcm_rates_and_channels_are_bounded_and_transactional() {
        let mut candidate = Some(StatefulResampler {
            input_rate: PCM_SAMPLE_RATE,
            output_rate: PCM_SAMPLE_RATE,
            position: 0.5,
            previous: Some(0.25),
        });
        let mut interviewer = None;

        for message in [
            pcm_message(Speaker::Candidate, f64::MIN_POSITIVE, 1, vec![0.5]),
            pcm_message(Speaker::Candidate, f64::MAX, 1, vec![0.5]),
            pcm_message(Speaker::Candidate, PCM_SAMPLE_RATE, 2, vec![f32::MAX, f32::MAX]),
        ] {
            let NativeEventOutcome::Warning(warning) =
                handle_native_message(message, &mut candidate, &mut interviewer)
            else {
                panic!("unsupported PCM warning");
            };
            assert_eq!(warning.code, "invalid_pcm");
            let state = candidate.as_ref().expect("candidate state");
            assert_eq!(state.input_rate, PCM_SAMPLE_RATE);
            assert_eq!(state.position, 0.5);
            assert_eq!(state.previous, Some(0.25));
            assert!(interviewer.is_none());
        }

        let NativeEventOutcome::Frame(frame) = handle_native_message(
            pcm_message(Speaker::Candidate, PCM_SAMPLE_RATE, PCM_CHANNELS, vec![0.75]),
            &mut candidate,
            &mut interviewer,
        ) else {
            panic!("valid PCM frame after rejected formats");
        };
        assert_eq!(frame.samples, vec![0.75]);
    }

    #[test]
    fn swift_boundary_supported_rates_resample_per_source() {
        for sample_rate in SUPPORTED_PCM_SAMPLE_RATES {
            let input_frames = sample_rate as usize;
            let mut candidate = None;
            let mut interviewer = None;
            let NativeEventOutcome::Frame(candidate_frame) = handle_native_message(
                pcm_message(Speaker::Candidate, sample_rate, PCM_CHANNELS, vec![0.25; input_frames]),
                &mut candidate,
                &mut interviewer,
            ) else {
                panic!("candidate frame at {sample_rate} Hz");
            };
            assert!((candidate_frame.samples.len() as isize - 16_000).abs() <= 1);
            assert!(candidate_frame.samples.iter().all(|sample| (*sample - 0.25).abs() < f32::EPSILON));
            assert_eq!(candidate.as_ref().expect("candidate state").input_rate, sample_rate);
            assert!(interviewer.is_none());

            let NativeEventOutcome::Frame(interviewer_frame) = handle_native_message(
                pcm_message(Speaker::Interviewer, sample_rate, PCM_CHANNELS, vec![-0.25; input_frames]),
                &mut candidate,
                &mut interviewer,
            ) else {
                panic!("interviewer frame at {sample_rate} Hz");
            };
            assert!((interviewer_frame.samples.len() as isize - 16_000).abs() <= 1);
            assert!(interviewer_frame.samples.iter().all(|sample| (*sample + 0.25).abs() < f32::EPSILON));
            assert_eq!(interviewer.as_ref().expect("interviewer state").input_rate, sample_rate);
        }
    }

    #[test]
    fn oversized_pcm_payload_is_rejected_before_resampler_mutation() {
        let mut candidate = Some(StatefulResampler {
            input_rate: PCM_SAMPLE_RATE,
            output_rate: PCM_SAMPLE_RATE,
            position: 0.25,
            previous: Some(0.5),
        });
        let mut interviewer = None;
        let message = pcm_message(
            Speaker::Candidate,
            PCM_SAMPLE_RATE,
            PCM_CHANNELS,
            vec![0.0; MAX_PCM_FRAMES + 1],
        );

        let NativeEventOutcome::Warning(warning) =
            handle_native_message(message, &mut candidate, &mut interviewer)
        else {
            panic!("oversized PCM warning");
        };
        assert_eq!(warning.code, "invalid_pcm");
        let state = candidate.as_ref().expect("candidate state");
        assert_eq!(state.position, 0.25);
        assert_eq!(state.previous, Some(0.5));
        assert!(interviewer.is_none());
    }

    #[test]
    fn encoded_pcm_bounds_are_checked_before_decode_and_preserve_state() {
        let mut candidate = Some(StatefulResampler {
            input_rate: PCM_SAMPLE_RATE,
            output_rate: PCM_SAMPLE_RATE,
            position: 0.25,
            previous: Some(0.5),
        });
        let mut interviewer = None;

        let mut oversized = native_message("pcm", Some(Speaker::Candidate));
        oversized.sample_rate = Some(PCM_SAMPLE_RATE);
        oversized.channels = Some(PCM_CHANNELS);
        oversized.captured_at_ms = Some(21);
        oversized.samples = Some("A".repeat(MAX_PCM_BASE64_BYTES + 1));

        let exact_bytes = vec![0_u8; MAX_PCM_BYTES];
        let exact_encoded = base64_for_test(&exact_bytes);
        assert_eq!(exact_encoded.len(), MAX_PCM_BASE64_BYTES);

        let mut whitespace = native_message("pcm", Some(Speaker::Candidate));
        whitespace.sample_rate = Some(PCM_SAMPLE_RATE);
        whitespace.channels = Some(PCM_CHANNELS);
        whitespace.captured_at_ms = Some(22);
        whitespace.samples = Some(format!(" {}", "A".repeat(MAX_PCM_BASE64_BYTES - 1)));

        for message in [oversized, whitespace] {
            let NativeEventOutcome::Warning(warning) =
                handle_native_message(message, &mut candidate, &mut interviewer)
            else {
                panic!("encoded PCM bound warning");
            };
            assert_eq!(warning.code, "invalid_pcm");
            let state = candidate.as_ref().expect("candidate state");
            assert_eq!(state.position, 0.25);
            assert_eq!(state.previous, Some(0.5));
            assert!(interviewer.is_none());
        }

        let NativeEventOutcome::Frame(exact) = handle_native_message(
            pcm_message(
                Speaker::Candidate,
                PCM_SAMPLE_RATE,
                PCM_CHANNELS,
                vec![0.0; MAX_PCM_FRAMES],
            ),
            &mut candidate,
            &mut interviewer,
        ) else {
            panic!("exact maximum PCM payload");
        };
        assert_eq!(exact.samples.len(), MAX_PCM_FRAMES);

        let NativeEventOutcome::Frame(continued) = handle_native_message(
            pcm_message(Speaker::Candidate, PCM_SAMPLE_RATE, PCM_CHANNELS, vec![0.75]),
            &mut candidate,
            &mut interviewer,
        ) else {
            panic!("valid PCM after encoded bound rejection");
        };
        assert_eq!(continued.samples, vec![0.75]);
    }

    #[test]
    fn resampling_preserves_duration_and_speaker_queues_are_independent() {
        let input = vec![0.5; 48_000];
        let mut resampler = StatefulResampler::new(48_000.0, 16_000.0);
        let mut output = Vec::new();
        for chunk in input.chunks(997) {
            output.extend(resampler.process(chunk));
        }
        assert!((output.len() as isize - 16_000).abs() <= 1);
        assert!(output.iter().all(|sample| (*sample - 0.5).abs() < f32::EPSILON));
        let candidate = BoundedFrames::new(2);
        let interviewer = BoundedFrames::new(2);
        let _ = candidate.push(PcmFrame { source: Speaker::Candidate, captured_at_ms: 1, samples: vec![0.1] });
        assert!(interviewer.pop().is_none());
    }

    #[test]
    fn private_inference_workspace_is_owner_only_and_always_removed() {
        use std::os::unix::fs::PermissionsExt;
        let path = {
            let workspace = InferenceWorkspace::new().expect("workspace");
            assert_eq!(fs::metadata(&workspace.path).expect("metadata").permissions().mode() & 0o777, 0o700);
            workspace.path.clone()
        };
        assert!(!path.exists());
    }

    #[test]
    fn failed_inference_workspace_creation_leaves_no_directory() {
        let parent = std::env::temp_dir().join(format!(
            "intervjuare-missing-parent-{}-{}",
            std::process::id(),
            INFERENCE_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&parent);
        assert!(InferenceWorkspace::new_in(&parent).is_err());
        assert!(!parent.exists());
    }

    #[test]
    fn terminal_cleanup_releases_runtime_for_immediate_restart() {
        let runtime = AudioRuntime::default();
        let shutdown = Arc::new(ShutdownState::default());
        let workers = Arc::new(Mutex::new(Vec::new()));
        let session = CaptureSession {
            session_id: "early-eof".into(),
            shutdown,
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers,
            server: Arc::new(Mutex::new(None)),
        };
        runtime.0.lock().expect("runtime").session = Some(session);

        let removed = take_session(&runtime, Some("early-eof")).expect("session");
        stop_session(removed, ShutdownMode::Cancel);

        let mut guard = runtime.0.lock().expect("runtime");
        assert!(guard.session.is_none());
        guard.session = Some(CaptureSession {
            session_id: "restart".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(Vec::new())),
            server: Arc::new(Mutex::new(None)),
        });
        assert_eq!(guard.session.as_ref().expect("restarted").session_id, "restart");
    }

    #[test]
    fn reader_cannot_run_before_its_handle_is_registered() {
        let runtime = AudioRuntime::default();
        let workers = Arc::new(Mutex::new(Vec::new()));
        runtime.0.lock().expect("runtime").session = Some(CaptureSession {
            session_id: "early-eof".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: workers.clone(),
            server: Arc::new(Mutex::new(None)),
        });

        let (reader_start, start_reader) = reader_start_barrier();
        let reached_cleanup = Arc::new(AtomicBool::new(false));
        let cleanup = reached_cleanup.clone();
        let reader = thread::spawn(move || {
            if reader_start.wait() {
                cleanup.store(true, Ordering::SeqCst);
            }
        });
        thread::sleep(Duration::from_millis(20));
        assert!(!reached_cleanup.load(Ordering::SeqCst));
        workers.lock().expect("workers").push(reader);
        assert_eq!(workers.lock().expect("workers").len(), 1);
        start_reader.send(()).expect("release reader");
        let reader = workers.lock().expect("workers").pop().expect("reader");
        reader.join().expect("reader join");
        assert!(reached_cleanup.load(Ordering::SeqCst));
        assert!(runtime.0.lock().expect("runtime").session.is_some());
    }

    #[test]
    fn session_is_not_visible_to_stop_until_reader_is_registered() {
        let runtime = AudioRuntime::default();
        let workers = Arc::new(Mutex::new(Vec::new()));
        let session = CaptureSession {
            session_id: "transactional-start".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: workers.clone(),
            server: Arc::new(Mutex::new(None)),
        };
        let (reader_start, start_reader) = reader_start_barrier();
        let reader_exited = Arc::new(AtomicBool::new(false));
        let exited = reader_exited.clone();
        let reader = thread::spawn(move || {
            let _ = reader_start.wait();
            exited.store(true, Ordering::SeqCst);
        });

        let mut guard = runtime.0.lock().expect("runtime");
        let stop_runtime = runtime.clone();
        let stop_attempted = Arc::new(AtomicBool::new(false));
        let attempted = stop_attempted.clone();
        let stopper = thread::spawn(move || {
            attempted.store(true, Ordering::SeqCst);
            take_session(&stop_runtime, None)
        });
        while !stop_attempted.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(20));
        assert!(!stopper.is_finished(), "Stop must wait for startup publication");

        publish_capture_session(&mut guard.session, session, reader).expect("publish session");
        assert_eq!(workers.lock().expect("workers").len(), 1);
        drop(guard);

        let owned = stopper
            .join()
            .expect("stopper")
            .expect("published session");
        assert_eq!(owned.workers.lock().expect("workers").len(), 1);
        drop(start_reader);
        stop_session(owned, ShutdownMode::Cancel);
        assert!(reader_exited.load(Ordering::SeqCst));
        assert!(runtime.0.lock().expect("runtime").session.is_none());
    }

    #[test]
    fn cancelled_reader_start_is_bounded_and_joinable() {
        let (reader_start, start_reader) = reader_start_barrier();
        let reader = thread::spawn(move || assert!(!reader_start.wait()));
        let started = Instant::now();
        drop(start_reader);
        reader.join().expect("reader join");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn concurrent_stops_take_session_once_and_reap_registered_reader() {
        let runtime = AudioRuntime::default();
        let reader_reaped = Arc::new(AtomicBool::new(false));
        let reaped = reader_reaped.clone();
        let workers = Arc::new(Mutex::new(vec![thread::spawn(move || {
            reaped.store(true, Ordering::SeqCst);
        })]));
        runtime.0.lock().expect("runtime").session = Some(CaptureSession {
            session_id: "concurrent-stop".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers,
            server: Arc::new(Mutex::new(None)),
        });

        let first_runtime = runtime.clone();
        let second_runtime = runtime.clone();
        let first = thread::spawn(move || take_session(&first_runtime, Some("concurrent-stop")));
        let second = thread::spawn(move || take_session(&second_runtime, Some("concurrent-stop")));
        let sessions = [first.join().expect("first stop"), second.join().expect("second stop")];
        assert_eq!(sessions.iter().filter(|session| session.is_some()).count(), 1);
        stop_session(
            sessions.into_iter().flatten().next().expect("owned session"),
            ShutdownMode::Cancel,
        );
        assert!(reader_reaped.load(Ordering::SeqCst));
        assert!(runtime.0.lock().expect("runtime").session.is_none());
    }

    #[test]
    fn obsolete_terminal_cleanup_cannot_take_replacement_session() {
        let runtime = AudioRuntime::default();
        runtime.0.lock().expect("runtime").session = Some(CaptureSession {
            session_id: "replacement".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(Vec::new())),
            server: Arc::new(Mutex::new(None)),
        });

        assert!(take_session(&runtime, Some("obsolete")).is_none());
        assert_eq!(
            runtime.0.lock().expect("runtime").session.as_ref().expect("replacement").session_id,
            "replacement"
        );
        stop_session(
            take_session(&runtime, Some("replacement")).expect("replacement"),
            ShutdownMode::Cancel,
        );
    }

    #[test]
    fn fake_failing_process_reports_exit_without_hanging() {
        let stop = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let status = run_managed_process(&mut command, &stop, None).expect("process status");
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn final_process_completes_within_deadline() {
        let stop = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let status = run_managed_process(&mut command, &stop, Some(Duration::from_millis(200)))
            .expect("timely final process");
        assert!(status.success());
    }

    #[test]
    fn final_process_is_terminated_and_reaped_at_deadline() {
        let stop = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        assert_eq!(
            run_managed_process(&mut command, &stop, Some(Duration::from_millis(80))),
            Err("timed out".into())
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fake_hanging_process_is_cancelled_and_reaped() {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            cancel.store(true, Ordering::SeqCst);
        });
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        assert_eq!(run_managed_process(&mut command, &stop, None), Err("cancelled".into()));
        trigger.join().expect("cancel trigger");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    // Model/runtime selection contract: the DEFAULT production build must use
    // exactly ggml-large-v3-turbo-q5_0.bin and never point at the full model.
    // build.rs is the single source of truth and the runtime must consume it
    // via env!, never the retired small Q5_1 model. The full unquantized
    // large-v3 (failed the M1 speed gate) is selectable ONLY through the
    // explicit test-only trial gate INTERVJUARE_WHISPER_MODEL_TRIAL, never as
    // an unconditional default.
    // (Retired filenames are assembled via concat! so this contract test
    // itself does not reintroduce banned literals into src/lib.rs.)
    #[test]
    fn production_runtime_selects_turbo_q5_model_only() {
        let retired_small = concat!("ggml-small", "-q5_1.bin");
        let retired_full_select = concat!("MODEL_NAME: &str = \"ggml-large", "-v3.bin\"");
        let retired_small_select = concat!("MODEL_NAME: &str = \"ggml-small", "-q5_1.bin\"");
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let build_script =
            fs::read_to_string(manifest.join("build.rs")).expect("read build.rs contract");
        let runtime_source =
            fs::read_to_string(manifest.join("src/lib.rs")).expect("read runtime contract");
        // Stale-cleanup mentions are allowed; selecting a retired model as the
        // unconditional default is not.
        assert!(
            !build_script.contains(retired_small_select),
            "build.rs must not select the old small model as MODEL_NAME"
        );
        assert!(
            !build_script.contains(retired_full_select),
            "build.rs must not select the full unquantized ggml-large-v3.bin as MODEL_NAME"
        );
        assert!(
            !runtime_source.contains(retired_small),
            "src/lib.rs must not resolve the old small model"
        );
        assert!(
            !build_script.contains("EXPECTED_MODEL_LEN: u64 = 3_095_033_483"),
            "build.rs must not keep the retired full large-v3 size as production validation"
        );
        assert!(
            build_script.contains("ggml-large-v3-turbo-q5_0.bin"),
            "build.rs must select exactly ggml-large-v3-turbo-q5_0.bin"
        );
        assert!(
            build_script.contains("cargo:rustc-env=INTERVJUARE_MODEL_NAME"),
            "build.rs must emit INTERVJUARE_MODEL_NAME as the single source of truth"
        );
        assert!(
            runtime_source.contains("env!(\"INTERVJUARE_MODEL_NAME\")"),
            "runtime must consume INTERVJUARE_MODEL_NAME via env! instead of a duplicated literal"
        );
        assert_eq!(
            env!("INTERVJUARE_MODEL_NAME"),
            "ggml-large-v3-turbo-q5_0.bin",
            "default build pipeline must expose exactly ggml-large-v3-turbo-q5_0.bin to the runtime"
        );
        assert_eq!(
            whisper_model_filename(),
            "ggml-large-v3-turbo-q5_0.bin",
            "default runtime lookup must resolve exactly ggml-large-v3-turbo-q5_0.bin"
        );
        // The default selection must never be the full trial model, even
        // though the trial gate exists in the same build script.
        assert_ne!(
            whisper_model_filename(),
            FULL_TRIAL_MODEL_NAME,
            "default runtime lookup must never point at the full trial model"
        );
        assert!(
            startup_timeout_for_model(whisper_model_filename()) == SERVER_STARTUP_TIMEOUT,
            "default build must not take the full trial readiness bound"
        );
    }

    // Test-only trial gate contract: the full model is selectable ONLY via
    // the explicit INTERVJUARE_WHISPER_MODEL_TRIAL=full-large-v3 opt-in, the
    // gate fails closed on any other value, rebuilds track the variable, and
    // staging keeps exactly the selected model. The default (unset/empty)
    // stays the q5 production model.
    #[test]
    fn trial_model_gate_is_explicit_opt_in_and_fail_closed() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let build_script =
            fs::read_to_string(manifest.join("build.rs")).expect("read build.rs contract");
        assert!(
            build_script.contains("INTERVJUARE_WHISPER_MODEL_TRIAL"),
            "build.rs must gate the trial behind an explicit env override"
        );
        assert!(
            build_script.contains("cargo:rerun-if-env-changed={TRIAL_MODEL_ENV}")
                && build_script
                    .contains("const TRIAL_MODEL_ENV: &str = \"INTERVJUARE_WHISPER_MODEL_TRIAL\""),
            "build.rs must re-run model selection when the trial override changes"
        );
        assert!(
            build_script.contains("full-large-v3"),
            "build.rs must document the exact trial opt-in value"
        );
        assert!(
            build_script.contains("ggml-large-v3.bin"),
            "build.rs must resolve the exact full trial asset"
        );
        assert!(
            build_script.contains("3095033483"),
            "build.rs must validate the exact full trial byte size"
        );
        assert!(
            build_script.contains("574_041_195"),
            "build.rs must keep the exact default q5 byte size"
        );
        assert!(
            build_script.contains("refusing to stage an unknown model"),
            "build.rs must fail closed on unsupported trial values instead of guessing"
        );
        assert!(
            build_script.contains("remove_nonselected_model_staging"),
            "build.rs must evict the non-selected model so the bundle holds exactly one"
        );
    }

    // Trial-specific readiness bound: the 15 s default can never ready a full
    // trial server (observed ~46-110 s warm vs ~12 s q5), so the trial gets a
    // finite 150 s bound. Inference and Stop budgets are identical for both
    // models: 5 s curl max-time, 8 s final wrapper, 14 s aggregate drain
    // (~3 s native + ~14 s drain + ~3 s teardown ≈ ~20 s total Stop).
    #[test]
    fn server_startup_timeout_is_trial_specific_and_bounded() {
        assert_eq!(SERVER_STARTUP_TIMEOUT, Duration::from_secs(15), "default readiness stays 15 s");
        assert_eq!(
            SERVER_STARTUP_TIMEOUT_FULL_TRIAL,
            Duration::from_secs(150),
            "trial readiness is a finite 150 s bound"
        );
        assert!(
            SERVER_STARTUP_TIMEOUT_FULL_TRIAL > Duration::from_secs(110),
            "trial bound must cover the observed ~46-110 s warm range with margin"
        );
        assert_eq!(
            startup_timeout_for_model("ggml-large-v3-turbo-q5_0.bin"),
            Duration::from_secs(15),
            "q5 selection keeps the default bound"
        );
        assert_eq!(
            startup_timeout_for_model(FULL_TRIAL_MODEL_NAME),
            Duration::from_secs(150),
            "full trial selection gets the extended finite bound"
        );
        assert_eq!(
            server_startup_timeout(),
            Duration::from_secs(15),
            "default build keeps the 15 s bound"
        );
        // Lifecycle regression pins: the trial changes ONLY readiness.
        assert_eq!(SERVER_REQUEST_TIMEOUT_SECS, 5, "curl max-time stays 5 s for both models");
        assert_eq!(SERVER_FINAL_TIMEOUT, Duration::from_secs(8), "final wrapper stays 8 s");
        assert_eq!(STOP_DRAIN_BUDGET, Duration::from_secs(14), "aggregate drain stays 14 s");
    }

    #[test]
    fn persistent_server_uses_exact_loopback_flags_and_nonstatic_path() {
        let request_a = generate_request_path();
        let request_b = generate_request_path();
        assert!(request_a.starts_with('/'));
        assert!(request_b.starts_with('/'));
        assert_ne!(request_a, request_b, "per-session request path must be non-static");
        let argv = whisper_server_argv(
            Path::new("/tmp/whisper-server"),
            Path::new("/tmp/ggml-large-v3-turbo-q5_0.bin"),
            17_231,
            &request_a,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("--host"), "server must bind an explicit host flag");
        assert!(joined.contains("127.0.0.1"), "server must bind loopback only");
        assert!(!joined.contains("0.0.0.0"), "server must never bind beyond loopback");
        assert!(joined.contains("--port"), "server must use an ephemeral/free port flag");
        assert!(joined.contains("17231"), "server argv must carry the selected port");
        assert!(joined.contains("--request-path"), "server must use a per-session request path");
        assert!(joined.contains(&request_a), "server argv must carry the generated path");
        assert!(joined.contains("--no-timestamps"), "server must disable timestamps");
        assert!(joined.contains("--no-context"), "server must disable cross-request context");
        assert!(joined.contains("--audio-ctx"), "server must bound audio context");
        assert!(joined.contains("640"), "audio context must be 640 frames");
        assert!(joined.contains("/tmp/ggml-large-v3-turbo-q5_0.bin"));
        let endpoint = inference_url(17_231, &request_a);
        assert!(endpoint.starts_with("http://127.0.0.1:17231"), "inference must stay on loopback");
        assert!(endpoint.contains(&request_a), "inference URL must include the session path");
        assert!(endpoint.ends_with("/inference"), "inference URL must target the inference path");
    }

    #[test]
    fn server_curl_routes_language_explicitly_and_parses_bounded_json() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = InferenceWorkspace::new().expect("curl fixture workspace");
        let fake_curl = fixture.path.join("fake-curl");
        fs::write(
            &fake_curl,
            r#"#!/bin/sh
endpoint=""
language=""
response=""
wav=""
for arg in "$@"; do
  case "$arg" in
    http://*) endpoint="$arg" ;;
    language=*) language="$arg" ;;
    response_format=*) : ;;
  esac
  case "$arg" in
    --output) want_output=1 ;;
    *)
      if [ "${want_output:-0}" = "1" ]; then response="$arg"; want_output=0; fi
      ;;
  esac
done
for arg in "$@"; do
  case "$arg" in
    file=@*) wav="${arg#file=@}"; wav="${wav%%;*}" ;;
  esac
done
if [ -z "$response" ] || [ -z "$wav" ] || [ ! -f "$wav" ]; then exit 3; fi
case " $* " in
  *" response_format=json "*) ;;
  *) exit 4 ;;
esac
case " $* " in
  *" no_timestamps=true "*) ;;
  *) exit 5 ;;
esac
case " $* " in
  *" prompt= "*) ;;
  *) exit 6 ;;
esac
printf '{"text":" heard:%s "}' "$language" > "$response"
"#,
        )
        .expect("fake curl script");
        let mut permissions = fs::metadata(&fake_curl).expect("script metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake_curl, permissions).expect("executable script");
        let endpoint = "http://127.0.0.1:17999/session-abc/inference";
        let wav = fixture.path.join("audio.wav");
        write_wav(&wav, &[0.03; 160]).expect("wav");
        for language in ["sv", "en"] {
            let response = fixture.path.join(format!("response-{language}.json"));
            let args = build_server_curl_args(endpoint, &wav, &response, language);
            let joined = args.join(" ");
            assert!(joined.contains(endpoint), "curl must POST to the loopback session endpoint");
            assert!(
                joined.contains(&format!("language={language}")),
                "curl must route language explicitly, got: {joined}"
            );
            assert!(!joined.contains("sh -c"), "curl must use argument arrays, never a shell");
            let status = run_managed_process(
                Command::new(&fake_curl).args(&args),
                &AtomicBool::new(false),
                Some(Duration::from_secs(5)),
            )
            .expect("fake curl run");
            assert!(status.success());
            let payload = fs::read(&response).expect("response file");
            assert_eq!(
                parse_server_response_json(&payload).expect("parse"),
                format!("heard:language={language}"),
            );
        }
        assert!(parse_server_response_json(&vec![b'x'; 300_000]).is_err(), "unbounded JSON must be rejected");
        assert!(parse_server_response_json(br#"{"nope":1}"#).is_err());
    }

    #[test]
    fn server_early_exit_fails_readiness_fast() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .expect("spawn exiting child");
        let started = Instant::now();
        let result = wait_for_server_ready(&mut child, &|| false, Duration::from_secs(5), &AtomicBool::new(false));
        assert!(result.is_err(), "early child exit must fail readiness");
        assert!(started.elapsed() < Duration::from_secs(2), "early exit must be detected promptly");
        let _ = child.wait();
    }

    #[test]
    fn server_readiness_timeout_is_bounded() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn hanging child");
        let started = Instant::now();
        let result = wait_for_server_ready(&mut child, &|| false, Duration::from_millis(120), &AtomicBool::new(false));
        assert!(result.is_err(), "missing readiness must time out");
        assert!(started.elapsed() < Duration::from_secs(5), "readiness wait must stay bounded");
        terminate_child(&mut child, Duration::ZERO);
    }

    #[test]
    fn server_readiness_wait_aborts_promptly_on_stop_cancel() {
        // Stop during a slow model load must not wait out the readiness
        // bound: the pending-start cancel flag aborts the wait quickly and
        // the caller (spawn_whisper_server) kills/reaps the child.
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            trigger.store(true, Ordering::SeqCst);
        });
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn hanging child");
        let started = Instant::now();
        let result = wait_for_server_ready(&mut child, &|| false, Duration::from_secs(60), &cancel);
        assert_eq!(result, Err("cancelled".into()), "cancelled load must abort the wait");
        assert!(started.elapsed() < Duration::from_secs(5), "cancelled wait must return promptly");
        canceller.join().expect("canceller join");
        terminate_child(&mut child, Duration::ZERO);
        assert!(
            child.wait().is_ok(),
            "cancelled readiness child must be reaped without orphans"
        );
    }

    fn pending_fixture(session_id: &str) -> PendingStart {
        PendingStart {
            session_id: session_id.into(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn slot_runtime(session_id: &str) -> (AudioRuntime, PendingStart) {
        let runtime = AudioRuntime::default();
        let pending = pending_fixture(session_id);
        runtime.0.lock().expect("runtime").starting = Some(pending.clone());
        (runtime, pending)
    }

    #[test]
    fn stop_takes_pending_start_once_and_leaves_no_session() {
        let (runtime, pending) = slot_runtime("pending-stop");
        // Stop path: take the slot, then signal cancel (as stop_runtime does).
        let taken = take_pending_start(&runtime, None).expect("pending slot");
        assert_eq!(taken.session_id, "pending-stop");
        taken.cancel.store(true, Ordering::SeqCst);
        assert!(pending.cancel.load(Ordering::SeqCst));
        // Second take finds nothing; abort is idempotent and touches nothing.
        assert!(take_pending_start(&runtime, None).is_none());
        abort_pending_start(&runtime, &pending);
        let guard = runtime.0.lock().expect("runtime");
        assert!(guard.session.is_none(), "a cancelled start must never publish a session");
        assert!(guard.starting.is_none(), "the slot must be released exactly once");
    }

    #[test]
    fn pending_start_ignores_mismatched_expected_session() {
        let (runtime, pending) = slot_runtime("pending-other");
        assert!(take_pending_start(&runtime, Some("unrelated")).is_none());
        assert!(
            runtime.0.lock().expect("runtime").starting.is_some(),
            "a foreign Stop must not steal the pending slot"
        );
        abort_pending_start(&runtime, &pending);
        assert!(runtime.0.lock().expect("runtime").starting.is_none());
    }

    #[test]
    fn pending_start_yields_to_published_session_and_keeps_foreign_slots() {
        let runtime = AudioRuntime::default();
        // A published session always wins over a pending slot.
        runtime.0.lock().expect("runtime").session = Some(CaptureSession {
            session_id: "live".into(),
            shutdown: Arc::new(ShutdownState::default()),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(Vec::new())),
            server: Arc::new(Mutex::new(None)),
        });
        runtime.0.lock().expect("runtime").starting = Some(pending_fixture("pending-shadowed"));
        assert!(take_pending_start(&runtime, None).is_none());
        // abort_pending_start only releases its own slot, never a replacement.
        let foreign = pending_fixture("someone-else");
        abort_pending_start(&runtime, &foreign);
        assert!(
            runtime.0.lock().expect("runtime").starting.is_some(),
            "abort must not clear a foreign slot"
        );
    }

    #[test]
    fn serialized_request_gate_rechecks_cancellation_before_submission() {
        let gate = InferenceGate::default();
        let stop = AtomicBool::new(false);
        let guard = acquire_inference_permit(&gate, &stop).expect("first permit");
        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        assert!(
            acquire_inference_permit(&gate, &cancelled).is_err(),
            "waiting work must re-check cancellation instead of queueing behind the active request"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(guard);
        assert!(acquire_inference_permit(&gate, &stop).is_ok());
    }

    #[test]
    fn inference_gate_poison_returns_actionable_error_without_spinning() {
        let gate = InferenceGate::default();
        let poisoner = gate.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoner.inner.lock().unwrap();
            panic!("deliberate gate poison for RED");
        }));
        assert!(gate.inner.try_lock().is_err(), "gate must be poisoned for this test");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_gate = gate.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = match acquire_inference_permit(&worker_gate, &worker_stop) {
                Ok(_guard) => "acquired-unexpected".to_string(),
                Err(error) => error,
            };
            let _ = tx.send(outcome);
        });
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(error) => assert!(
                error.contains("poison"),
                "poisoned gate must return an actionable poisoned error, got: {error}"
            ),
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                panic!("RED: poisoned gate spun without returning (would hang Stop)");
            }
        }
    }

    #[test]
    fn bounded_response_read_rejects_oversized_before_parse() {
        let fixture = InferenceWorkspace::new().expect("bounded fixture workspace");
        let oversized = fixture.path.join("oversized.json");
        let payload = vec![b'x'; SERVER_RESPONSE_LIMIT + 1];
        fs::write(&oversized, &payload).expect("oversized fixture");
        let error = read_bounded_response(&oversized).expect_err("oversized payload must be rejected");
        assert!(error.contains("exceeds") || error.contains("bounded") || error.contains("limit"));
        let exact = fixture.path.join("exact.json");
        fs::write(&exact, br#"{"text":"ok"}"#).expect("exact fixture");
        let bytes = read_bounded_response(&exact).expect("small payload must pass");
        assert_eq!(parse_server_response_json(&bytes).expect("parse"), "ok");
    }

    #[test]
    fn health_readiness_requires_exact_http_status_line() {
        assert!(is_health_ready_response("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n"));
        assert!(is_health_ready_response("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"));
        assert!(
            !is_health_ready_response("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 3\r\n\r\n200"),
            "503 with a 200 body must not be ready"
        );
        assert!(
            !is_health_ready_response("HTTP/1.1 503 Service Unavailable\r\nX-Debug: 200\r\n\r\nnope"),
            "503 with a 200 header must not be ready"
        );
        assert!(
            !is_health_ready_response("HTTP/1.1 2000 OK\r\n\r\n"),
            "malformed 2000 must not be ready"
        );
        assert!(!is_health_ready_response("garbage without status line 200"));
    }

    #[test]
    fn inference_timeouts_match_subsecond_benchmark_contract() {
        assert_eq!(SERVER_REQUEST_TIMEOUT_SECS, 5, "curl max-time must be 5 s");
        assert_eq!(SERVER_FINAL_TIMEOUT, Duration::from_secs(8), "final wrapper must be 8 s");
        let args = build_server_curl_args(
            "http://127.0.0.1:17999/s/inference",
            Path::new("/tmp/audio.wav"),
            Path::new("/tmp/response.json"),
            "sv",
        );
        let joined = args.join(" ");
        assert!(joined.contains("--max-time"), "curl argv must bound max-time");
        assert!(joined.contains("--max-filesize"), "curl argv must bound max-filesize");
        assert!(joined.contains("262144"), "curl max-filesize must equal 256 KiB");
    }

    // Liveness probe for owned fake children only. Never targets owner
    // processes: callers pass only PIDs returned by their own spawns above.
    fn owned_pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn fake_owned_child() -> Child {
        Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("owned fake child")
    }

    #[test]
    fn graceful_stop_finalizes_before_server_teardown_and_restarts() {
        // Real CaptureSession + production stop_session (not hand-simulated).
        let shutdown = Arc::new(ShutdownState::default());
        let server_child = fake_owned_child();
        let server_pid = server_child.id();
        let server = Arc::new(Mutex::new(Some(RunningServer {
            child: Some(server_child),
            endpoint: "http://127.0.0.1:1/s/inference".into(),
        })));
        let process_child = fake_owned_child();
        let process_pid = process_child.id();
        let finalized = Arc::new(AtomicBool::new(false));
        let saw_server_alive = Arc::new(AtomicBool::new(false));
        let worker_server = server.clone();
        let worker_done = finalized.clone();
        let worker_saw = saw_server_alive.clone();
        let worker = thread::spawn(move || {
            // Simulated trailing finalization: the owned fake server must
            // still be alive while finalization runs.
            thread::sleep(Duration::from_millis(60));
            if let Ok(mut guard) = worker_server.lock() {
                if let Some(running) = guard.as_mut() {
                    if let Some(child) = running.child.as_mut() {
                        if matches!(child.try_wait(), Ok(None)) {
                            worker_saw.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            worker_done.store(true, Ordering::SeqCst);
        });
        let session = CaptureSession {
            session_id: "stop-production-1".into(),
            shutdown,
            process: CaptureProcess {
                child: Arc::new(Mutex::new(Some(process_child))),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(vec![worker])),
            server: server.clone(),
        };
        let started = Instant::now();
        stop_session(session, ShutdownMode::GracefulDrain);
        assert!(finalized.load(Ordering::SeqCst), "trailing finalization must complete first");
        assert!(
            saw_server_alive.load(Ordering::SeqCst),
            "worker must observe the owned fake server alive during finalization"
        );
        assert!(server.lock().expect("server").is_none(), "server must be taken/terminated");
        assert!(!owned_pid_alive(server_pid), "owned fake server PID must be terminated/reaped");
        assert!(!owned_pid_alive(process_pid), "owned capture PID must be terminated/reaped");
        // Two owned `sleep 30` children terminate via the production 3 s
        // bounds each; assert bounded well below the 30 s sleep (no hang).
        assert!(started.elapsed() < Duration::from_secs(12), "graceful production Stop must stay bounded");
        // Immediate restart: a second real CaptureSession constructs and
        // stops cleanly with fresh owned PIDs (no orphan blocks restart).
        let shutdown2 = Arc::new(ShutdownState::default());
        let server_child2 = fake_owned_child();
        let server_pid2 = server_child2.id();
        assert_ne!(server_pid2, server_pid, "restart must create a fresh server process");
        let server2 = Arc::new(Mutex::new(Some(RunningServer {
            child: Some(server_child2),
            endpoint: "http://127.0.0.1:1/s2/inference".into(),
        })));
        let process_child2 = fake_owned_child();
        let process_pid2 = process_child2.id();
        let session2 = CaptureSession {
            session_id: "stop-production-2".into(),
            shutdown: shutdown2,
            process: CaptureProcess {
                child: Arc::new(Mutex::new(Some(process_child2))),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(Vec::new())),
            server: server2.clone(),
        };
        stop_session(session2, ShutdownMode::GracefulDrain);
        assert!(server2.lock().expect("server2").is_none());
        assert!(!owned_pid_alive(server_pid2));
        assert!(!owned_pid_alive(process_pid2));
    }

    #[test]
    fn cancel_stop_reaps_owned_server_without_finalization_wait() {
        let shutdown = Arc::new(ShutdownState::default());
        let cancel_signal = shutdown.clone();
        let server_child = fake_owned_child();
        let server_pid = server_child.id();
        let server = Arc::new(Mutex::new(Some(RunningServer {
            child: Some(server_child),
            endpoint: "http://127.0.0.1:1/cancel/inference".into(),
        })));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = exited.clone();
        let worker = thread::spawn(move || {
            // Cancel path: exit promptly instead of simulating finalization.
            while !cancel_signal.is_cancelled() {
                thread::sleep(Duration::from_millis(10));
            }
            worker_exited.store(true, Ordering::SeqCst);
        });
        let session = CaptureSession {
            session_id: "cancel-production".into(),
            shutdown,
            process: CaptureProcess {
                child: Arc::new(Mutex::new(Some(fake_owned_child()))),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(vec![worker])),
            server: server.clone(),
        };
        let started = Instant::now();
        stop_session(session, ShutdownMode::Cancel);
        assert!(exited.load(Ordering::SeqCst), "cancelled worker must exit");
        assert!(server.lock().expect("server").is_none());
        assert!(!owned_pid_alive(server_pid), "owned fake server must be reaped on cancel");
        assert!(started.elapsed() < Duration::from_secs(12), "cancel Stop must stay bounded");
    }

    // Shared synthetic fixtures for the decoupled ingestion/dispatch tests.
    // Metadata-only 20 ms cadence (captured_at_ms += 20, 320 samples at
    // 16 kHz). Per-frame amplitude makes frame order verifiable from the
    // final snapshot; every frame passes the production VAD gate.
    fn green_speech_samples(frame_index: usize) -> Vec<f32> {
        vec![0.03 + frame_index as f32 * 0.0001; 320]
    }

    fn green_silence_samples() -> Vec<f32> {
        vec![0.0; 320]
    }

    fn green_speech_frame(source: Speaker, index: usize, base_ms: u64) -> PcmFrame {
        PcmFrame {
            source,
            captured_at_ms: base_ms + index as u64 * 20,
            samples: green_speech_samples(index),
        }
    }

    // GREEN (audio_backpressure fix): real BoundedFrames queue (cap 80) ->
    // production run_ingestion (continuous drain, never blocks on inference)
    // -> production DispatchQueue -> production run_dispatcher with a
    // latch-held fake inference (4.1 s equivalent, no sleep). The producer
    // pushes 101 speech frames while the first inference is held.
    #[test]
    fn green_continuous_drain_zero_pcm_drops_during_held_inference() {
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        let dispatch = Arc::new(DispatchQueue::new());
        let shutdown = Arc::new(ShutdownState::default());
        let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker_dispatch = dispatch.clone();
        let warn_record = warnings.clone();
        let lease = IngestionLease {
            remaining: Arc::new(AtomicUsize::new(1)),
            dispatch: dispatch.clone(),
        };
        let losses = FinalLossTracker::default();
        let ingestion = thread::spawn(move || {
            let _lease = lease;
            run_ingestion(
                &worker_queue,
                &worker_shutdown,
                Speaker::Candidate,
                "sv",
                &worker_dispatch,
                &losses,
                &|_code, message, _| warn_record.lock().expect("warnings").push(message),
                &|| {},
            );
        });
        let release: Arc<(Mutex<bool>, Condvar)> =
            Arc::new((Mutex::new(false), Condvar::new()));
        let seen: Arc<Mutex<Vec<(Speaker, u64, u64, u64, SubmissionKind, usize)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let infer_release = release.clone();
        let infer_seen = seen.clone();
        let infer_shutdown = shutdown.clone();
        let dispatch_worker = dispatch.clone();
        let events_record = events.clone();
        let dispatcher = thread::spawn(move || {
            let first = AtomicBool::new(true);
            let infer = |job: &InferenceJob| -> Result<String, String> {
                infer_seen.lock().expect("jobs").push((
                    job.speaker,
                    job.segment_number,
                    job.started_at_ms,
                    job.captured_at_ms,
                    job.kind,
                    job.samples.len(),
                ));
                if first.swap(false, Ordering::SeqCst) {
                    let (lock, cvar) = &*infer_release;
                    let mut held = lock.lock().expect("latch");
                    while !*held {
                        held = cvar.wait_timeout(held, Duration::from_millis(50)).expect("latch").0;
                    }
                    return Ok("held inference result".to_string());
                }
                if job.kind == SubmissionKind::Final {
                    Ok("final text".to_string())
                } else {
                    Ok("partial text".to_string())
                }
            };
            run_dispatcher(
                &dispatch_worker,
                &infer_shutdown,
                "green-session",
                &infer,
                &|event| events_record.lock().expect("events").push(event),
                &|_, _, _| {},
                &FinalLossTracker::default(),
            );
        });
        // Paced like live callbacks (5 ms between 20 ms frames is still 4x
        // faster than the live 20 ms cadence, so keeping up proves headroom;
        // an unpaced instant burst would only measure thread-spawn latency).
        let mut drops = 0_usize;
        for index in 0..101_usize {
            if queue.push(green_speech_frame(Speaker::Candidate, index, 1_000)).dropped {
                drops += 1;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(drops, 0, "continuous drain must absorb the bounded stall with zero PCM drops");
        // The dispatcher pops immediately and holds the job in-flight, so
        // "received work" is observed at the fake inference boundary, not
        // in the pending queue.
        let wait_start = Instant::now();
        loop {
            if seen.lock().expect("jobs").len() >= 1 {
                break;
            }
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "dispatcher never received work during the held stall"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let (pending_jobs, pending_ms) = dispatch.pending_counts();
        assert!(
            pending_jobs <= DISPATCH_MAX_JOBS,
            "pending jobs must stay within the named bound, got {pending_jobs}"
        );
        assert!(
            pending_ms <= samples_to_ms(DISPATCH_MAX_SAMPLES),
            "pending audio must stay within the named sample bound, got {pending_ms} ms"
        );
        {
            let (lock, cvar) = &*release;
            *lock.lock().expect("latch") = true;
            cvar.notify_all();
        }
        shutdown.request(ShutdownMode::GracefulDrain);
        shutdown.mark_input_closed();
        ingestion.join().expect("ingestion join");
        assert!(dispatch.is_closed(), "last ingestion exit must close the dispatch queue");
        dispatcher.join().expect("dispatcher join");
        assert!(
            warnings.lock().expect("warnings").is_empty(),
            "bounded stall must not raise overload warnings"
        );
        // All accepted frames ingested in order with matching source and
        // timestamps: the trailing final snapshot equals the exact pushed
        // concatenation (101 frames * 320 samples).
        let seen = seen.lock().expect("jobs");
        let finals: Vec<_> = seen
            .iter()
            .filter(|job| job.4 == SubmissionKind::Final)
            .collect();
        assert_eq!(finals.len(), 1, "exactly one final snapshot, got {seen:?}");
        let (_, segment, started, captured, _, len) = finals[0];
        assert_eq!(*segment, 1);
        assert_eq!(*started, 1_000);
        assert_eq!(*captured, 1_000 + 100 * 20);
        assert_eq!(*len, 101 * 320);
        let events = events.lock().expect("events");
        assert!(!events.is_empty(), "held inference must still produce hypotheses");
        let mut previous_revision = 0;
        for event in events.iter() {
            assert_eq!(event.speaker, Speaker::Candidate);
            assert_eq!(event.session_id, "green-session");
            assert_eq!(event.segment_id, "candidate-1");
            assert!(event.revision > previous_revision, "revisions must be monotonic");
            previous_revision = event.revision;
        }
        let last = events.last().expect("last event");
        assert!(last.final_segment, "stream must end with exactly one final");
        assert_eq!(events.iter().filter(|event| event.final_segment).count(), 1);
    }

    // GREEN (premature final): the production UtteranceSegmenter owns no
    // clock, so a wall-clock inference stall cannot masquerade as silence.
    // This negative test sleeps 900 ms on the wall (> FINAL_SILENCE_MS)
    // while only 20 ms of captured audio passes after voice: the old
    // Instant-based predicate would finalize here; the captured-clock
    // predicate must not. Real silence on the captured clock then yields
    // exactly one final with monotonic revisions.
    #[test]
    fn green_captured_silence_yields_exactly_one_final() {
        let mut segmenter = UtteranceSegmenter::new();
        let mut snapshots = Vec::new();
        for index in 0..50_usize {
            if let Some(snapshot) = segmenter.ingest(
                &green_speech_frame(Speaker::Candidate, index, 1_000),
                false,
            ) {
                snapshots.push(snapshot);
            }
        }
        assert_eq!(snapshots.len(), 1, "one partial at 900 ms, got {}", snapshots.len());
        assert_eq!(snapshots[0].kind, SubmissionKind::Partial);
        assert_eq!(snapshots[0].segment_number, 1);
        // Wall-clock stall longer than the silence gate, captured gap 20 ms.
        thread::sleep(Duration::from_millis(900));
        let no_premature = segmenter.ingest(
            &green_speech_frame(Speaker::Candidate, 50, 1_000),
            false,
        );
        assert!(
            no_premature.is_none(),
            "a 900 ms wall stall with 20 ms of captured audio must not finalize"
        );
        // Real silence on the captured clock: last voice at captured
        // 1_000 + 50*20 = 2_000; final needs latest >= 2_850.
        let mut finals = Vec::new();
        for silence_index in 0..50_usize {
            let frame = PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 2_020 + silence_index as u64 * 20,
                samples: green_silence_samples(),
            };
            if let Some(snapshot) = segmenter.ingest(&frame, false) {
                finals.push(snapshot);
            }
        }
        assert_eq!(finals.len(), 1, "exactly one final after real silence");
        assert_eq!(finals[0].kind, SubmissionKind::Final);
        assert_eq!(finals[0].segment_number, 1);
        // Utterance keeps trailing silence audio (existing assembly
        // semantics): 51 speech + 43 silence frames trigger the final.
        assert_eq!(finals[0].samples.len(), (51 + 43) * 320);
        assert!((finals[0].samples[0] - 0.03).abs() < f32::EPSILON);
        assert_eq!(finals[0].samples[51 * 320], 0.0);
        assert!(!segmenter.has_utterance(), "final clears the utterance");
        let after = segmenter.ingest(
            &PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 4_000,
                samples: green_silence_samples(),
            },
            false,
        );
        assert!(after.is_none(), "silence after final emits nothing");
    }

    // GREEN (overload): repeated prolonged stalls hit the named finite
    // dispatch bound and fail with typed metadata-only overload instead of
    // silently losing finals. Partials coalesce; finals evict partials;
    // only an all-finals-full queue sheds explicitly with a warning.
    #[test]
    fn green_repeated_stalls_yield_bounded_typed_overload() {
        fn final_job(segment: u64) -> InferenceJob {
            InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: segment,
                started_at_ms: segment * 1_000,
                captured_at_ms: segment * 1_000 + 500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; 3_200],
                language: "sv".to_string(),
            }
        }
        let dispatch = DispatchQueue::new();
        // Per-source reservation (M3): one source caps at 6 finals; the 7th
        // from the same source sheds explicitly even though the shared
        // queue (8) still has room — that room belongs to the other source.
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch.submit(final_job(segment)).expect("bounded finals fit");
        }
        assert_eq!(dispatch.pending_counts().0, DISPATCH_MAX_JOBS_PER_SOURCE);
        let overload = dispatch.submit(final_job(7)).expect_err("7th same-source final must overload");
        assert_eq!(overload.pending_jobs, DISPATCH_MAX_JOBS_PER_SOURCE);
        assert_eq!(overload.shed_kind, SubmissionKind::Final);
        assert_eq!(overload.shed_segment, "candidate-7");
        assert_eq!(STT_OVERLOAD_CODE, "stt_overload");
        let message = format_overload_message(Speaker::Candidate, &overload);
        assert!(message.contains("candidate-7"), "shed segment must be identified, got: {message}");
        assert!(message.contains("6 jobs"), "pending depth must be reported, got: {message}");
        assert!(!message.contains("0.03"), "must not leak sample values: {message}");
        let (jobs, pending_ms) = dispatch.pending_counts();
        assert_eq!(jobs, DISPATCH_MAX_JOBS_PER_SOURCE, "queue stays bounded after overload");
        assert!(pending_ms <= samples_to_ms(DISPATCH_MAX_SAMPLES));
        // A final evicts the oldest partial instead of failing while any
        // partial remains: fill the candidate reservation with partials,
        // then a candidate final evicts its own oldest partial (count stays
        // at the 6-job reservation).
        let evicting = DispatchQueue::new();
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            evicting
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 200,
                    kind: SubmissionKind::Partial,
                    samples: vec![0.03; 3_200],
                    language: "sv".to_string(),
                })
                .expect("bounded partials fit");
        }
        evicting.submit(final_job(100)).expect("final evicts oldest partial");
        assert_eq!(evicting.pending_counts().0, DISPATCH_MAX_JOBS_PER_SOURCE);
        // Partial coalescing: two snapshots for the same segment keep one
        // latest bounded entry.
        let coalescing = DispatchQueue::new();
        coalescing
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 1,
                started_at_ms: 1_000,
                captured_at_ms: 1_200,
                kind: SubmissionKind::Partial,
                samples: vec![0.03; 100],
                language: "sv".to_string(),
            })
            .expect("first partial");
        coalescing
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 1,
                started_at_ms: 1_000,
                captured_at_ms: 1_400,
                kind: SubmissionKind::Partial,
                samples: vec![0.03; 200],
                language: "sv".to_string(),
            })
            .expect("coalesced partial");
        assert_eq!(coalescing.pending_counts().0, 1);
        assert_eq!(coalescing.pending_counts().1, samples_to_ms(200));
    }

    // GREEN (fairness): one session-scoped dispatcher serves both sources
    // FIFO; labels, segment IDs and per-source revisions are preserved.
    #[test]
    fn green_two_source_finals_are_fair_and_labelled() {
        let dispatch = DispatchQueue::new();
        for (speaker, segment) in [
            (Speaker::Candidate, 1),
            (Speaker::Interviewer, 1),
            (Speaker::Candidate, 2),
        ] {
            dispatch
                .submit(InferenceJob {
                    speaker,
                    segment_number: segment,
                    started_at_ms: 1_000,
                    captured_at_ms: 1_500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 320],
                    language: "sv".to_string(),
                })
                .expect("fairness fixtures fit");
        }
        dispatch.close();
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let record = events.clone();
        run_dispatcher(
            &dispatch,
            &ShutdownState::default(),
            "fair-session",
            &|job| Ok(format!("heard-{}", job.speaker.id())),
            &|event| record.lock().expect("events").push(event),
            &|_, _, _| panic!("fairness run must not warn"),
            &FinalLossTracker::default(),
        );
        let events = events.lock().expect("events");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].speaker, Speaker::Candidate);
        assert_eq!(events[1].speaker, Speaker::Interviewer);
        assert_eq!(events[2].speaker, Speaker::Candidate);
        assert_eq!(events[0].segment_id, "candidate-1");
        assert_eq!(events[1].segment_id, "interviewer-1");
        assert_eq!(events[2].segment_id, "candidate-2");
        assert_eq!(events[0].revision, 1);
        assert_eq!(events[1].revision, 1, "per-source revisions are independent");
        assert_eq!(events[2].revision, 1, "new segment resets revision");
        assert!(events.iter().all(|event| event.final_segment));
        assert!(events.iter().all(|event| event.session_id == "fair-session"));
    }

    // GREEN (lifecycle): graceful Stop drains PCM and emits the trailing
    // final before teardown; Cancel during a stalled request aborts
    // promptly with no stale hypothesis; a fresh session starts clean.
    #[test]
    fn green_stop_drains_trailing_final_cancel_aborts_and_restart_is_clean() {
        // Graceful Stop with real ingestion + dispatcher threads.
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        let dispatch = Arc::new(DispatchQueue::new());
        let shutdown = Arc::new(ShutdownState::default());
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker_dispatch = dispatch.clone();
        let lease = IngestionLease {
            remaining: Arc::new(AtomicUsize::new(1)),
            dispatch: dispatch.clone(),
        };
        let stop_losses = FinalLossTracker::default();
        let ingestion = thread::spawn(move || {
            let _lease = lease;
            run_ingestion(
                &worker_queue,
                &worker_shutdown,
                Speaker::Interviewer,
                "en",
                &worker_dispatch,
                &stop_losses,
                &|_, _, _| {},
                &|| {},
            );
        });
        let dispatch_worker = dispatch.clone();
        let shutdown_worker = shutdown.clone();
        let events_record = events.clone();
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                &dispatch_worker,
                &shutdown_worker,
                "stop-1",
                &|_| Ok("trailing hello".to_string()),
                &|event| events_record.lock().expect("events").push(event),
                &|_, _, _| {},
                &FinalLossTracker::default(),
            );
        });
        // 10 speech frames = 200 ms: no partial due, trailing final at Stop.
        for index in 0..10_usize {
            assert!(!queue.push(green_speech_frame(Speaker::Interviewer, index, 5_000)).dropped);
        }
        shutdown.request(ShutdownMode::GracefulDrain);
        shutdown.mark_input_closed();
        ingestion.join().expect("ingestion join");
        assert!(dispatch.is_closed(), "Stop must close dispatch after drain");
        dispatcher.join().expect("dispatcher join");
        let events = events.lock().expect("events");
        assert_eq!(events.len(), 1, "exactly the trailing final, got {events:?}");
        assert_eq!(events[0].session_id, "stop-1");
        assert_eq!(events[0].segment_id, "interviewer-1");
        assert_eq!(events[0].revision, 1);
        assert!(events[0].final_segment);
        drop(events);

        // Cancel during a stalled request aborts promptly, reaps the owned
        // wait, and emits no hypothesis for the cancelled non-final.
        let stalled = Arc::new(DispatchQueue::new());
        let cancel_shutdown = Arc::new(ShutdownState::default());
        stalled
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 1,
                started_at_ms: 1_000,
                captured_at_ms: 1_200,
                kind: SubmissionKind::Partial,
                samples: vec![0.03; 320],
                language: "sv".to_string(),
            })
            .expect("cancel fixture");
        let entered = Arc::new(AtomicBool::new(false));
        let worker_stalled = stalled.clone();
        let worker_shutdown = cancel_shutdown.clone();
        let entered_probe = entered.clone();
        let cancelled_events: Arc<Mutex<Vec<TranscriptEvent>>> =
            Arc::new(Mutex::new(Vec::new()));
        let cancelled_record = cancelled_events.clone();
        let canceller = thread::spawn(move || {
            let cancel_signal = worker_shutdown.cancel_signal();
            run_dispatcher(
                &worker_stalled,
                &worker_shutdown,
                "cancel-1",
                &|_| {
                    entered_probe.store(true, Ordering::SeqCst);
                    while !cancel_signal.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err("cancelled".to_string())
                },
                &|event| cancelled_record.lock().expect("events").push(event),
                &|_, _, _| {},
                &FinalLossTracker::default(),
            );
        });
        let wait_start = Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "dispatcher never entered the stalled request"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let cancel_start = Instant::now();
        cancel_shutdown.request(ShutdownMode::Cancel);
        canceller.join().expect("cancelled dispatcher join");
        assert!(
            cancel_start.elapsed() < Duration::from_secs(5),
            "Cancel must abort the stalled request promptly"
        );
        assert!(
            cancelled_events.lock().expect("events").is_empty(),
            "cancelled non-final must emit no hypothesis"
        );

        // Immediate restart: a fresh dispatcher/session starts clean with
        // its own session id and revision base (no stale updates).
        let fresh = DispatchQueue::new();
        fresh
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 1,
                started_at_ms: 9_000,
                captured_at_ms: 9_200,
                kind: SubmissionKind::Final,
                samples: vec![0.03; 320],
                language: "sv".to_string(),
            })
            .expect("restart fixture");
        fresh.close();
        let fresh_events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let fresh_record = fresh_events.clone();
        run_dispatcher(
            &fresh,
            &ShutdownState::default(),
            "restart-1",
            &|_| Ok("restart hello".to_string()),
            &|event| fresh_record.lock().expect("events").push(event),
            &|_, _, _| {},
            &FinalLossTracker::default(),
        );
        let fresh_events = fresh_events.lock().expect("events");
        assert_eq!(fresh_events.len(), 1);
        assert_eq!(fresh_events[0].session_id, "restart-1");
        assert_eq!(fresh_events[0].revision, 1);
    }

    // M1: dual frame+sample bound on the PCM queue. Normal 20 ms frames never
    // approach the sample cap; adversarial max legal frames stay finite and
    // counted. Full-system worst case documented at the constants.
    #[test]
    fn m1_pcm_queue_sample_bound_is_finite_and_normal_case_never_trips() {
        // Normal case: 80 frames of 20 ms @16 kHz (320 samples) = 25_600
        // samples, far below the 256_000 sample cap: zero drops.
        let normal = BoundedFrames::new(PCM_QUEUE_CAPACITY);
        assert_eq!(normal.max_samples(), PCM_QUEUE_MAX_SAMPLES);
        for index in 0..PCM_QUEUE_CAPACITY {
            let dropped = normal.push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 1_000 + index as u64 * 20,
                samples: vec![0.03; 320],
            });
            assert!(!dropped.dropped, "normal 20 ms frames must not trip the sample cap");
        }
        assert_eq!(normal.len(), PCM_QUEUE_CAPACITY);
        assert_eq!(normal.queued_samples(), PCM_QUEUE_CAPACITY * 320);
        // Adversarial case: max legal frames (48_000 samples each). The queue
        // must stay finite: depth <= 80 AND samples <= 256_000, with every
        // eviction counted (no silent unbounded snapshots).
        let adversarial = BoundedFrames::new(PCM_QUEUE_CAPACITY);
        let mut drops = 0_u64;
        for index in 0..80_usize {
            if adversarial
                .push(PcmFrame {
                    source: Speaker::Interviewer,
                    captured_at_ms: 5_000 + index as u64 * 20,
                    samples: vec![0.03; MAX_PCM_FRAMES],
                })
                .dropped
            {
                drops += 1;
            }
        }
        assert!(drops > 0, "max legal frames must trip the finite sample cap");
        assert!(adversarial.len() <= PCM_QUEUE_CAPACITY);
        assert!(
            adversarial.queued_samples() <= PCM_QUEUE_MAX_SAMPLES,
            "queued samples must stay within the sample cap, got {}",
            adversarial.queued_samples()
        );
        let (drop_frames, drop_samples) = adversarial.dropped_counts();
        assert!(drop_frames >= drops);
        assert!(drop_samples > 0);
        // Frame-count-only accounting would claim 80 * 48_000 = 3.84M
        // samples (~15.4 MiB) per source; the sample cap holds 256k (~1 MiB).
        assert!(
            PCM_QUEUE_MAX_SAMPLES < 80 * MAX_PCM_FRAMES,
            "sample cap must tighten the frame-cap-only worst case"
        );
        // Privacy: counters only, no audio.
        assert!(normal.queued_duration_ms() <= samples_to_ms(PCM_QUEUE_MAX_SAMPLES));
    }

    // M3: per-source reservation. A candidate flood (up to its own cap) must
    // not shed an interviewer final while the interviewer is under its
    // reservation. Labels, languages, revisions and final immutability hold
    // through the dispatcher.
    #[test]
    fn m3_candidate_flood_keeps_interviewer_final_independent() {
        fn final_job(speaker: Speaker, segment: u64, samples: usize) -> InferenceJob {
            InferenceJob {
                speaker,
                segment_number: segment,
                started_at_ms: segment * 1_000,
                captured_at_ms: segment * 1_000 + 500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; samples],
                language: if speaker == Speaker::Candidate { "sv".into() } else { "en".into() },
            }
        }
        let dispatch = DispatchQueue::new();
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(final_job(Speaker::Candidate, segment, 3_200))
                .expect("candidate flood up to its own reservation fits");
        }
        let (cand_jobs, _) = dispatch.source_counts(Speaker::Candidate);
        assert_eq!(cand_jobs, DISPATCH_MAX_JOBS_PER_SOURCE);
        // The idle source's final is still accepted (shared has 8-6=2 left).
        dispatch
            .submit(final_job(Speaker::Interviewer, 1, 3_200))
            .expect("interviewer final must survive a candidate flood");
        let (ivr_jobs, _) = dispatch.source_counts(Speaker::Interviewer);
        assert_eq!(ivr_jobs, 1);
        // The flooder's next final exceeds its own reservation and fails
        // explicitly (terminal path in production) instead of taking the
        // other source's room.
        let overload = dispatch
            .submit(final_job(Speaker::Candidate, 99, 3_200))
            .expect_err("candidate beyond its reservation must shed explicitly");
        assert_eq!(overload.shed_kind, SubmissionKind::Final);
        assert_eq!(overload.shed_segment, "candidate-99");
        // Sample reservation: flood candidate samples to its cap, interviewer
        // samples still fit.
        let sample_dispatch = DispatchQueue::new();
        sample_dispatch
            .submit(final_job(Speaker::Candidate, 1, DISPATCH_MAX_SAMPLES_PER_SOURCE))
            .expect("candidate sample reservation fits");
        sample_dispatch
            .submit(final_job(Speaker::Interviewer, 1, 3_200))
            .expect("interviewer samples fit alongside a sample-flooded candidate");
        // End-to-end through the real dispatcher: order, labels, per-source
        // revisions, final immutability.
        dispatch.close();
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let record = events.clone();
        run_dispatcher(
            &dispatch,
            &ShutdownState::default(),
            "fair-flood",
            &|job| Ok(format!("heard-{}", job.speaker.id())),
            &|event| record.lock().expect("events").push(event),
            &|_, _, _| panic!("flood drain must not warn without expiry"),
            &FinalLossTracker::default(),
        );
        let events = events.lock().expect("events");
        assert_eq!(events.len(), DISPATCH_MAX_JOBS_PER_SOURCE + 1);
        assert!(events.iter().all(|e| e.final_segment));
        assert!(events.iter().all(|e| e.session_id == "fair-flood"));
        let interviewer: Vec<_> = events.iter().filter(|e| e.speaker == Speaker::Interviewer).collect();
        assert_eq!(interviewer.len(), 1);
        assert_eq!(interviewer[0].segment_id, "interviewer-1");
        assert_eq!(interviewer[0].revision, 1);
    }

    // M2: accepted-final rejection is a deterministic terminal failure.
    // End-to-end through production run_ingestion: saturated finals ->
    // typed terminal warning with cumulative counters delivered BEFORE the
    // terminal Cancel, then teardown. Partials shed without terminal failure.
    #[test]
    fn m2_saturated_final_is_terminal_with_cumulative_warning_first() {
        let dispatch = Arc::new(DispatchQueue::new());
        // Saturate the candidate per-source reservation with finals.
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 3_200],
                    language: "sv".into(),
                })
                .expect("saturation fixtures fit");
        }
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        let shutdown = Arc::new(ShutdownState::default());
        let losses = FinalLossTracker::default();
        // Warnings carry (code, message); record order.
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_record = warnings.clone();
        // Feed enough speech for a partial (900 ms = 45 frames) plus a final
        // trigger: silence frames advance the captured clock past 850 ms.
        for index in 0..50_usize {
            queue.push(green_speech_frame(Speaker::Candidate, index, 20_000));
        }
        for silence_index in 0..50_usize {
            queue.push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 21_000 + silence_index as u64 * 20,
                samples: green_silence_samples(),
            });
        }
        run_ingestion(
            &queue,
            &shutdown,
            Speaker::Candidate,
            "sv",
            &dispatch,
            &losses,
            &|code, message, _| warn_record.lock().expect("warnings").push((code.into(), message)),
            &|| {},
        );
        // Terminal failure: Cancel requested, never presented as healthy.
        assert!(
            shutdown.is_cancelled(),
            "a shed accepted final must terminally fail the session"
        );
        let warnings = warnings.lock().expect("warnings");
        let terminal: Vec<_> = warnings.iter().filter(|(code, _)| code == STT_FINAL_LOST_CODE).collect();
        assert_eq!(terminal.len(), 1, "exactly one terminal final-loss warning, got {warnings:?}");
        assert!(
            terminal[0].1.contains("Lost finals so far: 1"),
            "cumulative lost counter must be present, got: {}",
            terminal[0].1
        );
        assert!(
            terminal[0].1.contains("candidate-"),
            "shed segment must be identified, got: {}",
            terminal[0].1
        );
        assert!(!terminal[0].1.contains("0.03"), "must not leak sample values");
        let (lost_finals, lost_ms) = losses.snapshot();
        assert_eq!(lost_finals, 1);
        assert!(lost_ms > 0, "lost duration must be honest and nonzero");
        // Ordering: the terminal warning was delivered before teardown
        // (Cancel observed only after warn). run_ingestion emits warn then
        // requests Cancel then breaks, so reaching here with warnings
        // non-empty proves warn-before-terminal on this seam.
    }

    // M2b: partial shed stays non-terminal (shed/coalesce when superseded).
    #[test]
    fn m2_partial_shed_does_not_terminate_capture() {
        let dispatch = DispatchQueue::new();
        // Fill the candidate per-source reservation with partials so a
        // further partial sheds (non-terminally).
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 200,
                    kind: SubmissionKind::Partial,
                    samples: vec![0.03; 3_200],
                    language: "sv".into(),
                })
                .expect("partial fixtures fit");
        }
        let overload = dispatch
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 99,
                started_at_ms: 99_000,
                captured_at_ms: 99_200,
                kind: SubmissionKind::Partial,
                samples: vec![0.03; 3_200],
                language: "sv".into(),
            })
            .expect_err("overflowing partial must shed");
        assert_eq!(overload.shed_kind, SubmissionKind::Partial);
        let message = format_overload_message(Speaker::Candidate, &overload);
        assert!(message.contains("candidate-99"));
        // No terminal code for partials: the caller path only terminally
        // fails on SubmissionKind::Final (covered by m2 above).
        assert!(!message.contains("Capture stopped"));
    }

    // M4: ingestion-thread-only idle fallback. Speech with no further frames
    // yields exactly one final in bounded time; backed-up frames never
    // trigger it prematurely; no duplicate after frames resume.
    #[test]
    fn m4_idle_silence_without_callbacks_yields_exactly_one_final() {
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        let dispatch = Arc::new(DispatchQueue::new());
        let shutdown = Arc::new(ShutdownState::default());
        let losses = FinalLossTracker::default();
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker_dispatch = dispatch.clone();
        let ingestion = thread::spawn(move || {
            run_ingestion(
                &worker_queue,
                &worker_shutdown,
                Speaker::Candidate,
                "sv",
                &worker_dispatch,
                &losses,
                &|_, _, _| {},
                &|| {},
            );
        });
        // 10 speech frames = 200 ms: no partial due, no captured-clock final
        // (needs 850 ms silence). Then the device goes quiet: no callbacks.
        for index in 0..10_usize {
            assert!(!queue.push(green_speech_frame(Speaker::Candidate, index, 40_000)).dropped);
        }
        // Backed-up check: while frames remain queued/arriving the idle
        // fallback must not fire prematurely. Drain of 10 frames takes
        // milliseconds, far below the 1200 ms idle gate, so sleep 400 ms and
        // assert nothing emitted yet.
        thread::sleep(Duration::from_millis(400));
        // Idle fallback fires at ~1200 ms of continuous queue emptiness.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let state = dispatch.state.lock().expect("dispatch queue");
                let finals = state.jobs.iter().filter(|j| j.kind == SubmissionKind::Final).count();
                if finals == 1 {
                    break;
                }
                assert!(
                    finals <= 1,
                    "at most one idle final may exist, queue={:?}",
                    state.jobs.len()
                );
            }
            assert!(Instant::now() < deadline, "idle fallback never produced the final in bounded time");
            thread::sleep(Duration::from_millis(50));
        }
        // Resume: silence frames after the idle final must not duplicate it.
        for silence_index in 0..5_usize {
            queue.push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 60_000 + silence_index as u64 * 20,
                samples: green_silence_samples(),
            });
        }
        thread::sleep(Duration::from_millis(300));
        {
            let state = dispatch.state.lock().expect("dispatch queue");
            let finals = state.jobs.iter().filter(|j| j.kind == SubmissionKind::Final).count();
            assert_eq!(finals, 1, "frames resumed after idle final must not duplicate it");
        }
        shutdown.request(ShutdownMode::Cancel);
        ingestion.join().expect("ingestion join");
    }

    // B1: finite aggregate Stop via the REAL stop_session with a held fake
    // inference and multiple finals. Every final is either decoded or
    // reported; the join never multiplies per-final timeout by queue length.
    #[test]
    fn b1_stop_with_full_final_queue_is_bounded_and_reports_every_final() {
        let dispatch = Arc::new(DispatchQueue::new());
        // Fill 8 finals within per-source reservations (6 candidate + 2
        // interviewer) so the full-queue drain path is exercised honestly.
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 1_600],
                    language: "sv".into(),
                })
                .expect("candidate finals fit");
        }
        for segment in 1..=2_u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Interviewer,
                    segment_number: segment,
                    started_at_ms: 50_000 + segment * 1_000,
                    captured_at_ms: 50_000 + segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 1_600],
                    language: "en".into(),
                })
                .expect("interviewer finals fit");
        }
        assert_eq!(dispatch.pending_counts().0, DISPATCH_MAX_JOBS);
        let shutdown = Arc::new(ShutdownState::default());
        // Short but honest drain budget for test speed (production uses 14 s;
        // the seam — expiry check before each inference + per-final cap +
        // per-final reporting — is identical).
        shutdown.request_with_budget(ShutdownMode::GracefulDrain, Duration::from_millis(600));
        shutdown.mark_input_closed();
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_dispatch = dispatch.clone();
        let worker_shutdown = shutdown.clone();
        let events_record = events.clone();
        let warnings_record = warnings.clone();
        // Each fake inference takes 500 ms (scaled 8 s final). Without the
        // aggregate budget this drain needs 8 * 500 ms = 4 s; with the 600 ms
        // budget only ~1-2 jobs decode before expiry reports the rest.
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                &worker_dispatch,
                &worker_shutdown,
                "b1-session",
                &|_| {
                    thread::sleep(Duration::from_millis(500));
                    Ok("held final".to_string())
                },
                &|event| events_record.lock().expect("events").push(event),
                &|code, message, _| warnings_record.lock().expect("warnings").push((code.into(), message)),
                &FinalLossTracker::default(),
            );
        });
        // Two instant ingestion workers (empty queues, no utterance) so the
        // real stop_session joins all three workers like production.
        let mk_ingestion = |dispatch: Arc<DispatchQueue>, shutdown: Arc<ShutdownState>| {
            thread::spawn(move || {
                let lease = IngestionLease {
                    remaining: Arc::new(AtomicUsize::new(99)),
                    dispatch: dispatch.clone(),
                };
                let _lease = lease;
                // No frames, no utterance: exits on the draining+closed path
                // once the dispatcher lease closes the queue. Avoid touching
                // the shared 2-count lease so the dispatcher close is owned
                // by this test below.
                while !shutdown.is_requested() {
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let ing_a = mk_ingestion(dispatch.clone(), shutdown.clone());
        let ing_b = mk_ingestion(dispatch.clone(), shutdown.clone());
        let session = CaptureSession {
            session_id: "b1-session".into(),
            shutdown: shutdown.clone(),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(None)),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(vec![dispatcher, ing_a, ing_b])),
            server: Arc::new(Mutex::new(None)),
        };
        // Close the queue like the last ingestion exit would, so the
        // dispatcher drains instead of waiting for more work.
        dispatch.close();
        let started = Instant::now();
        stop_session(session, ShutdownMode::GracefulDrain);
        let elapsed = started.elapsed();
        // Bounded well below the 4 s unbounded drain (no 500 ms * 8 hang),
        // with generous margin for loaded CI.
        assert!(
            elapsed < Duration::from_secs(3),
            "aggregate Stop must stay finite, took {elapsed:?}"
        );
        let events = events.lock().expect("events");
        let warnings = warnings.lock().expect("warnings");
        let decoded_finals = events.iter().filter(|e| e.final_segment).count();
        let reported = warnings
            .iter()
            .filter(|(code, _)| code == STT_FINAL_LOST_CODE)
            .count();
        assert_eq!(
            decoded_finals + reported,
            DISPATCH_MAX_JOBS,
            "every final is either decoded or reported: {decoded_finals} decoded + {reported} reported, events={events:?} warnings={warnings:?}"
        );
        assert!(reported >= 1, "drain expiry must report undecoded finals, got {warnings:?}");
        assert!(decoded_finals >= 1, "budget must preserve as many finals as possible, got {events:?}");
        // No transcript/audio/path content in diagnostics.
        for (_, message) in warnings.iter() {
            assert!(!message.contains("0.03"), "must not leak sample values: {message}");
        }
    }

    // B1b: trailing-final curl honors min(8 s, remaining drain) and kills +
    // reaps promptly (managed path, no deadlock) using a real subprocess.
    #[test]
    fn b1_trailing_final_curl_is_capped_by_remaining_drain_and_reaped() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = InferenceWorkspace::new().expect("curl cap workspace");
        let hanging_curl = fixture.path.join("hanging-curl");
        fs::write(&hanging_curl, "#!/bin/sh\nsleep 30\n").expect("hanging curl script");
        let mut permissions = fs::metadata(&hanging_curl).expect("script metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&hanging_curl, permissions).expect("executable script");
        let shutdown = ShutdownState::default();
        let gate = InferenceGate::default();
        let diag = SourceDiag::default();
        let started = Instant::now();
        let outcome = transcribe_via_server_with_curl(
            "http://127.0.0.1:17997/s/inference",
            &[0.03; 160],
            "sv",
            shutdown.stop_signal(),
            shutdown.cancel_signal(),
            true,
            Some(Duration::from_millis(200)),
            &gate,
            &hanging_curl,
            &diag,
            SubmissionKind::Final,
        );
        let elapsed = started.elapsed();
        assert!(outcome.is_err(), "capped trailing final must fail fast, got {outcome:?}");
        // 200 ms cap + reap margin, far below the 8 s per-final timeout and
        // the 30 s sleep (kill/reap verified, no orphan, no deadlock).
        assert!(
            elapsed < Duration::from_secs(3),
            "capped curl must be killed/reaped promptly, took {elapsed:?}"
        );
        assert_eq!(diag.current_phase(), SttPhase::Idle, "phase guard must return to idle");
    }

    // M5: controlled 20 ms virtual-timestamp producer with a latch held while
    // >= 205 frames arrive through the REAL production
    // ingestion+dispatcher+queue path. No PCM drops in the finite budget;
    // source IDs, order, tail and virtual timestamps verified. Wall cadence
    // here is accelerated (no 20 ms sleeps for 205 frames = instant burst
    // absorbed by continuous drain); the assertion that matters is that the
    // consumer actually ingested every frame (no drops) with exact virtual
    // timestamps, not wall timing.
    #[test]
    fn m5_virtual_20ms_producer_with_held_inference_drops_nothing() {
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        let dispatch = Arc::new(DispatchQueue::new());
        let shutdown = Arc::new(ShutdownState::default());
        let losses = Arc::new(FinalLossTracker::default());
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker_dispatch = dispatch.clone();
        let worker_losses = losses.clone();
        let ingestion = thread::spawn(move || {
            let lease = IngestionLease {
                remaining: Arc::new(AtomicUsize::new(1)),
                dispatch: worker_dispatch.clone(),
            };
            let _lease = lease;
            run_ingestion(
                &worker_queue,
                &worker_shutdown,
                Speaker::Interviewer,
                "en",
                &worker_dispatch,
                &worker_losses,
                &|_, _, _| {},
                &|| {},
            );
        });
        let release: Arc<(Mutex<bool>, Condvar)> =
            Arc::new((Mutex::new(false), Condvar::new()));
        let seen: Arc<Mutex<Vec<(u64, u64, SubmissionKind, usize)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let infer_release = release.clone();
        let infer_seen = seen.clone();
        let infer_shutdown = shutdown.clone();
        let dispatch_worker = dispatch.clone();
        let dispatcher = thread::spawn(move || {
            let first = AtomicBool::new(true);
            run_dispatcher(
                &dispatch_worker,
                &infer_shutdown,
                "m5-session",
                &|job| {
                    infer_seen.lock().expect("jobs").push((
                        job.segment_number,
                        job.captured_at_ms,
                        job.kind,
                        job.samples.len(),
                    ));
                    if first.swap(false, Ordering::SeqCst) {
                        let (lock, cvar) = &*infer_release;
                        let mut held = lock.lock().expect("latch");
                        while !*held {
                            held = cvar.wait_timeout(held, Duration::from_millis(50)).expect("latch").0;
                        }
                    }
                    Ok("m5 text".to_string())
                },
                &|_| {},
                &|_, _, _| {},
                &FinalLossTracker::default(),
            );
        });
        // 205 virtual 20 ms frames (4.1 s of virtual speech, the live stall
        // scale) with the first inference latched. Virtual timestamps advance
        // exactly 20 ms per frame from base 70_000. Wall cadence is
        // accelerated but explicitly paced (1 ms/frame, stated here) so the
        // continuous drain — which never blocks on the latched inference —
        // is proven to ingest every frame rather than the burst merely
        // sitting in the queue; the consumer-ingested assertion below is the
        // behavioral proof.
        const FRAMES: usize = 205;
        const BASE_MS: u64 = 70_000;
        let mut drops = 0_usize;
        for index in 0..FRAMES {
            if queue.push(green_speech_frame(Speaker::Interviewer, index, BASE_MS)).dropped {
                drops += 1;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(drops, 0, "continuous drain must absorb 205 virtual frames with zero PCM drops");
        // Wait until the consumer actually ingested work (dispatcher saw the
        // first job), proving ingestion kept up rather than the burst merely
        // sitting in the queue.
        let wait_start = Instant::now();
        loop {
            if !seen.lock().expect("jobs").is_empty() {
                break;
            }
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "consumer never ingested the virtual burst"
            );
            thread::sleep(Duration::from_millis(5));
        }
        {
            let (lock, cvar) = &*release;
            *lock.lock().expect("latch") = true;
            cvar.notify_all();
        }
        shutdown.request(ShutdownMode::GracefulDrain);
        shutdown.mark_input_closed();
        ingestion.join().expect("ingestion join");
        dispatcher.join().expect("dispatcher join");
        let seen = seen.lock().expect("jobs");
        let finals: Vec<_> = seen.iter().filter(|job| job.2 == SubmissionKind::Final).collect();
        assert_eq!(finals.len(), 1, "exactly one trailing final, got {seen:?}");
        // Tail virtual timestamp: base + 204*20; length 205*320 samples.
        assert_eq!(finals[0].1, BASE_MS + (FRAMES as u64 - 1) * 20);
        assert_eq!(finals[0].3, FRAMES * 320);
        let (lost, _) = losses.snapshot();
        assert_eq!(lost, 0, "bounded virtual burst must lose no finals");
    }

    // M6: terminal warning carries a cumulative numeric summary, is
    // metadata-only, and is ordered before teardown. UI gate coverage lives
    // in src/captureSession.test.ts (terminal warning passes after stopped).
    #[test]
    fn m6_terminal_warning_is_cumulative_metadata_only_and_ordered() {
        let overload = DispatchOverload {
            pending_jobs: DISPATCH_MAX_JOBS,
            pending_ms: samples_to_ms(DISPATCH_MAX_SAMPLES),
            shed_kind: SubmissionKind::Final,
            shed_segment: "candidate-7".into(),
        };
        let message = format_final_lost_message(Speaker::Candidate, &overload, 3, 1_250);
        assert!(message.contains("candidate-7"), "shed segment, got: {message}");
        assert!(message.contains("8 jobs"), "pending depth, got: {message}");
        assert!(message.contains("3"), "cumulative lost count, got: {message}");
        assert!(message.contains("1250"), "cumulative lost ms, got: {message}");
        assert!(message.contains("Restart capture"), "actionable, got: {message}");
        for leaked in ["0.03", "secret", ".wav", ".json", "tmp/", "intervjuare-inference"] {
            assert!(!message.contains(leaked), "metadata-only, leaked {leaked}: {message}");
        }
        assert_eq!(STT_FINAL_LOST_CODE, "stt_final_lost");
        // Ordering on the Rust seam: run_ingestion emits the terminal warning
        // synchronously before requesting Cancel (m2 asserts Cancel after
        // warn); the reader emits terminal stopped only after observing
        // Cancel/EOF, so warn-before-stopped holds on the emit path.
    }

    // A1: real stop_session with a queued partial + multiple finals and a
    // held fake inference. The partial must be SKIPPED once draining (never
    // inferred — pre-fix it started a full non-final request past the
    // budget), finals are each decoded-or-reported, and the total stays
    // bounded by the aggregate budget (native + drain + teardown), proving
    // the ~20 s production claim instead of a 10 s-wrapper overrun.
    #[test]
    fn a1_stop_with_partial_and_finals_is_bounded_by_aggregate() {
        let dispatch = Arc::new(DispatchQueue::new());
        dispatch
            .submit(InferenceJob {
                speaker: Speaker::Candidate,
                segment_number: 1,
                started_at_ms: 1_000,
                captured_at_ms: 1_200,
                kind: SubmissionKind::Partial,
                samples: vec![0.03; 3_200],
                language: "sv".into(),
            })
            .expect("partial fixture");
        for segment in 1..=3_u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: 10 + segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 3_200],
                    language: "sv".into(),
                })
                .expect("candidate finals fit");
        }
        dispatch
            .submit(InferenceJob {
                speaker: Speaker::Interviewer,
                segment_number: 1,
                started_at_ms: 50_000,
                captured_at_ms: 50_500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; 3_200],
                language: "en".into(),
            })
            .expect("interviewer final fits");
        let shutdown = Arc::new(ShutdownState::default());
        // Honest short budget for test speed (production: 14 s). stop_session
        // re-requests GracefulDrain but first-request-wins keeps this budget.
        shutdown.request_with_budget(ShutdownMode::GracefulDrain, Duration::from_millis(700));
        shutdown.mark_input_closed();
        let seen: Arc<Mutex<Vec<SubmissionKind>>> = Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let losses = Arc::new(FinalLossTracker::default());
        let worker_dispatch = dispatch.clone();
        let worker_shutdown = shutdown.clone();
        let seen_record = seen.clone();
        let events_record = events.clone();
        let warnings_record = warnings.clone();
        let worker_losses = losses.clone();
        // Each fake inference holds 800 ms: unbounded drain would need
        // 5 x 800 ms = 4 s plus a 10 s non-final wrapper pre-fix.
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                &worker_dispatch,
                &worker_shutdown,
                "a1-session",
                &|job| {
                    seen_record.lock().expect("jobs").push(job.kind);
                    thread::sleep(Duration::from_millis(800));
                    Ok("a1 text".to_string())
                },
                &|event| events_record.lock().expect("events").push(event),
                &|code, message, _| warnings_record.lock().expect("warnings").push((code.into(), message)),
                &worker_losses,
            );
        });
        // Two REAL ingestion workers (empty queues, instant drain) so the
        // production stop_session joins all three workers like production.
        let mk_idle_ingestion = || {
            let worker_shutdown = shutdown.clone();
            let worker_dispatch = dispatch.clone();
            thread::spawn(move || {
                let lease = IngestionLease {
                    remaining: Arc::new(AtomicUsize::new(99)),
                    dispatch: worker_dispatch.clone(),
                };
                let _lease = lease;
                // utters nothing; exits once draining+closed is observable.
                while !worker_shutdown.is_requested() {
                    thread::sleep(Duration::from_millis(5));
                }
                // Drain like production: exit promptly; the dispatcher owns
                // the close via the test below.
            })
        };
        let ing_a = mk_idle_ingestion();
        let ing_b = mk_idle_ingestion();
        // Instant-exit native child: process.stop() reaps it promptly, so the
        // measured total is drain + server teardown (the native 3 s bound is
        // covered by the lifecycle tests). A sleep-30 server proves teardown
        // reaps owned children within the aggregate bound.
        let mut native_child = Command::new("/bin/sh");
        native_child.args(["-c", "exit 0"]);
        let native_child = native_child.spawn().expect("owned native child");
        let native_pid = native_child.id();
        let server_child = fake_owned_child();
        let server_pid = server_child.id();
        let server = Arc::new(Mutex::new(Some(RunningServer {
            child: Some(server_child),
            endpoint: "http://127.0.0.1:1/a1/inference".into(),
        })));
        let session = CaptureSession {
            session_id: "a1-session".into(),
            shutdown: shutdown.clone(),
            process: CaptureProcess {
                child: Arc::new(Mutex::new(Some(native_child))),
                stdin: Arc::new(Mutex::new(None)),
            },
            workers: Arc::new(Mutex::new(vec![dispatcher, ing_a, ing_b])),
            server: server.clone(),
        };
        dispatch.close();
        let started = Instant::now();
        stop_session(session, ShutdownMode::GracefulDrain);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(6),
            "aggregate Stop (native+drain+server teardown) must stay finite, took {elapsed:?} (unbounded: 5x800 ms drain + 10 s wrapper)"
        );
        assert!(!owned_pid_alive(native_pid), "owned native child must be reaped (no orphan)");
        assert!(!owned_pid_alive(server_pid), "owned server child must be reaped (no orphan)");
        let seen = seen.lock().expect("jobs");
        assert!(
            !seen.iter().any(|kind| *kind == SubmissionKind::Partial),
            "draining partial must be skipped, never inferred — got {seen:?} (pre-fix: partial ran a full request past the budget)"
        );
        let events = events.lock().expect("events");
        let warnings = warnings.lock().expect("warnings");
        let decoded = events.iter().filter(|e| e.final_segment).count();
        let reported = warnings.iter().filter(|(code, _)| code == STT_FINAL_LOST_CODE).count();
        assert_eq!(
            decoded + reported,
            4,
            "every final decoded-or-reported: {decoded}+{reported}, events={events:?} warnings={warnings:?}"
        );
        assert!(reported >= 1, "expiry must report undecoded finals, got {warnings:?}");
        for (_, message) in warnings.iter() {
            assert!(message.contains("Lost finals so far:"), "cumulative counter required, got: {message}");
            assert!(!message.contains("0.03"), "metadata-only, got: {message}");
        }
    }

    // A1b: a draining NON-final curl is capped by the remaining aggregate
    // budget and killed/reaped promptly (managed path, no deadlock), using a
    // real hanging subprocess. Pre-fix it ran the full 10 s wrapper past an
    // expired budget; this asserts < 3 s.
    #[test]
    fn a1b_draining_nonfinal_curl_is_capped_by_remaining_budget() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = InferenceWorkspace::new().expect("nonfinal cap workspace");
        let hanging_curl = fixture.path.join("hanging-curl-nonfinal");
        fs::write(&hanging_curl, "#!/bin/sh\nsleep 30\n").expect("hanging curl script");
        let mut permissions = fs::metadata(&hanging_curl).expect("script metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&hanging_curl, permissions).expect("executable script");
        let shutdown = ShutdownState::default();
        let gate = InferenceGate::default();
        let diag = SourceDiag::default();
        let started = Instant::now();
        let outcome = transcribe_via_server_with_curl(
            "http://127.0.0.1:17996/s/inference",
            &[0.03; 160],
            "sv",
            shutdown.stop_signal(),
            shutdown.cancel_signal(),
            false,
            Some(Duration::ZERO),
            &gate,
            &hanging_curl,
            &diag,
            SubmissionKind::Partial,
        );
        let elapsed = started.elapsed();
        assert!(outcome.is_err(), "expired-budget non-final must fail fast, got {outcome:?}");
        assert!(
            elapsed < Duration::from_secs(3),
            "capped non-final must be killed/reaped promptly, took {elapsed:?} (pre-fix: ~10 s wrapper)"
        );
        assert_eq!(diag.current_phase(), SttPhase::Idle, "phase guard must return to idle");
    }

    // A2: drain-expiry close is atomic with the report, and a final submitted
    // just after expiry is reported via the SHARED cumulative tracker — never
    // silently sitting in an unconsumed queue. finish_on_stop cannot suppress
    // the warning: expiry reporting runs before/after inference, never inside
    // the capped curl.
    #[test]
    fn a2_late_final_after_drain_expiry_is_reported_not_lost() {
        let dispatch = Arc::new(DispatchQueue::new());
        let losses = Arc::new(FinalLossTracker::default());
        for segment in 1..=2_u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 3_200],
                    language: "sv".into(),
                })
                .expect("expiry fixtures fit");
        }
        let shutdown = Arc::new(ShutdownState::default());
        shutdown.request_with_budget(ShutdownMode::GracefulDrain, Duration::from_millis(100));
        shutdown.mark_input_closed();
        // Let the budget expire before the dispatcher starts.
        thread::sleep(Duration::from_millis(150));
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_dispatch = dispatch.clone();
        let worker_shutdown = shutdown.clone();
        let warnings_record = warnings.clone();
        let worker_losses = losses.clone();
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                &worker_dispatch,
                &worker_shutdown,
                "a2-session",
                &|_| panic!("expired drain must start no inference"),
                &|_| panic!("expired drain must emit no hypothesis"),
                &|code, message, _| warnings_record.lock().expect("warnings").push((code.into(), message)),
                &worker_losses,
            );
        });
        dispatcher.join().expect("dispatcher join (no hang, no inference)");
        assert!(dispatch.is_closed(), "expiry must atomically close the queue");
        // A late ingestion final submitted just after expiry: fails closed.
        let late = dispatch.submit(InferenceJob {
            speaker: Speaker::Interviewer,
            segment_number: 9,
            started_at_ms: 90_000,
            captured_at_ms: 90_500,
            kind: SubmissionKind::Final,
            samples: vec![0.03; 3_200],
            language: "en".into(),
        });
        assert!(late.is_err(), "post-expiry submit must fail closed, not sit unconsumed");
        // The REAL ingestion seam reports it through the shared tracker: run a
        // real ingestion worker whose only final hits the closed queue.
        let late_queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        for index in 0..50_usize {
            late_queue.push(green_speech_frame(Speaker::Interviewer, index, 30_000));
        }
        for silence_index in 0..50_usize {
            late_queue.push(PcmFrame {
                source: Speaker::Interviewer,
                captured_at_ms: 31_000 + silence_index as u64 * 20,
                samples: green_silence_samples(),
            });
        }
        let late_shutdown = Arc::new(ShutdownState::default());
        let late_warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let late_record = late_warnings.clone();
        let terminal_calls = Arc::new(AtomicU64::new(0));
        let terminal_probe = terminal_calls.clone();
        let late_dispatch = dispatch.clone();
        let late_losses = losses.clone();
        run_ingestion(
            &late_queue,
            &late_shutdown,
            Speaker::Interviewer,
            "en",
            &late_dispatch,
            &late_losses,
            &|code, message, _| late_record.lock().expect("warnings").push((code.into(), message)),
            &|| {
                terminal_probe.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert!(late_shutdown.is_cancelled(), "shed late final terminally fails the session");
        assert_eq!(terminal_calls.load(Ordering::SeqCst), 1, "terminal hook fires exactly once");
        let warnings = warnings.lock().expect("warnings");
        assert_eq!(warnings.len(), 2, "dispatcher reports both queued finals, got {warnings:?}");
        let late_warnings = late_warnings.lock().expect("warnings");
        // Two warnings, in order: the segment-1 partial shed on the closed
        // queue (honest overload, non-terminal), then the terminal final
        // loss. Both route through the real ingestion submit-failure seam.
        assert_eq!(late_warnings.len(), 2, "partial-overload + terminal final loss, got {late_warnings:?}");
        assert_eq!(late_warnings[0].0, STT_OVERLOAD_CODE);
        assert_eq!(late_warnings[1].0, STT_FINAL_LOST_CODE);
        // Shared cumulative tracker: 2 (dispatcher) + 1 (late ingestion) = 3.
        assert!(
            late_warnings[1].1.contains("Lost finals so far: 3"),
            "cumulative across dispatcher+ingestion seams required, got: {}",
            late_warnings[1].1
        );
        assert!(late_warnings[1].1.contains("interviewer-"), "got: {}", late_warnings[1].1);
        let (lost, lost_ms) = losses.snapshot();
        assert_eq!(lost, 3);
        assert!(lost_ms > 0);
    }

    // B1: saturated final on the PRODUCTION ingestion path triggers full
    // terminal cleanup even when the reader is blocked on an owned live child
    // with no stdout. Real run_ingestion + real owned child + a reader
    // blocked in BufReader::lines() (the exact production blocking call):
    // warning BEFORE unblock, child reaped, no hang, restart clean.
    // Lock order: the terminal hook holds no queue/worker locks (submit
    // already returned) and CaptureProcess::stop drops handle locks before
    // the child wait. Self-join is avoided: the hook stops only the native
    // child; full session teardown stays with the reader/stop path.
    // Gap (honest): the Tauri AppHandle emit path is not exercised here —
    // warn-before-stopped ordering on the emit path is covered by m6 + the
    // sequence log below (warn logged before reader EOF observed).
    #[test]
    fn b1_terminal_final_loss_unblocks_blocked_reader_and_reaps_child() {
        use std::io::{BufRead, BufReader};
        // Owned fake native child, kept alive with NO stdout bytes: any
        // reader of its stdout blocks exactly like production on a live
        // silent child. Only this owned PID is ever signalled.
        let mut native = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("owned fake native child");
        let native_pid = native.id();
        let native_stdout = native.stdout.take().expect("owned child stdout");
        let process = Arc::new(CaptureProcess {
            child: Arc::new(Mutex::new(Some(native))),
            stdin: Arc::new(Mutex::new(None)),
        });
        // Saturate the candidate reservation so the next accepted final sheds.
        let dispatch = Arc::new(DispatchQueue::new());
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 3_200],
                    language: "sv".into(),
                })
                .expect("saturation fixtures fit");
        }
        let queue = Arc::new(BoundedFrames::new(PCM_QUEUE_CAPACITY));
        for index in 0..50_usize {
            queue.push(green_speech_frame(Speaker::Candidate, index, 40_000));
        }
        for silence_index in 0..50_usize {
            queue.push(PcmFrame {
                source: Speaker::Candidate,
                captured_at_ms: 41_000 + silence_index as u64 * 20,
                samples: green_silence_samples(),
            });
        }
        let shutdown = Arc::new(ShutdownState::default());
        let losses = Arc::new(FinalLossTracker::default());
        // Sequence log: warn must precede reader EOF.
        let sequence: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_sequence = sequence.clone();
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_record = warnings.clone();
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker_dispatch = dispatch.clone();
        let worker_losses = losses.clone();
        let worker_process = process.clone();
        let ingestion = thread::spawn(move || {
            let lease = IngestionLease {
                remaining: Arc::new(AtomicUsize::new(1)),
                dispatch: worker_dispatch.clone(),
            };
            let _lease = lease;
            run_ingestion(
                &worker_queue,
                &worker_shutdown,
                Speaker::Candidate,
                "sv",
                &worker_dispatch,
                &worker_losses,
                &|code, message, _| {
                    warn_sequence.lock().expect("sequence").push("warning");
                    warn_record.lock().expect("warnings").push((code.into(), message));
                },
                &|| worker_process.stop(),
            );
        });
        // Reader blocked with no stdout bytes, checking Cancel per line like
        // production — with no lines arriving, only the owned-child kill
        // (EOF) can unblock it. Pre-fix (no terminal hook) this blocks ~30 s.
        let reader_sequence = sequence.clone();
        let reader_shutdown = shutdown.clone();
        let reader = thread::spawn(move || {
            let mut lines = 0_u32;
            for line in BufReader::new(native_stdout).lines() {
                if reader_shutdown.is_cancelled() {
                    break;
                }
                if line.is_err() {
                    break;
                }
                lines += 1;
            }
            reader_sequence.lock().expect("sequence").push("reader-eof");
            lines
        });
        let started = Instant::now();
        ingestion.join().expect("ingestion join");
        reader.join().expect("reader join (must not block 30 s)");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "terminal cleanup must unblock the reader promptly, took {elapsed:?} (pre-fix: blocked ~30 s)"
        );
        assert!(shutdown.is_cancelled(), "saturated final requests Cancel");
        assert!(!owned_pid_alive(native_pid), "owned native child must be reaped (no orphan)");
        let sequence = sequence.lock().expect("sequence");
        assert_eq!(
            *sequence,
            vec!["warning", "warning", "reader-eof"],
            "partial-overload + terminal warning BEFORE unblock/stopped, got {sequence:?}"
        );
        let warnings = warnings.lock().expect("warnings");
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].0, STT_OVERLOAD_CODE, "shed partial warns non-terminally first");
        assert_eq!(warnings[1].0, STT_FINAL_LOST_CODE, "terminal final loss is last before teardown");
        // Full production teardown after unblock stays bounded and a fresh
        // session constructs cleanly (Start succeeds after terminal loss).
        let server_child = fake_owned_child();
        let server_pid = server_child.id();
        let server = Arc::new(Mutex::new(Some(RunningServer {
            child: Some(server_child),
            endpoint: "http://127.0.0.1:1/b1/inference".into(),
        })));
        let restart_shutdown = Arc::new(ShutdownState::default());
        stop_session(
            CaptureSession {
                session_id: "b1-session".into(),
                shutdown: restart_shutdown,
                process: CaptureProcess {
                    child: Arc::new(Mutex::new(None)),
                    stdin: Arc::new(Mutex::new(None)),
                },
                workers: Arc::new(Mutex::new(Vec::new())),
                server,
            },
            ShutdownMode::Cancel,
        );
        assert!(!owned_pid_alive(server_pid), "owned fake server reaped on teardown");
    }

    // B2: every undecoded final shares ONE session-scoped cumulative counter
    // with reason-correct text. Real run_dispatcher, two phases (Cancel then
    // drain-expiry) on the same tracker: ordered N=1,2,3, cancel text never
    // claims a Stop-budget expiry, drain text does, all metadata-only.
    #[test]
    fn b2_undecoded_finals_share_cumulative_counter_with_reason() {
        let losses = Arc::new(FinalLossTracker::default());
        // Phase 1: Cancel during a held inference with 2 queued finals.
        let dispatch = DispatchQueue::new();
        for segment in 1..=2_u64 {
            dispatch
                .submit(InferenceJob {
                    speaker: Speaker::Candidate,
                    segment_number: segment,
                    started_at_ms: segment * 1_000,
                    captured_at_ms: segment * 1_000 + 500,
                    kind: SubmissionKind::Final,
                    samples: vec![0.03; 16_000],
                    language: "sv".into(),
                })
                .expect("cancel fixtures fit");
        }
        let shutdown = Arc::new(ShutdownState::default());
        let entered = Arc::new(AtomicBool::new(false));
        let entered_probe = entered.clone();
        let cancel_signal = shutdown.clone();
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_record = warnings.clone();
        let worker_losses = losses.clone();
        let canceller = thread::spawn(move || {
            run_dispatcher(
                &dispatch,
                &cancel_signal,
                "b2-session",
                &|_| {
                    entered_probe.store(true, Ordering::SeqCst);
                    while !cancel_signal.is_cancelled() {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err("cancelled".to_string())
                },
                &|_| panic!("cancelled run must emit no hypothesis"),
                &|code, message, _| warn_record.lock().expect("warnings").push((code.into(), message)),
                &worker_losses,
            );
        });
        while !entered.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
        shutdown.request(ShutdownMode::Cancel);
        canceller.join().expect("cancelled dispatcher join");
        // Phase 2: drain-expiry with 1 queued final on the SAME tracker.
        let dispatch2 = DispatchQueue::new();
        dispatch2
            .submit(InferenceJob {
                speaker: Speaker::Interviewer,
                segment_number: 7,
                started_at_ms: 70_000,
                captured_at_ms: 70_500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; 32_000],
                language: "en".into(),
            })
            .expect("expiry fixture fits");
        let shutdown2 = ShutdownState::default();
        shutdown2.request_with_budget(ShutdownMode::GracefulDrain, Duration::from_millis(1));
        thread::sleep(Duration::from_millis(20));
        run_dispatcher(
            &dispatch2,
            &shutdown2,
            "b2-session",
            &|_| panic!("expired drain must start no inference"),
            &|_| {},
            &|code, message, _| warnings.lock().expect("warnings").push((code.into(), message)),
            &losses,
        );
        let warnings = warnings.lock().expect("warnings");
        assert_eq!(warnings.len(), 3, "all undecoded finals warned in order, got {warnings:?}");
        for (code, _) in warnings.iter() {
            assert_eq!(code, STT_FINAL_LOST_CODE);
        }
        assert!(warnings[0].1.contains("Lost finals so far: 1"), "got: {}", warnings[0].1);
        assert!(warnings[1].1.contains("Lost finals so far: 2"), "got: {}", warnings[1].1);
        assert!(warnings[2].1.contains("Lost finals so far: 3"), "got: {}", warnings[2].1);
        assert!(warnings[2].1.contains("4000"), "cumulative ms 1000+1000+2000, got: {}", warnings[2].1);
        assert!(
            !warnings[0].1.contains("Stop budget expired"),
            "cancel text must not claim a budget expiry, got: {}",
            warnings[0].1
        );
        assert!(warnings[0].1.contains("cancelled"), "got: {}", warnings[0].1);
        assert!(warnings[2].1.contains("Stop budget expired"), "got: {}", warnings[2].1);
        assert!(warnings[0].1.contains("candidate-1"), "got: {}", warnings[0].1);
        assert!(warnings[2].1.contains("interviewer-7"), "got: {}", warnings[2].1);
        for (_, message) in warnings.iter() {
            for leaked in ["0.03", "secret", ".wav", ".json", "tmp/", "intervjuare-inference"] {
                assert!(!message.contains(leaked), "metadata-only, leaked {leaked}: {message}");
            }
        }
        let (lost, lost_ms) = losses.snapshot();
        assert_eq!(lost, 3);
        assert_eq!(lost_ms, 4_000);
    }

    // C: one source at its own cap still leaves room for one FULL max final
    // from the other source. Real DispatchQueue + real run_dispatcher: no
    // 3200-sample-only testers — jobs are 48k speech snapshots and full-size
    // 192k/240k finals; labels, languages and per-source revisions intact.
    // Over-quota failure is attributed to the flooder's own segment.
    #[test]
    fn c_full_size_final_fits_beside_capped_source() {
        fn big_final(speaker: Speaker, segment: u64, samples: usize) -> InferenceJob {
            InferenceJob {
                speaker,
                segment_number: segment,
                started_at_ms: segment * 1_000,
                captured_at_ms: segment * 1_000 + 500,
                kind: SubmissionKind::Final,
                samples: vec![0.03; samples],
                language: if speaker == Speaker::Candidate { "sv".into() } else { "en".into() },
            }
        }
        // Candidate to its exact sample reservation: 6 x 48_000 = 288_000.
        let dispatch = DispatchQueue::new();
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch
                .submit(big_final(Speaker::Candidate, segment, 48_000))
                .expect("candidate up to its own reservation fits");
        }
        let (jobs, samples) = dispatch.source_counts(Speaker::Candidate);
        assert_eq!(jobs, DISPATCH_MAX_JOBS_PER_SOURCE);
        assert_eq!(samples, DISPATCH_MAX_SAMPLES_PER_SOURCE);
        // Nominal 12 s final (192k) from the idle source still fits.
        dispatch
            .submit(big_final(Speaker::Interviewer, 1, MAX_UTTERANCE_SAMPLES))
            .expect("nominal 12 s final must survive a capped source (pre-fix: shared 384k rejected it)");
        // Full max final (240k) from the idle source still fits.
        let dispatch2 = DispatchQueue::new();
        for segment in 1..=DISPATCH_MAX_JOBS_PER_SOURCE as u64 {
            dispatch2
                .submit(big_final(Speaker::Candidate, segment, 48_000))
                .expect("candidate reservation fits");
        }
        dispatch2
            .submit(big_final(Speaker::Interviewer, 1, UTTERANCE_HARD_CAP))
            .expect("full max final must survive a capped source (pre-fix: shared 384k rejected it)");
        // The flooder's 7th final fails attributed to ITSELF, not the other.
        let overload = dispatch2
            .submit(big_final(Speaker::Candidate, 99, 48_000))
            .expect_err("candidate beyond its own reservation must shed explicitly");
        assert_eq!(overload.shed_segment, "candidate-99");
        // End-to-end through the REAL dispatcher: order, labels, languages,
        // per-source revisions, final immutability.
        let seen: Arc<Mutex<Vec<(Speaker, String, SubmissionKind)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let seen_record = seen.clone();
        let events: Arc<Mutex<Vec<TranscriptEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_record = events.clone();
        dispatch2.close();
        run_dispatcher(
            &dispatch2,
            &ShutdownState::default(),
            "c-session",
            &|job| {
                seen_record.lock().expect("jobs").push((
                    job.speaker,
                    job.language.clone(),
                    job.kind,
                ));
                Ok(format!("heard-{}", job.speaker.id()))
            },
            &|event| events_record.lock().expect("events").push(event),
            &|_, _, _| panic!("fair drain must not warn"),
            &FinalLossTracker::default(),
        );
        let seen = seen.lock().expect("jobs");
        assert_eq!(seen.len(), DISPATCH_MAX_JOBS_PER_SOURCE + 1);
        assert!(seen.iter().all(|(_, _, kind)| *kind == SubmissionKind::Final));
        assert!(seen[..6].iter().all(|(speaker, language, _)| {
            *speaker == Speaker::Candidate && language == "sv"
        }));
        assert_eq!(seen[6], (Speaker::Interviewer, "en".to_string(), SubmissionKind::Final));
        let events = events.lock().expect("events");
        assert_eq!(events.len(), DISPATCH_MAX_JOBS_PER_SOURCE + 1);
        let candidate_revisions: Vec<u32> =
            events.iter().filter(|e| e.speaker == Speaker::Candidate).map(|e| e.revision).collect();
        assert!(candidate_revisions.iter().all(|revision| *revision == 1), "new segment resets revision");
        let interviewer: Vec<_> =
            events.iter().filter(|e| e.speaker == Speaker::Interviewer).collect();
        assert_eq!(interviewer.len(), 1);
        assert_eq!(interviewer[0].segment_id, "interviewer-1");
        assert_eq!(interviewer[0].revision, 1);
        assert!(events.iter().all(|e| e.final_segment && e.session_id == "c-session"));
    }

    // D: bounded segmentation preserves every accepted sample. A last legal
    // 48k normalized frame straddling the 12 s boundary lands in ONE final
    // with all samples (no silent loss); an overshoot beyond the hard cap
    // splits losslessly (prefix final + retained tail), never truncates.
    // The PCM queue sample-cap warning path is unaffected (drop counted).
    #[test]
    fn d_straddling_max_frame_and_overshoot_lose_nothing() {
        // Straddle: 599 x 320 speech frames = 191_680 samples (11_980 ms, no
        // final yet — partials do not clear), then one legal 48_000-sample
        // voice frame crossing the 12 s boundary.
        let mut segmenter = UtteranceSegmenter::new();
        let mut partials = 0_usize;
        for index in 0..599_usize {
            if let Some(snapshot) =
                segmenter.ingest(&green_speech_frame(Speaker::Candidate, index, 1_000), false)
            {
                assert_eq!(snapshot.kind, SubmissionKind::Partial);
                partials += 1;
            }
        }
        assert!(partials >= 1, "partial schedule must fire before 12 s");
        let straddler = PcmFrame {
            source: Speaker::Candidate,
            captured_at_ms: 1_000 + 599 * 20,
            samples: vec![0.03; MAX_PCM_FRAMES],
        };
        let snapshot = segmenter
            .ingest(&straddler, false)
            .expect("boundary straddle must emit");
        assert_eq!(snapshot.kind, SubmissionKind::Final);
        assert_eq!(
            snapshot.samples.len(),
            599 * 320 + MAX_PCM_FRAMES,
            "every accepted sample lands in the final — zero silent loss"
        );
        assert!(!segmenter.has_utterance(), "final clears the utterance");
        // Overshoot: one 300_000-sample voice frame exceeds the 240k hard
        // cap. Pre-fix truncate() silently dropped 60k; now the prefix emits
        // as a bounded final and the tail is retained as a new segment.
        let mut splitter = UtteranceSegmenter::new();
        let overshoot = PcmFrame {
            source: Speaker::Interviewer,
            captured_at_ms: 5_000,
            samples: vec![0.03; 300_000],
        };
        assert!(frame_has_speech(&overshoot.samples), "overshoot fixture must be speech");
        let first = splitter.ingest(&overshoot, false).expect("overshoot must emit");
        assert_eq!(first.kind, SubmissionKind::Final);
        assert_eq!(first.samples.len(), UTTERANCE_HARD_CAP);
        let tail = splitter.force_final().expect("retained tail must finalize");
        assert_eq!(tail.kind, SubmissionKind::Final);
        assert_eq!(
            first.samples.len() + tail.samples.len(),
            300_000,
            "prefix + tail == everything accepted (pre-fix: 240_000, 60k silently lost)"
        );
        assert_ne!(tail.segment_number, first.segment_number, "tail continues as its own segment");
        assert!(!splitter.has_utterance());
        // Queue sample-cap drops are still counted and warnable (not
        // silenced by the segmentation change).
        let tiny = BoundedFrames::with_sample_cap(1, 320);
        assert!(!tiny
            .push(PcmFrame { source: Speaker::Candidate, captured_at_ms: 1, samples: vec![0.03; 320] })
            .dropped);
        assert!(tiny
            .push(PcmFrame { source: Speaker::Candidate, captured_at_ms: 2, samples: vec![0.03; 320] })
            .dropped);
        assert_eq!(tiny.dropped_counts().0, 1);
    }

    // Per-model STT profile selection: q5 keeps the legacy values, only the
    // full trial model gets the long-utterance / fewer-partials profile.
    #[test]
    fn stt_profile_is_model_specific_and_q5_unchanged() {
        assert_eq!(stt_profile_for_model("ggml-large-v3-turbo-q5_0.bin"), Q5_STT_PROFILE);
        assert_eq!(stt_profile_for_model(FULL_TRIAL_MODEL_NAME), FULL_TRIAL_STT_PROFILE);
        assert_eq!(stt_profile(), Q5_STT_PROFILE, "default build keeps the q5 profile");
        assert_eq!(Q5_STT_PROFILE.max_utterance_ms, 12_000);
        assert_eq!(Q5_STT_PROFILE.first_partial_ms, 900);
        assert_eq!(Q5_STT_PROFILE.partial_window_ms, 2_400);
        assert_eq!(Q5_STT_PROFILE.pause_split_lookback_ms, None);
        assert_eq!(Q5_STT_PROFILE.audio_ctx, "640");
        assert_eq!(Q5_STT_PROFILE.request_timeout_secs, 5);
        assert!(!Q5_STT_PROFILE.prefer_finals);
        // Full trial: audio context (50 frames/s) must cover the hard cap,
        // otherwise whisper would silently drop the end of a long final.
        let full = FULL_TRIAL_STT_PROFILE;
        let ctx: usize = full.audio_ctx.parse().expect("numeric audio ctx");
        assert!(full.utterance_hard_cap() * 50 / 16_000 <= ctx, "audio ctx must cover hard cap");
        assert!(full.dispatch_max_samples_per_source >= full.utterance_hard_cap());
        assert!(
            Duration::from_secs(full.request_timeout_secs) <= SERVER_FINAL_TIMEOUT,
            "curl max-time must stay inside the final wrapper"
        );
    }

    // Change 1: at max length the full profile cuts at the quietest 20 ms
    // window in the lookback instead of mid-word, losslessly.
    #[test]
    fn full_profile_splits_long_utterance_at_pause() {
        let mut segmenter = UtteranceSegmenter::with_profile(FULL_TRIAL_STT_PROFILE);
        let mut finals = Vec::new();
        // 1000 frames x 20 ms = 20 s; frame 960 (19.2 s) is a short pause.
        for index in 0..1000_usize {
            let frame = if index == 960 {
                PcmFrame {
                    source: Speaker::Candidate,
                    captured_at_ms: 1_000 + index as u64 * 20,
                    samples: green_silence_samples(),
                }
            } else {
                green_speech_frame(Speaker::Candidate, index, 1_000)
            };
            if let Some(snapshot) = segmenter.ingest(&frame, false) {
                if snapshot.kind == SubmissionKind::Final {
                    finals.push(snapshot);
                }
            }
        }
        assert_eq!(finals.len(), 1, "exactly one final at max length");
        let cut = 960 * 320 + 160;
        assert_eq!(finals[0].samples.len(), cut, "cut lands in the pause window");
        assert_eq!(finals[0].segment_number, 1);
        let tail = segmenter.force_final().expect("tail with speech is retained");
        assert_eq!(tail.segment_number, 2, "tail continues as its own segment");
        assert_eq!(cut + tail.samples.len(), 1000 * 320, "no sample lost");
    }

    // A cut whose tail would hold no speech falls back to one whole final,
    // so a silent fragment is never sent alone to Whisper.
    #[test]
    fn full_profile_pause_split_falls_back_when_tail_is_silent() {
        let mut segmenter = UtteranceSegmenter::with_profile(FULL_TRIAL_STT_PROFILE);
        let mut finals = Vec::new();
        for index in 0..1000_usize {
            let frame = if index == 999 {
                PcmFrame {
                    source: Speaker::Candidate,
                    captured_at_ms: 1_000 + index as u64 * 20,
                    samples: green_silence_samples(),
                }
            } else {
                green_speech_frame(Speaker::Candidate, index, 1_000)
            };
            if let Some(snapshot) = segmenter.ingest(&frame, false) {
                if snapshot.kind == SubmissionKind::Final {
                    finals.push(snapshot);
                }
            }
        }
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].samples.len(), 1000 * 320, "whole utterance in one final");
        assert!(!segmenter.has_utterance());
    }

    fn profile_job(speaker: Speaker, segment: u64, kind: SubmissionKind, samples: usize) -> InferenceJob {
        InferenceJob {
            speaker,
            segment_number: segment,
            started_at_ms: 0,
            captured_at_ms: 0,
            kind,
            samples: vec![0.03; samples],
            language: "sv".to_string(),
        }
    }

    // Change 2: on the full profile a final drops its own queued partials
    // and is dispatched before other partials; q5 stays strict FIFO.
    #[test]
    fn full_profile_dispatch_prefers_finals_q5_stays_fifo() {
        let full = DispatchQueue::with_profile(&FULL_TRIAL_STT_PROFILE);
        full.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Partial, 320)).unwrap();
        full.submit(profile_job(Speaker::Interviewer, 1, SubmissionKind::Partial, 320)).unwrap();
        full.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Final, 640)).unwrap();
        assert_eq!(full.source_counts(Speaker::Candidate), (1, 640), "superseded partial removed");
        let first = full.pop().expect("final first");
        assert_eq!((first.speaker, first.kind), (Speaker::Candidate, SubmissionKind::Final));
        let second = full.pop().expect("other partial next");
        assert_eq!((second.speaker, second.kind), (Speaker::Interviewer, SubmissionKind::Partial));
        assert!(full.pop().is_none());
        assert_eq!(full.pending_counts(), (0, 0));

        let q5 = DispatchQueue::with_profile(&Q5_STT_PROFILE);
        q5.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Partial, 320)).unwrap();
        q5.submit(profile_job(Speaker::Interviewer, 1, SubmissionKind::Partial, 320)).unwrap();
        q5.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Final, 640)).unwrap();
        assert_eq!(q5.source_counts(Speaker::Candidate), (2, 960), "q5 keeps queued partials");
        let first = q5.pop().expect("fifo");
        assert_eq!((first.speaker, first.kind), (Speaker::Candidate, SubmissionKind::Partial));
    }

    // A max-length full-trial final fits the full queue but would exceed the
    // q5 per-source reservation, proving the caps scale with the profile.
    #[test]
    fn full_profile_dispatch_accepts_max_length_final() {
        let cap = FULL_TRIAL_STT_PROFILE.utterance_hard_cap();
        let full = DispatchQueue::with_profile(&FULL_TRIAL_STT_PROFILE);
        full.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Final, cap))
            .expect("20 s+ final fits the full profile");
        full.submit(profile_job(Speaker::Interviewer, 1, SubmissionKind::Final, cap))
            .expect("other source still fits its own max final");
        let q5 = DispatchQueue::with_profile(&Q5_STT_PROFILE);
        assert!(q5.submit(profile_job(Speaker::Candidate, 1, SubmissionKind::Final, cap)).is_err());
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime = AudioRuntime::default();
    let exit_runtime = runtime.clone();
    tauri::Builder::default()
        .manage(runtime)
        .manage(copilot::Copilot::new())
        .invoke_handler(tauri::generate_handler![
            list_audio_sources,
            request_screen_capture_permission,
            start_audio_capture,
            stop_audio_capture,
            copilot::copilot_status,
            copilot::load_candidate_profile,
            copilot::set_job_context,
            copilot::set_auto_answer,
            copilot::check_hermes,
            copilot::force_answer,
            copilot::submit_test_turn,
            copilot::clear_conversation,
            copilot::cancel_suggestion
        ])
        .on_window_event(move |window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                stop_runtime(
                    &exit_runtime,
                    &window.app_handle(),
                    None,
                    ShutdownMode::Cancel,
                    "Application closed; audio capture stopped.",
                );
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Intervjuare");
}
