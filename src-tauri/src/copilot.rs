//! Interview copilot orchestration (Milestone 2): conversation state →
//! context package → one streaming Hermes request → progressive suggestion
//! events (anchors first, then the full answer).
//!
//! Trigger rules (see docs/context-management.md):
//! - an interviewer final that completes the latest interviewer turn starts a
//!   generation automatically (unless auto-answer is off or the whole turn is
//!   a backchannel such as "Mm.");
//! - a new interviewer partial that continues the same turn cancels the
//!   in-flight generation, because the question was not finished yet;
//! - a newer generation always supersedes the older one;
//! - candidate speech never cancels a suggestion (the candidate reads it
//!   while answering).

use crate::{
    context::{self, ContextPackage, JobContext},
    conversation::{self, Conversation, Turn, Update},
    hermes::{self, CancelToken, HermesConfig, HermesConfigSummary, HermesStatus, StreamOutcome},
    profile::{self, Profile, ProfileSummary},
    suggestion::{self, ParsedSuggestion},
    now_ms, Speaker, TranscriptEvent, CURL_BIN,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};
use tauri::{AppHandle, Emitter, Manager, State};

pub const SUGGESTION_EVENT: &str = "suggestion-event";
const MAX_TEST_TURN_CHARS: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionStatus {
    Preparing,
    Streaming,
    Done,
    Skipped,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionTiming {
    /// Captured end of the interviewer's speech for this turn.
    pub turn_ended_at_ms: u64,
    pub requested_at_ms: u64,
    pub first_anchor_at_ms: Option<u64>,
    pub first_answer_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionEvent {
    pub suggestion_id: u64,
    pub status: SuggestionStatus,
    pub question: String,
    pub anchors: Vec<String>,
    pub answer: String,
    /// Cited ids that exist in profile.yaml.
    pub sources: Vec<String>,
    /// Cited ids that do NOT exist in profile.yaml (grounding warning).
    pub unverified_sources: Vec<String>,
    pub no_verified_example: bool,
    pub error: Option<String>,
    pub forced: bool,
    pub timing: SuggestionTiming,
}

#[derive(Clone, Debug)]
struct GenerationPlan {
    id: u64,
    package: ContextPackage,
    question: String,
    turn_ended_at_ms: u64,
    forced: bool,
    citable_ids: BTreeSet<String>,
}

struct ActiveGeneration {
    id: u64,
    token: Arc<CancelToken>,
    turn_first_segment: String,
}

struct CopilotInner {
    conversation: Conversation,
    job: JobContext,
    profile: Option<Arc<Profile>>,
    profile_path: PathBuf,
    profile_error: Option<String>,
    auto_answer: bool,
    active: Option<ActiveGeneration>,
    next_id: u64,
    simulated_segments: u64,
}

impl CopilotInner {
    fn new(profile_path: PathBuf) -> Self {
        let mut inner = Self {
            conversation: Conversation::default(),
            job: JobContext::default(),
            profile: None,
            profile_path,
            profile_error: None,
            auto_answer: true,
            active: None,
            next_id: 1,
            simulated_segments: 0,
        };
        let path = inner.profile_path.clone();
        let _ = inner.load_profile(path);
        inner
    }

    fn load_profile(&mut self, path: PathBuf) -> Result<ProfileSummary, String> {
        self.profile_path = path;
        match profile::load_profile(&self.profile_path) {
            Ok(loaded) => {
                let summary = loaded.summary.clone();
                self.profile = Some(Arc::new(loaded));
                self.profile_error = None;
                Ok(summary)
            }
            Err(error) => {
                // A failed reload never keeps a stale profile from another path.
                self.profile = None;
                self.profile_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn cancel_active(&mut self) {
        if let Some(active) = self.active.take() {
            active.token.cancel();
        }
    }

    fn on_transcript(&mut self, event: &TranscriptEvent) -> Option<(GenerationPlan, Arc<CancelToken>)> {
        if self.conversation.session_id().is_some_and(|session| session != event.session_id) {
            self.cancel_active();
        }
        match self.conversation.apply(event) {
            Update::Ignored | Update::Changed { speaker: Speaker::Candidate, .. } => None,
            Update::Changed { speaker: Speaker::Interviewer, segment_id, final_segment: false } => {
                let continues_active_turn = match (&self.active, self.conversation.turn_of_segment(&segment_id)) {
                    (Some(active), Some(turn)) => turn.first_segment_id == active.turn_first_segment,
                    _ => false,
                };
                if continues_active_turn {
                    self.cancel_active();
                }
                None
            }
            Update::Changed { speaker: Speaker::Interviewer, segment_id, final_segment: true } => {
                if !self.auto_answer {
                    return None;
                }
                let turn = self.conversation.turn_of_segment(&segment_id)?;
                let (history, current) = self.conversation.latest_interviewer_turn()?;
                if !turn.complete || current.first_segment_id != turn.first_segment_id || conversation::is_backchannel(&current.text) {
                    return None;
                }
                Some(self.plan(&history, current, false))
            }
        }
    }

    fn force(&mut self) -> Result<(GenerationPlan, Arc<CancelToken>), String> {
        let (history, current) = self
            .conversation
            .latest_interviewer_turn()
            .ok_or("No interviewer turn yet. Wait for the interviewer, or add a simulated turn.")?;
        Ok(self.plan(&history, current, true))
    }

    fn plan(&mut self, history: &[Turn], current: Turn, forced: bool) -> (GenerationPlan, Arc<CancelToken>) {
        self.cancel_active();
        let id = self.next_id;
        self.next_id += 1;
        let token = Arc::new(CancelToken::default());
        self.active = Some(ActiveGeneration {
            id,
            token: token.clone(),
            turn_first_segment: current.first_segment_id.clone(),
        });
        let plan = GenerationPlan {
            id,
            package: context::build_context(&self.job, self.profile.as_deref(), history, &current),
            question: current.text,
            turn_ended_at_ms: current.ended_at_ms,
            forced,
            citable_ids: self.profile.as_ref().map(|profile| profile.citable_ids.clone()).unwrap_or_default(),
        };
        (plan, token)
    }

    fn finish(&mut self, id: u64) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.active = None;
        }
    }

    fn simulated_event(&mut self, speaker: Speaker, text: &str) -> Result<TranscriptEvent, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("Simulated turn is empty.".into());
        }
        let text: String = text.chars().take(MAX_TEST_TURN_CHARS).collect();
        self.simulated_segments += 1;
        let now = now_ms();
        Ok(TranscriptEvent {
            session_id: self.conversation.session_id().unwrap_or("simulated").to_string(),
            segment_id: format!("simulated-{}-{}", speaker.id(), self.simulated_segments),
            revision: 1,
            speaker,
            text,
            final_segment: true,
            started_at_ms: now,
            ended_at_ms: now,
            captured_at_ms: now,
            emitted_at_ms: now,
            latency_ms: 0,
        })
    }
}

/// Tauri-managed copilot state.
pub struct Copilot(Arc<Mutex<CopilotInner>>);

impl Copilot {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(CopilotInner::new(profile::default_profile_path()))))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, CopilotInner>, String> {
        self.0.lock().map_err(|_| "copilot state poisoned".to_string())
    }
}

/// Hook from the STT dispatcher: called for every emitted transcript event.
/// Never blocks on Hermes; generation runs on its own thread.
pub fn on_transcript(app: &AppHandle, event: &TranscriptEvent) {
    let copilot = app.state::<Copilot>();
    let planned = match copilot.lock() {
        Ok(mut inner) => inner.on_transcript(event),
        Err(_) => None,
    };
    if let Some((plan, token)) = planned {
        spawn_generation(app.clone(), copilot.0.clone(), plan, token);
    }
}

fn spawn_generation(app: AppHandle, state: Arc<Mutex<CopilotInner>>, plan: GenerationPlan, token: Arc<CancelToken>) {
    thread::spawn(move || {
        let id = plan.id;
        let config = hermes::resolve_config();
        run_generation(&plan, &token, &config, CURL_BIN, |event| {
            let _ = app.emit(SUGGESTION_EVENT, event);
        });
        if let Ok(mut inner) = state.lock() {
            inner.finish(id);
        }
    });
}

/// Streams one suggestion and reports every visible change through `emit`.
fn run_generation(
    plan: &GenerationPlan,
    token: &CancelToken,
    config: &HermesConfig,
    curl_bin: &str,
    mut emit: impl FnMut(&SuggestionEvent),
) {
    let mut event = SuggestionEvent {
        suggestion_id: plan.id,
        status: SuggestionStatus::Preparing,
        question: plan.question.clone(),
        anchors: Vec::new(),
        answer: String::new(),
        sources: Vec::new(),
        unverified_sources: Vec::new(),
        no_verified_example: false,
        error: None,
        forced: plan.forced,
        timing: SuggestionTiming {
            turn_ended_at_ms: plan.turn_ended_at_ms,
            requested_at_ms: now_ms(),
            ..SuggestionTiming::default()
        },
    };
    emit(&event);

    let mut raw = String::new();
    let result = hermes::stream_chat(config, &plan.package, token, curl_bin, |delta| {
        raw.push_str(delta);
        if apply_parsed(&mut event, suggestion::parse(&raw, false)) {
            event.status = SuggestionStatus::Streaming;
            emit(&event);
        }
    });

    let parsed = suggestion::parse(&raw, true);
    let (skip, no_verified_example, cited) = (parsed.skip, parsed.no_verified_example, parsed.sources.clone());
    apply_parsed(&mut event, parsed);
    let (sources, unverified): (Vec<String>, Vec<String>) =
        cited.into_iter().partition(|id| plan.citable_ids.contains(id));
    event.sources = sources;
    event.unverified_sources = unverified;
    event.no_verified_example = no_verified_example;
    event.timing.finished_at_ms = Some(now_ms());
    event.status = match result {
        Ok(StreamOutcome::Cancelled) => SuggestionStatus::Cancelled,
        Ok(StreamOutcome::Completed { .. }) if skip => SuggestionStatus::Skipped,
        Ok(StreamOutcome::Completed { .. }) if event.anchors.is_empty() && event.answer.is_empty() => {
            event.error = Some("Hermes returned an empty response.".into());
            SuggestionStatus::Failed
        }
        Ok(StreamOutcome::Completed { .. }) => SuggestionStatus::Done,
        Err(error) => {
            event.error = Some(error);
            SuggestionStatus::Failed
        }
    };
    emit(&event);
}

/// Copies newly visible anchors/answer into the event. Returns true when
/// something the candidate can see changed.
fn apply_parsed(event: &mut SuggestionEvent, parsed: ParsedSuggestion) -> bool {
    let mut changed = false;
    if parsed.anchors != event.anchors {
        if event.anchors.is_empty() && !parsed.anchors.is_empty() {
            event.timing.first_anchor_at_ms = Some(now_ms());
        }
        event.anchors = parsed.anchors;
        changed = true;
    }
    if parsed.answer != event.answer {
        if event.answer.is_empty() && !parsed.answer.is_empty() {
            event.timing.first_answer_at_ms = Some(now_ms());
        }
        event.answer = parsed.answer;
        changed = true;
    }
    changed
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStatus {
    pub profile: Option<ProfileSummary>,
    pub profile_error: Option<String>,
    pub profile_path: String,
    pub job: JobContext,
    pub auto_answer: bool,
    pub hermes: HermesConfigSummary,
}

#[tauri::command]
pub fn copilot_status(copilot: State<'_, Copilot>) -> Result<CopilotStatus, String> {
    let inner = copilot.lock()?;
    Ok(CopilotStatus {
        profile: inner.profile.as_ref().map(|profile| profile.summary.clone()),
        profile_error: inner.profile_error.clone(),
        profile_path: inner.profile_path.display().to_string(),
        job: inner.job.clone(),
        auto_answer: inner.auto_answer,
        hermes: hermes::resolve_config().summary(),
    })
}

#[tauri::command]
pub fn load_candidate_profile(copilot: State<'_, Copilot>, path: Option<String>) -> Result<ProfileSummary, String> {
    let mut inner = copilot.lock()?;
    let path = path
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| inner.profile_path.clone());
    inner.load_profile(path)
}

#[tauri::command]
pub fn set_job_context(copilot: State<'_, Copilot>, job: JobContext) -> Result<(), String> {
    copilot.lock()?.job = job;
    Ok(())
}

#[tauri::command]
pub fn set_auto_answer(copilot: State<'_, Copilot>, enabled: bool) -> Result<(), String> {
    copilot.lock()?.auto_answer = enabled;
    Ok(())
}

#[tauri::command]
pub async fn check_hermes() -> Result<HermesStatus, String> {
    tauri::async_runtime::spawn_blocking(|| hermes::check_status(&hermes::resolve_config(), CURL_BIN))
        .await
        .map_err(|error| format!("Hermes check failed: {error}"))
}

#[tauri::command]
pub fn force_answer(app: AppHandle, copilot: State<'_, Copilot>) -> Result<u64, String> {
    let (plan, token) = copilot.lock()?.force()?;
    let id = plan.id;
    spawn_generation(app, copilot.0.clone(), plan, token);
    Ok(id)
}

/// Test input for working without audio: a typed turn is treated exactly
/// like a final transcript segment from that speaker.
#[tauri::command]
pub fn submit_test_turn(app: AppHandle, copilot: State<'_, Copilot>, speaker: Speaker, text: String) -> Result<TranscriptEvent, String> {
    let (event, planned) = {
        let mut inner = copilot.lock()?;
        let event = inner.simulated_event(speaker, &text)?;
        let planned = inner.on_transcript(&event);
        (event, planned)
    };
    if let Some((plan, token)) = planned {
        spawn_generation(app, copilot.0.clone(), plan, token);
    }
    Ok(event)
}

#[tauri::command]
pub fn clear_conversation(copilot: State<'_, Copilot>) -> Result<(), String> {
    let mut inner = copilot.lock()?;
    inner.cancel_active();
    inner.conversation.reset(None);
    Ok(())
}

#[tauri::command]
pub fn cancel_suggestion(copilot: State<'_, Copilot>) -> Result<(), String> {
    copilot.lock()?.cancel_active();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read as _, Write},
        net::TcpListener,
        time::Duration,
    };

    fn inner() -> CopilotInner {
        CopilotInner::new(PathBuf::from("/nonexistent/profile.yaml"))
    }

    fn event(id: &str, revision: u32, speaker: Speaker, text: &str, final_segment: bool, started: u64) -> TranscriptEvent {
        TranscriptEvent {
            session_id: "s".into(),
            segment_id: id.into(),
            revision,
            speaker,
            text: text.into(),
            final_segment,
            started_at_ms: started,
            ended_at_ms: started + 900,
            captured_at_ms: started + 900,
            emitted_at_ms: started + 1_200,
            latency_ms: 300,
        }
    }

    #[test]
    fn missing_profile_is_reported_not_fatal() {
        let inner = inner();
        assert!(inner.profile.is_none());
        assert!(inner.profile_error.as_deref().unwrap().contains("Could not open"));
    }

    #[test]
    fn interviewer_final_triggers_and_partials_do_not() {
        let mut inner = inner();
        assert!(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Kan du berätta", false, 0)).is_none());
        let (plan, _) = inner
            .on_transcript(&event("interviewer-1", 2, Speaker::Interviewer, "Kan du berätta om dig själv?", true, 0))
            .expect("final interviewer turn triggers");
        assert_eq!(plan.question, "Kan du berätta om dig själv?");
        assert_eq!(plan.turn_ended_at_ms, 900);
        assert!(plan.package.user.contains("CURRENT INTERVIEWER TURN\nKan du berätta om dig själv?"));
        assert!(inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Ja, absolut.", true, 2_000)).is_none());
    }

    #[test]
    fn continuing_the_same_turn_cancels_and_retriggers_with_the_full_question() {
        let mut inner = inner();
        let (_, first_token) = inner
            .on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem.", true, 0))
            .unwrap();
        assert!(inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Som du", false, 1_500)).is_none());
        assert!(first_token.is_cancelled(), "the unfinished question's answer is cancelled");
        assert!(inner.active.is_none());
        let (plan, _) = inner
            .on_transcript(&event("interviewer-2", 2, Speaker::Interviewer, "Som du själv upptäckte.", true, 1_500))
            .unwrap();
        assert_eq!(plan.question, "Berätta om ett problem. Som du själv upptäckte.");
    }

    #[test]
    fn a_new_interviewer_turn_after_the_candidate_does_not_cancel_until_it_is_final() {
        let mut inner = inner();
        let (_, token) = inner
            .on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om Sagobo.", true, 0))
            .unwrap();
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Sagobo är en app.", false, 2_000));
        inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Hur", false, 5_000));
        assert!(!token.is_cancelled(), "candidate is reading the suggestion while answering");
        let (plan, _) = inner
            .on_transcript(&event("interviewer-2", 2, Speaker::Interviewer, "Hur testade du det?", true, 5_000))
            .unwrap();
        assert!(token.is_cancelled(), "newer question supersedes");
        assert!(plan.package.user.contains("INTERVIEWER: Berätta om Sagobo.\nCANDIDATE: Sagobo är en app."));
    }

    #[test]
    fn backchannels_and_disabled_auto_answer_do_not_trigger_but_force_does() {
        let mut inner = inner();
        assert!(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Mm.", true, 0)).is_none());
        let (plan, _) = inner.force().expect("force works for any interviewer turn");
        assert!(plan.forced);
        assert_eq!(plan.question, "Mm.");

        inner.auto_answer = false;
        assert!(inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Vad är SSR?", true, 3_000)).is_none());
        assert!(inner.force().is_ok());
    }

    #[test]
    fn force_without_interviewer_turn_is_an_error() {
        let mut inner = inner();
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Hej", true, 0));
        assert!(inner.force().err().expect("no interviewer turn").contains("No interviewer turn"));
    }

    #[test]
    fn simulated_turns_join_the_current_session() {
        let mut inner = inner();
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Hej och välkommen.", true, 0));
        let simulated = inner.simulated_event(Speaker::Candidate, "  Tack!  ").unwrap();
        assert_eq!(simulated.session_id, "s");
        assert_eq!(simulated.text, "Tack!");
        assert!(simulated.final_segment);
        assert!(inner.simulated_event(Speaker::Interviewer, "   ").is_err());
        inner.on_transcript(&simulated);
        assert_eq!(inner.conversation.turns().len(), 2);
    }

    #[test]
    fn finish_only_clears_the_matching_generation() {
        let mut inner = inner();
        let (first, _) = inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Fråga ett?", true, 0)).unwrap();
        let (second, _) = inner.force().unwrap();
        inner.finish(first.id);
        assert_eq!(inner.active.as_ref().map(|active| active.id), Some(second.id));
        inner.finish(second.id);
        assert!(inner.active.is_none());
    }

    fn serve_sse(chunks: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .unwrap();
            for chunk in chunks {
                let frame = serde_json::json!({ "choices": [{ "delta": { "content": chunk } }] });
                stream.write_all(format!("data: {frame}\n\n").as_bytes()).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                .unwrap();
        });
        base
    }

    fn plan_with_profile() -> GenerationPlan {
        let mut inner = inner();
        inner.profile = Some(Arc::new(
            profile::parse_profile("p", "candidate:\n  name: A\nstories:\n  password_reset_race_condition:\n    action: x\n".into()).unwrap(),
        ));
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem du hittade.", true, 0)).unwrap().0
    }

    fn config(base_url: String) -> HermesConfig {
        HermesConfig {
            base_url,
            api_key: Some("k".into()),
            key_source: "test",
            model: hermes::DEFAULT_MODEL.into(),
            reasoning_effort: "low".into(),
            api_server_enabled: Some(true),
        }
    }

    #[test]
    fn generation_streams_anchors_before_answer_and_checks_grounding() {
        let base = serve_sse(
            ["ANCHORS\n- password ", "reset\n- multiple tabs\n", "ANSWER\nNär jag ", "byggde det...\n", "SOURCES: password_reset_race_condition, invented_story\n"]
                .map(String::from)
                .to_vec(),
        );
        let plan = plan_with_profile();
        let mut events = Vec::new();
        run_generation(&plan, &CancelToken::default(), &config(base), CURL_BIN, |event| events.push(event.clone()));

        assert_eq!(events.first().unwrap().status, SuggestionStatus::Preparing);
        let first_anchor = events.iter().position(|event| !event.anchors.is_empty()).unwrap();
        let first_answer = events.iter().position(|event| !event.answer.is_empty()).unwrap();
        assert!(first_anchor < first_answer, "anchors must arrive before answer text");
        assert_eq!(events[first_anchor].anchors[0], "password reset");
        assert!(events[first_anchor].answer.is_empty());

        let last = events.last().unwrap();
        assert_eq!(last.status, SuggestionStatus::Done);
        assert_eq!(last.anchors, vec!["password reset", "multiple tabs"]);
        assert_eq!(last.answer, "När jag byggde det...");
        assert_eq!(last.sources, vec!["password_reset_race_condition"]);
        assert_eq!(last.unverified_sources, vec!["invented_story"], "ids missing from profile.yaml are flagged");
        let timing = &last.timing;
        assert!(timing.first_anchor_at_ms.unwrap() <= timing.first_answer_at_ms.unwrap());
        assert!(timing.requested_at_ms <= timing.first_anchor_at_ms.unwrap());
        assert!(timing.finished_at_ms.is_some());
    }

    #[test]
    fn generation_reports_skip_and_failure() {
        let plan = plan_with_profile();
        let mut events = Vec::new();
        run_generation(&plan, &CancelToken::default(), &config(serve_sse(vec!["SKIP\n".into()])), CURL_BIN, |event| {
            events.push(event.clone())
        });
        assert_eq!(events.last().unwrap().status, SuggestionStatus::Skipped);

        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", unused.local_addr().unwrap());
        drop(unused);
        let mut events = Vec::new();
        run_generation(&plan, &CancelToken::default(), &config(base), CURL_BIN, |event| events.push(event.clone()));
        let last = events.last().unwrap();
        assert_eq!(last.status, SuggestionStatus::Failed);
        assert!(last.error.as_deref().unwrap().contains("not reachable"));
    }
}
