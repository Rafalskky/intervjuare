//! Interview copilot orchestration: conversation state → context package →
//! one streaming model request → progressive suggestion events (anchors
//! first, then the full answer). Background interview memory and the
//! "Låt oss se" screen flow hang off the same state.
//!
//! Trigger rules (see docs/context-management.md):
//! - an interviewer final that completes the latest interviewer turn starts a
//!   suggestion automatically (unless auto-answer is off or the whole turn is
//!   a backchannel such as "Mm.");
//! - a new interviewer partial that continues the same turn cancels the
//!   in-flight suggestion, because the question was not finished yet;
//! - a newer suggestion always supersedes the older one;
//! - candidate speech never cancels a suggestion (the candidate reads it
//!   while answering), except the explicit "Låt oss se" command, which starts
//!   a screen-aware suggestion and is removed from the dialogue.

use crate::{
    analysis,
    codex_auth::{self, Account, AccountSummary},
    context::{self, AnswerMode, AnswerOptions, ContextPackage, JobContext, UsedStory},
    conversation::{self, Conversation, Turn, Update},
    llm::{self, CancelToken, ModelInfo, ModelRequest, StreamOutcome},
    memory::{self, InterviewMemory},
    profile::{self, Profile, ProfileSummary},
    screen::{self, Screenshot},
    settings::{self, Settings},
    suggestion::{self, ParsedSuggestion},
    voice_command, now_ms, Speaker, TranscriptEvent, CURL_BIN,
};
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};
use tauri::{AppHandle, Emitter, Manager, State};

pub const SUGGESTION_EVENT: &str = "suggestion-event";
pub const MEMORY_EVENT: &str = "memory-event";
pub const VOICE_COMMAND_EVENT: &str = "voice-command";
pub const ANALYSIS_EVENT: &str = "analysis-event";
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
    pub mode: AnswerMode,
    pub alternative: bool,
    pub screen: bool,
    pub model: String,
    pub timing: SuggestionTiming,
}

#[derive(Clone, Debug)]
struct GenerationPlan {
    id: u64,
    package: ContextPackage,
    question: String,
    turn_ended_at_ms: u64,
    forced: bool,
    mode: AnswerMode,
    alternative: bool,
    /// `Some(application id)` when a screenshot must be attached.
    screen: Option<Option<String>>,
    model: String,
    reasoning_effort: String,
    account_email: Option<String>,
    citable_ids: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct MemoryPlan {
    request: ModelRequest,
    absorbed_until_ms: u64,
    absorbed_turns: usize,
    account_email: Option<String>,
}

enum Work {
    Generate(GenerationPlan, Arc<CancelToken>),
    Memory(MemoryPlan, Arc<CancelToken>),
}

struct ActiveGeneration {
    id: u64,
    token: Arc<CancelToken>,
    turn_first_segment: String,
}

#[derive(Clone, Copy, Default)]
struct Regenerate {
    mode: Option<AnswerMode>,
    alternative: bool,
}

struct CopilotInner {
    conversation: Conversation,
    settings: Settings,
    settings_path: PathBuf,
    settings_warning: Option<String>,
    profile: Option<Arc<Profile>>,
    profile_path: PathBuf,
    profile_error: Option<String>,
    active: Option<ActiveGeneration>,
    next_id: u64,
    simulated_segments: u64,
    memory: InterviewMemory,
    memory_job: Option<Arc<CancelToken>>,
    stories_used: Vec<UsedStory>,
    last_sources: Vec<String>,
    command_segments: HashSet<String>,
    screen_application: Option<String>,
    analysis: Option<(u64, Arc<CancelToken>)>,
}

impl CopilotInner {
    fn new(settings_path: PathBuf) -> Self {
        let (settings, settings_warning) = settings::load(&settings_path);
        let profile_path = settings.profile_path.as_ref().map(PathBuf::from).unwrap_or_else(profile::default_profile_path);
        let mut inner = Self {
            conversation: Conversation::default(),
            settings,
            settings_path,
            settings_warning,
            profile: None,
            profile_path: profile_path.clone(),
            profile_error: None,
            active: None,
            next_id: 1,
            simulated_segments: 0,
            memory: InterviewMemory::default(),
            memory_job: None,
            stories_used: Vec::new(),
            last_sources: Vec::new(),
            command_segments: HashSet::new(),
            screen_application: None,
            analysis: None,
        };
        let _ = inner.load_profile(profile_path);
        inner
    }

    fn persist(&mut self) -> Result<(), String> {
        settings::save(&self.settings_path, &self.settings)?;
        self.settings_warning = None;
        Ok(())
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

    /// A new interview: forget everything learned about the previous one.
    fn reset_interview(&mut self, session_id: Option<String>) {
        self.cancel_active();
        if let Some((_, analysis)) = self.analysis.take() {
            analysis.cancel();
        }
        if let Some(job) = self.memory_job.take() {
            job.cancel();
        }
        self.conversation.reset(session_id);
        self.memory = InterviewMemory::default();
        self.stories_used.clear();
        self.last_sources.clear();
        self.command_segments.clear();
    }

    fn on_transcript(&mut self, event: &TranscriptEvent) -> Vec<Work> {
        if self.conversation.session_id() != Some(event.session_id.as_str()) {
            self.reset_interview(Some(event.session_id.clone()));
        }
        if self.command_segments.contains(&event.segment_id) {
            return Vec::new();
        }
        if event.speaker == Speaker::Candidate && event.final_segment && voice_command::detect(&event.text).is_some() {
            self.command_segments.insert(event.segment_id.clone());
            self.conversation.remove_segment(&event.segment_id);
            let application = self.screen_application.clone();
            return self.plan_screen(application, false);
        }
        match self.conversation.apply(event) {
            Update::Ignored | Update::Changed { speaker: Speaker::Candidate, .. } => Vec::new(),
            Update::Changed { speaker: Speaker::Interviewer, segment_id, final_segment: false } => {
                let continues_active_turn = match (&self.active, self.conversation.turn_of_segment(&segment_id)) {
                    (Some(active), Some(turn)) => turn.first_segment_id == active.turn_first_segment,
                    _ => false,
                };
                if continues_active_turn {
                    self.cancel_active();
                }
                Vec::new()
            }
            Update::Changed { speaker: Speaker::Interviewer, segment_id, final_segment: true } => {
                if !self.settings.auto_answer {
                    return Vec::new();
                }
                let Some(turn) = self.conversation.turn_of_segment(&segment_id) else { return Vec::new() };
                let Some((history, current)) = self.conversation.latest_interviewer_turn() else { return Vec::new() };
                if !turn.complete || current.first_segment_id != turn.first_segment_id || conversation::is_backchannel(&current.text) {
                    return Vec::new();
                }
                self.plan_answer(&history, current, false, Regenerate::default(), None)
            }
        }
    }

    fn force(&mut self, regenerate: Regenerate) -> Result<Vec<Work>, String> {
        let (history, current) = self
            .conversation
            .latest_interviewer_turn()
            .ok_or("No interviewer turn yet. Wait for the interviewer, or add a simulated turn.")?;
        Ok(self.plan_answer(&history, current, true, regenerate, None))
    }

    /// Screen-aware suggestion for "Låt oss se" or the Capture screen button.
    fn plan_screen(&mut self, application: Option<String>, forced: bool) -> Vec<Work> {
        let (history, current) = self.conversation.latest_interviewer_turn().unwrap_or_else(|| {
            let turns = self.conversation.turns();
            let now = now_ms();
            (turns, Turn {
                speaker: Speaker::Interviewer,
                text: "(No interviewer question yet: explain what is on screen and how to respond.)".into(),
                complete: true,
                first_segment_id: format!("screen-{now}"),
                started_at_ms: now,
                ended_at_ms: now,
            })
        });
        self.plan_answer(&history, current, forced, Regenerate::default(), Some(application))
    }

    fn plan_answer(&mut self, history: &[Turn], current: Turn, forced: bool, regenerate: Regenerate, screen: Option<Option<String>>) -> Vec<Work> {
        self.cancel_active();
        let id = self.next_id;
        self.next_id += 1;
        let token = Arc::new(CancelToken::default());
        self.active = Some(ActiveGeneration { id, token: token.clone(), turn_first_segment: current.first_segment_id.clone() });
        let mode = regenerate.mode.unwrap_or(self.settings.answer_mode);
        let options = AnswerOptions {
            mode,
            avoid_sources: if regenerate.alternative { self.last_sources.clone() } else { Vec::new() },
            screen: screen.is_some(),
        };
        let memory_text = (!self.memory.text.is_empty()).then_some(self.memory.text.as_str());
        let package = context::build_context(&self.settings.job, self.profile.as_deref(), memory_text, &self.stories_used, history, &current, &options);
        let plan = GenerationPlan {
            id,
            package,
            question: current.text,
            turn_ended_at_ms: current.ended_at_ms,
            forced,
            mode,
            alternative: regenerate.alternative,
            model: if screen.is_some() { self.settings.screen_model().to_string() } else { self.settings.model.clone() },
            screen,
            reasoning_effort: self.settings.reasoning_effort.clone(),
            account_email: self.settings.account_email.clone(),
            citable_ids: self.profile.as_ref().map(|profile| profile.citable_ids.clone()).unwrap_or_default(),
        };
        let mut work = vec![Work::Generate(plan, token)];
        work.extend(self.plan_memory(history));
        work
    }

    /// Background memory update when enough complete turns are unabsorbed.
    fn plan_memory(&mut self, history: &[Turn]) -> Option<Work> {
        if self.memory_job.is_some() {
            return None;
        }
        let turns = memory::turns_to_absorb(history, self.memory.covered_until_ms);
        if turns.len() < memory::MIN_NEW_TURNS {
            return None;
        }
        let ids: Vec<String> = self.profile.as_ref().map(|profile| profile.citable_ids.iter().cloned().collect()).unwrap_or_default();
        let token = Arc::new(CancelToken::default());
        self.memory_job = Some(token.clone());
        Some(Work::Memory(
            MemoryPlan {
                request: ModelRequest {
                    model: self.settings.model.clone(),
                    reasoning_effort: self.settings.reasoning_effort.clone(),
                    instructions: memory::MEMORY_INSTRUCTIONS.to_string(),
                    text: memory::request_text(&self.memory.text, &turns, &ids, self.settings.job.language),
                    image_data_url: None,
                    cache_key: None,
                },
                absorbed_until_ms: turns.last().map_or(0, |turn| turn.ended_at_ms),
                absorbed_turns: turns.len(),
                account_email: self.settings.account_email.clone(),
            },
            token,
        ))
    }

    fn finish(&mut self, final_event: &SuggestionEvent) {
        if self.active.as_ref().is_some_and(|active| active.id == final_event.suggestion_id) {
            self.active = None;
        }
        if final_event.status == SuggestionStatus::Done {
            self.last_sources = final_event.sources.clone();
            for id in &final_event.sources {
                if !self.stories_used.iter().any(|story| &story.id == id) {
                    self.stories_used.push(UsedStory { id: id.clone(), question: final_event.question.clone() });
                }
            }
        }
    }

    /// Returns false when the job was superseded (reset/cleared meanwhile).
    fn finish_memory(&mut self, token: &Arc<CancelToken>, plan: &MemoryPlan, result: &Result<String, String>) -> bool {
        if !self.memory_job.as_ref().is_some_and(|current| Arc::ptr_eq(current, token)) {
            return false;
        }
        self.memory_job = None;
        if let Ok(text) = result {
            self.memory.text = text.clone();
            self.memory.covered_until_ms = plan.absorbed_until_ms;
            self.memory.absorbed_turns += plan.absorbed_turns;
        }
        true
    }

    /// Post-interview report request from everything known about this interview.
    fn plan_analysis(&mut self, report_language: Option<String>) -> Result<(u64, ModelRequest, Arc<CancelToken>, Option<String>), String> {
        if let Some(language) = report_language.filter(|language| matches!(language.as_str(), "pl" | "sv" | "en")) {
            self.settings.report_language = language;
        }
        let request = analysis::build_request(
            &self.settings.job,
            self.profile.as_deref(),
            &self.memory.text,
            &self.stories_used,
            &self.conversation.turns(),
            &self.settings.report_language,
            self.settings.analysis_model(),
            &self.settings.reasoning_effort,
        )?;
        if let Some((_, previous)) = self.analysis.take() {
            previous.cancel();
        }
        let id = self.next_id;
        self.next_id += 1;
        let token = Arc::new(CancelToken::default());
        self.analysis = Some((id, token.clone()));
        Ok((id, request, token, self.settings.account_email.clone()))
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
        Self(Arc::new(Mutex::new(CopilotInner::new(settings::settings_path()))))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, CopilotInner>, String> {
        self.0.lock().map_err(|_| "copilot state poisoned".to_string())
    }

    /// Meeting application chosen for capture; screenshots read its windows.
    pub fn set_screen_application(&self, application_id: Option<String>) {
        if let Ok(mut inner) = self.lock() {
            inner.screen_application = application_id.filter(|id| !id.is_empty());
        }
    }
}

/// Hook from the STT dispatcher: called for every emitted transcript event.
/// Never blocks on the model; work runs on its own threads.
pub fn on_transcript(app: &AppHandle, event: &TranscriptEvent) {
    let copilot = app.state::<Copilot>();
    let work = match copilot.lock() {
        Ok(mut inner) => {
            let is_command = event.speaker == Speaker::Candidate
                && event.final_segment
                && !inner.command_segments.contains(&event.segment_id)
                && voice_command::detect(&event.text).is_some();
            let work = inner.on_transcript(event);
            if is_command {
                let _ = app.emit(VOICE_COMMAND_EVENT, serde_json::json!({ "segmentId": event.segment_id, "command": "lookAtScreen" }));
            }
            work
        }
        Err(_) => Vec::new(),
    };
    dispatch(app, &copilot.0, work);
}

fn dispatch(app: &AppHandle, state: &Arc<Mutex<CopilotInner>>, work: Vec<Work>) -> Option<u64> {
    let mut suggestion_id = None;
    for item in work {
        match item {
            Work::Generate(plan, token) => {
                suggestion_id = Some(plan.id);
                spawn_generation(app.clone(), state.clone(), plan, token);
            }
            Work::Memory(plan, token) => spawn_memory(app.clone(), state.clone(), plan, token),
        }
    }
    suggestion_id
}

fn resolve_account(email: Option<&str>) -> Result<Account, String> {
    codex_auth::select(&codex_auth::discover_accounts(), email, codex_auth::now_secs())
}

fn spawn_generation(app: AppHandle, state: Arc<Mutex<CopilotInner>>, plan: GenerationPlan, token: Arc<CancelToken>) {
    thread::spawn(move || {
        let account = resolve_account(plan.account_email.as_deref());
        let application = plan.screen.clone().flatten();
        let final_event = run_generation(&plan, &token, account, CURL_BIN, &llm::base_url(), || screen::capture(application.as_deref()), |event| {
            let _ = app.emit(SUGGESTION_EVENT, event);
        });
        if let Ok(mut inner) = state.lock() {
            inner.finish(&final_event);
        }
    });
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryEvent {
    text: String,
    absorbed_turns: usize,
    error: Option<String>,
}

fn spawn_memory(app: AppHandle, state: Arc<Mutex<CopilotInner>>, plan: MemoryPlan, token: Arc<CancelToken>) {
    thread::spawn(move || {
        let result = resolve_account(plan.account_email.as_deref())
            .and_then(|account| llm::complete(&account, &plan.request, &token, CURL_BIN, &llm::base_url()))
            .and_then(|output| memory::sanitize(&output).ok_or_else(|| "The model returned an empty memory.".to_string()));
        let Ok(mut inner) = state.lock() else { return };
        if inner.finish_memory(&token, &plan, &result) {
            let event = MemoryEvent { text: inner.memory.text.clone(), absorbed_turns: inner.memory.absorbed_turns, error: result.err() };
            drop(inner);
            let _ = app.emit(MEMORY_EVENT, event);
        }
    });
}

/// Streams one suggestion and reports every visible change through `emit`.
/// Returns the final event.
fn run_generation(
    plan: &GenerationPlan,
    token: &CancelToken,
    account: Result<Account, String>,
    curl_bin: &str,
    base_url: &str,
    capture: impl FnOnce() -> Result<Screenshot, String>,
    mut emit: impl FnMut(&SuggestionEvent),
) -> SuggestionEvent {
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
        mode: plan.mode,
        alternative: plan.alternative,
        screen: plan.screen.is_some(),
        model: plan.model.clone(),
        timing: SuggestionTiming { turn_ended_at_ms: plan.turn_ended_at_ms, requested_at_ms: now_ms(), ..SuggestionTiming::default() },
    };
    emit(&event);

    let fail = |mut event: SuggestionEvent, error: String, emit: &mut dyn FnMut(&SuggestionEvent)| {
        event.status = SuggestionStatus::Failed;
        event.error = Some(error);
        event.timing.finished_at_ms = Some(now_ms());
        emit(&event);
        event
    };
    let account = match account {
        Ok(account) => account,
        Err(error) => return fail(event, error, &mut emit),
    };
    let image = if plan.screen.is_some() {
        match capture() {
            Ok(shot) => Some(shot.data_url),
            Err(error) => return fail(event, format!("Screen assistance is unavailable: {error}"), &mut emit),
        }
    } else {
        None
    };
    let request = ModelRequest {
        model: plan.model.clone(),
        reasoning_effort: plan.reasoning_effort.clone(),
        instructions: plan.package.system.clone(),
        text: plan.package.user.clone(),
        image_data_url: image,
        cache_key: Some(plan.package.cache_key.clone()),
    };

    let mut raw = String::new();
    let result = llm::stream_response(&account, &request, token, curl_bin, base_url, |delta| {
        raw.push_str(delta);
        if apply_parsed(&mut event, suggestion::parse(&raw, false)) {
            event.status = SuggestionStatus::Streaming;
            emit(&event);
        }
    });
    drop(request);

    let parsed = suggestion::parse(&raw, true);
    let (skip, no_verified_example, cited) = (parsed.skip, parsed.no_verified_example, parsed.sources.clone());
    apply_parsed(&mut event, parsed);
    let (sources, unverified): (Vec<String>, Vec<String>) = cited.into_iter().partition(|id| plan.citable_ids.contains(id));
    event.sources = sources;
    event.unverified_sources = unverified;
    event.no_verified_example = no_verified_example;
    event.timing.finished_at_ms = Some(now_ms());
    event.status = match result {
        Ok(StreamOutcome::Cancelled) => SuggestionStatus::Cancelled,
        Ok(StreamOutcome::Completed { .. }) if skip => SuggestionStatus::Skipped,
        Ok(StreamOutcome::Completed { .. }) if event.anchors.is_empty() && event.answer.is_empty() => {
            event.error = Some("The model returned an empty response.".into());
            SuggestionStatus::Failed
        }
        Ok(StreamOutcome::Completed { .. }) => SuggestionStatus::Done,
        Err(error) => {
            event.error = Some(error);
            SuggestionStatus::Failed
        }
    };
    emit(&event);
    event
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnalysisStatus {
    Running,
    Done,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisEvent {
    pub analysis_id: u64,
    pub status: AnalysisStatus,
    pub text: String,
    pub error: Option<String>,
    pub model: String,
}

/// Streams the post-interview report; emits at most every ~150 ms of
/// new text so a long report does not flood the UI.
fn run_analysis(
    id: u64,
    request: &ModelRequest,
    token: &CancelToken,
    account: Result<Account, String>,
    curl_bin: &str,
    base_url: &str,
    mut emit: impl FnMut(&AnalysisEvent),
) -> AnalysisEvent {
    let mut event = AnalysisEvent { analysis_id: id, status: AnalysisStatus::Running, text: String::new(), error: None, model: request.model.clone() };
    emit(&event);
    let account = match account {
        Ok(account) => account,
        Err(error) => {
            event.status = AnalysisStatus::Failed;
            event.error = Some(error);
            emit(&event);
            return event;
        }
    };
    let mut last_emit = Instant::now();
    let result = llm::stream_response(&account, request, token, curl_bin, base_url, |delta| {
        event.text.push_str(delta);
        if last_emit.elapsed().as_millis() >= 150 {
            last_emit = Instant::now();
            emit(&event);
        }
    });
    event.status = match result {
        Ok(StreamOutcome::Completed { .. }) if event.text.trim().is_empty() => {
            event.error = Some("The model returned an empty report.".into());
            AnalysisStatus::Failed
        }
        Ok(StreamOutcome::Completed { .. }) => AnalysisStatus::Done,
        Ok(StreamOutcome::Cancelled) => AnalysisStatus::Cancelled,
        Err(error) => {
            event.error = Some(error);
            AnalysisStatus::Failed
        }
    };
    emit(&event);
    event
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStatus {
    pub profile: Option<ProfileSummary>,
    pub profile_error: Option<String>,
    pub profile_path: String,
    pub settings: Settings,
    pub settings_warning: Option<String>,
    pub accounts: Vec<AccountSummary>,
    pub memory: String,
    pub memory_turns: usize,
    pub stories_used: Vec<String>,
}

#[tauri::command]
pub fn copilot_status(copilot: State<'_, Copilot>) -> Result<CopilotStatus, String> {
    let now = codex_auth::now_secs();
    let accounts = codex_auth::discover_accounts().iter().map(|account| account.summary(now)).collect();
    let inner = copilot.lock()?;
    Ok(CopilotStatus {
        profile: inner.profile.as_ref().map(|profile| profile.summary.clone()),
        profile_error: inner.profile_error.clone(),
        profile_path: inner.profile_path.display().to_string(),
        settings: inner.settings.clone(),
        settings_warning: inner.settings_warning.clone(),
        accounts,
        memory: inner.memory.text.clone(),
        memory_turns: inner.memory.absorbed_turns,
        stories_used: inner.stories_used.iter().map(|story| story.id.clone()).collect(),
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
    let summary = inner.load_profile(path.clone())?;
    inner.settings.profile_path = Some(path.display().to_string());
    inner.persist()?;
    Ok(summary)
}

#[tauri::command]
pub fn set_job_context(copilot: State<'_, Copilot>, job: JobContext) -> Result<(), String> {
    let mut inner = copilot.lock()?;
    inner.settings.job = job;
    inner.persist()
}

#[tauri::command]
pub fn set_auto_answer(copilot: State<'_, Copilot>, enabled: bool) -> Result<(), String> {
    let mut inner = copilot.lock()?;
    inner.settings.auto_answer = enabled;
    inner.persist()
}

#[tauri::command]
pub fn set_answer_mode(copilot: State<'_, Copilot>, mode: AnswerMode) -> Result<(), String> {
    let mut inner = copilot.lock()?;
    inner.settings.answer_mode = mode;
    inner.persist()
}

#[tauri::command]
pub fn set_model_settings(
    copilot: State<'_, Copilot>,
    account_email: Option<String>,
    model: String,
    reasoning_effort: String,
    screen_model: String,
) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Choose a model.".into());
    }
    let mut inner = copilot.lock()?;
    inner.settings.account_email = account_email.map(|email| email.trim().to_string()).filter(|email| !email.is_empty());
    inner.settings.model = model.to_string();
    inner.settings.reasoning_effort = reasoning_effort.trim().to_string();
    inner.settings.screen_model = screen_model.trim().to_string();
    inner.persist()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    pub ok: bool,
    pub message: String,
    pub account: Option<AccountSummary>,
    pub models: Vec<ModelInfo>,
    pub latency_ms: u64,
}

/// Proves the chosen sign-in works by listing its models (cheap request).
#[tauri::command]
pub async fn check_connection(copilot: State<'_, Copilot>) -> Result<ConnectionStatus, String> {
    let email = copilot.lock()?.settings.account_email.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let started = Instant::now();
        let now = codex_auth::now_secs();
        let result = resolve_account(email.as_deref()).and_then(|account| {
            let models = llm::list_models(&account, CURL_BIN, &llm::base_url())?;
            Ok((account, models))
        });
        let latency_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok((account, models)) => ConnectionStatus {
                ok: true,
                message: format!("Connected to ChatGPT as {} ({} models available).", account.email, models.len()),
                account: Some(account.summary(now)),
                models,
                latency_ms,
            },
            Err(message) => ConnectionStatus { ok: false, message, account: None, models: Vec::new(), latency_ms },
        }
    })
    .await
    .map_err(|error| format!("connection check failed: {error}"))
}

/// Force / regenerate for the latest interviewer turn, optionally with a
/// one-off mode ("Shorter", "STAR", …) or a different example.
#[tauri::command]
pub fn force_answer(app: AppHandle, copilot: State<'_, Copilot>, mode: Option<AnswerMode>, alternative: Option<bool>) -> Result<u64, String> {
    let work = copilot.lock()?.force(Regenerate { mode, alternative: alternative.unwrap_or(false) })?;
    dispatch(&app, &copilot.0, work).ok_or_else(|| "nothing to generate".into())
}

/// Manual "Capture screen" (same flow as saying "Låt oss se").
#[tauri::command]
pub fn capture_screen_answer(app: AppHandle, copilot: State<'_, Copilot>, application_id: Option<String>) -> Result<u64, String> {
    let work = {
        let mut inner = copilot.lock()?;
        let application = application_id.filter(|id| !id.is_empty()).or_else(|| inner.screen_application.clone());
        inner.plan_screen(application, true)
    };
    dispatch(&app, &copilot.0, work).ok_or_else(|| "nothing to generate".into())
}

/// Test input for working without audio: a typed turn is treated exactly
/// like a final transcript segment from that speaker (including commands).
#[tauri::command]
pub fn submit_test_turn(app: AppHandle, copilot: State<'_, Copilot>, speaker: Speaker, text: String) -> Result<TranscriptEvent, String> {
    let (event, work) = {
        let mut inner = copilot.lock()?;
        let event = inner.simulated_event(speaker, &text)?;
        let work = inner.on_transcript(&event);
        (event, work)
    };
    if speaker == Speaker::Candidate && voice_command::detect(&event.text).is_some() {
        let _ = app.emit(VOICE_COMMAND_EVENT, serde_json::json!({ "segmentId": event.segment_id, "command": "lookAtScreen" }));
    }
    dispatch(&app, &copilot.0, work);
    Ok(event)
}

/// Starts the post-interview report for the current conversation.
#[tauri::command]
pub fn analyze_interview(app: AppHandle, copilot: State<'_, Copilot>, report_language: Option<String>) -> Result<u64, String> {
    let (id, request, token, email) = {
        let mut inner = copilot.lock()?;
        let planned = inner.plan_analysis(report_language)?;
        let _ = inner.persist();
        planned
    };
    let state = copilot.0.clone();
    thread::spawn(move || {
        let account = resolve_account(email.as_deref());
        run_analysis(id, &request, &token, account, CURL_BIN, &llm::base_url(), |event| {
            let _ = app.emit(ANALYSIS_EVENT, event);
        });
        if let Ok(mut inner) = state.lock() {
            if inner.analysis.as_ref().is_some_and(|(current, _)| *current == id) {
                inner.analysis = None;
            }
        }
    });
    Ok(id)
}

#[tauri::command]
pub fn cancel_analysis(copilot: State<'_, Copilot>) -> Result<(), String> {
    if let Some((_, token)) = copilot.lock()?.analysis.take() {
        token.cancel();
    }
    Ok(())
}

/// Saves a finished report as Markdown in the local reports folder. Only
/// on explicit owner action; returns the file path.
#[tauri::command]
pub fn save_analysis_report(copilot: State<'_, Copilot>, text: String) -> Result<String, String> {
    let (dir, filename) = {
        let inner = copilot.lock()?;
        (analysis::reports_dir(&inner.settings_path), analysis::report_filename(&inner.settings.job.company, now_ms()))
    };
    analysis::save_report(&dir, &filename, &text).map(|path| path.display().to_string())
}

#[tauri::command]
pub fn set_report_settings(copilot: State<'_, Copilot>, report_language: String, analysis_model: String) -> Result<(), String> {
    if !matches!(report_language.as_str(), "pl" | "sv" | "en") {
        return Err("Report language must be pl, sv or en.".into());
    }
    let mut inner = copilot.lock()?;
    inner.settings.report_language = report_language;
    inner.settings.analysis_model = analysis_model.trim().to_string();
    inner.persist()
}

#[tauri::command]
pub fn clear_conversation(copilot: State<'_, Copilot>) -> Result<(), String> {
    copilot.lock()?.reset_interview(None);
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
    use crate::llm::test_server::{completed, delta, serve_once, unused_base, SSE_HEAD};

    fn inner() -> (CopilotInner, crate::InferenceWorkspace) {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let mut inner = CopilotInner::new(workspace.path.join("settings.json"));
        let _ = inner.load_profile(PathBuf::from("/nonexistent/profile.yaml"));
        (inner, workspace)
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

    fn generation(work: Vec<Work>) -> (GenerationPlan, Arc<CancelToken>) {
        work.into_iter()
            .find_map(|item| match item {
                Work::Generate(plan, token) => Some((plan, token)),
                Work::Memory(..) => None,
            })
            .expect("a generation")
    }

    fn has_memory(work: &[Work]) -> bool {
        work.iter().any(|item| matches!(item, Work::Memory(..)))
    }

    #[test]
    fn missing_profile_is_reported_not_fatal() {
        let (inner, _workspace) = inner();
        assert!(inner.profile.is_none());
        assert!(inner.profile_error.as_deref().unwrap().contains("Could not open"));
    }

    #[test]
    fn interviewer_final_triggers_and_partials_do_not() {
        let (mut inner, _workspace) = inner();
        assert!(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Kan du berätta", false, 0)).is_empty());
        let (plan, _) = generation(inner.on_transcript(&event("interviewer-1", 2, Speaker::Interviewer, "Kan du berätta om dig själv?", true, 0)));
        assert_eq!(plan.question, "Kan du berätta om dig själv?");
        assert_eq!(plan.turn_ended_at_ms, 900);
        assert_eq!(plan.model, "gpt-6-luna");
        assert!(plan.screen.is_none());
        assert!(plan.package.user.contains("CURRENT INTERVIEWER TURN\nKan du berätta om dig själv?"));
        assert!(inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Ja, absolut.", true, 2_000)).is_empty());
    }

    #[test]
    fn continuing_the_same_turn_cancels_and_retriggers_with_the_full_question() {
        let (mut inner, _workspace) = inner();
        let (_, first_token) = generation(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem.", true, 0)));
        assert!(inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Som du", false, 1_500)).is_empty());
        assert!(first_token.is_cancelled(), "the unfinished question's answer is cancelled");
        let (plan, _) = generation(inner.on_transcript(&event("interviewer-2", 2, Speaker::Interviewer, "Som du själv upptäckte.", true, 1_500)));
        assert_eq!(plan.question, "Berätta om ett problem. Som du själv upptäckte.");
    }

    #[test]
    fn a_new_interviewer_turn_after_the_candidate_does_not_cancel_until_it_is_final() {
        let (mut inner, _workspace) = inner();
        let (_, token) = generation(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om Sagobo.", true, 0)));
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Sagobo är en app.", false, 2_000));
        inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Hur", false, 5_000));
        assert!(!token.is_cancelled(), "candidate is reading the suggestion while answering");
        let (plan, _) = generation(inner.on_transcript(&event("interviewer-2", 2, Speaker::Interviewer, "Hur testade du det?", true, 5_000)));
        assert!(token.is_cancelled(), "newer question supersedes");
        assert!(plan.package.user.contains("INTERVIEWER: Berätta om Sagobo.\nCANDIDATE: Sagobo är en app."));
    }

    #[test]
    fn backchannels_and_disabled_auto_answer_do_not_trigger_but_force_does() {
        let (mut inner, _workspace) = inner();
        assert!(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Mm.", true, 0)).is_empty());
        let (plan, _) = generation(inner.force(Regenerate::default()).unwrap());
        assert!(plan.forced);
        assert_eq!(plan.question, "Mm.");

        inner.settings.auto_answer = false;
        assert!(inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Vad är SSR?", true, 3_000)).is_empty());
        assert!(inner.force(Regenerate::default()).is_ok());
    }

    #[test]
    fn force_without_interviewer_turn_is_an_error() {
        let (mut inner, _workspace) = inner();
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Hej", true, 0));
        assert!(inner.force(Regenerate::default()).err().expect("error").contains("No interviewer turn"));
    }

    #[test]
    fn regenerate_applies_one_off_mode_and_alternative_example() {
        let (mut inner, _workspace) = inner();
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem.", true, 0));
        inner.last_sources = vec!["password_reset_race_condition".into()];
        let (plan, _) = generation(inner.force(Regenerate { mode: Some(AnswerMode::Star), alternative: true }).unwrap());
        assert_eq!(plan.mode, AnswerMode::Star);
        assert!(plan.alternative);
        assert!(plan.package.user.contains("Mode: STAR"));
        assert!(plan.package.user.contains("do not rely on password_reset_race_condition"));
        assert_eq!(inner.settings.answer_mode, AnswerMode::Normal, "one-off mode does not change the default");
    }

    #[test]
    fn finished_suggestions_record_stories_used_for_later_prompts() {
        let (mut inner, _workspace) = inner();
        let (plan, _) = generation(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem.", true, 0)));
        let mut done = SuggestionEvent {
            suggestion_id: plan.id,
            status: SuggestionStatus::Done,
            question: plan.question.clone(),
            anchors: vec!["a".into()],
            answer: "x".into(),
            sources: vec!["analytics_adblock".into()],
            unverified_sources: vec![],
            no_verified_example: false,
            error: None,
            forced: false,
            mode: AnswerMode::Normal,
            alternative: false,
            screen: false,
            model: plan.model.clone(),
            timing: SuggestionTiming::default(),
        };
        inner.finish(&done);
        assert!(inner.active.is_none());
        done.status = SuggestionStatus::Cancelled;
        done.sources = vec!["ignored".into()];
        inner.finish(&done);
        assert_eq!(inner.stories_used.iter().map(|story| story.id.as_str()).collect::<Vec<_>>(), vec!["analytics_adblock"]);
        let (next, _) = generation(inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Ett annat problem?", true, 5_000)));
        assert!(next.package.user.contains("STORIES ALREADY USED"));
        assert!(next.package.user.contains("- analytics_adblock (for: Berätta om ett problem.)"));
    }

    #[test]
    fn memory_update_starts_after_enough_new_turns_and_is_absorbed() {
        let (mut inner, _workspace) = inner();
        let mut work = inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om dig själv.", true, 0));
        assert!(!has_memory(&work), "too few turns yet");
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Jag heter Alex.", true, 2_000));
        inner.on_transcript(&event("interviewer-2", 1, Speaker::Interviewer, "Vad gör du nu?", true, 4_000));
        inner.on_transcript(&event("candidate-2", 1, Speaker::Candidate, "Jag bygger Sagobo.", true, 6_000));
        work = inner.on_transcript(&event("interviewer-3", 1, Speaker::Interviewer, "Hur testar du?", true, 8_000));
        let (plan, token) = work
            .into_iter()
            .find_map(|item| match item {
                Work::Memory(plan, token) => Some((plan, token)),
                Work::Generate(..) => None,
            })
            .expect("memory job with four new turns");
        assert_eq!(plan.absorbed_turns, 4);
        assert!(plan.request.text.contains("CANDIDATE: Jag bygger Sagobo."));
        assert!(!plan.request.text.contains("Hur testar du?"), "the current question is not memory yet");
        assert!(!has_memory(&inner.force(Regenerate::default()).unwrap()), "one memory job at a time");

        assert!(inner.finish_memory(&token, &plan, &Ok("topics_discussed: [Sagobo]".into())));
        assert_eq!(inner.memory.absorbed_turns, 4);
        assert_eq!(inner.memory.covered_until_ms, 6_900);
        let (next, _) = generation(inner.force(Regenerate::default()).unwrap());
        assert!(next.package.user.contains("INTERVIEW MEMORY\ntopics_discussed: [Sagobo]"));

        inner.reset_interview(None);
        assert!(inner.memory.text.is_empty());
        assert!(!inner.finish_memory(&token, &plan, &Ok("stale".into())), "a reset discards late memory");
    }

    #[test]
    fn lat_oss_se_starts_a_screen_suggestion_and_is_not_dialogue() {
        let (mut inner, _workspace) = inner();
        inner.screen_application = Some("4242".into());
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Vad tror du är fel här?", true, 0));
        assert!(inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Låt oss", false, 2_000)).is_empty());
        let (plan, _) = generation(inner.on_transcript(&event("candidate-1", 2, Speaker::Candidate, "Låt oss se.", true, 2_000)));
        assert_eq!(plan.screen, Some(Some("4242".into())));
        assert_eq!(plan.question, "Vad tror du är fel här?");
        assert!(plan.package.user.contains("SCREEN CONTEXT"));
        assert!(!plan.package.user.contains("CANDIDATE: Låt oss"), "the command never becomes dialogue");
        assert_eq!(inner.conversation.turns().len(), 1);
        assert!(inner.on_transcript(&event("candidate-1", 3, Speaker::Candidate, "Låt oss se.", true, 2_000)).is_empty());

        let (manual, _) = generation(inner.plan_screen(None, true));
        assert_eq!(manual.screen, Some(None));
        assert!(manual.forced);
    }

    #[test]
    fn analysis_uses_the_whole_interview_and_supersedes_older_reports() {
        let (mut inner, _workspace) = inner();
        assert!(inner.plan_analysis(None).err().expect("empty").contains("Nothing to analyse"));
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om dig själv.", true, 0));
        inner.on_transcript(&event("candidate-1", 1, Speaker::Candidate, "Jag heter Alex.", true, 2_000));
        inner.memory.text = "topics_discussed: [intro]".into();
        let (first_id, request, first_token, _) = inner.plan_analysis(Some("sv".into())).unwrap();
        assert_eq!(inner.settings.report_language, "sv");
        assert!(request.text.starts_with("REPORT LANGUAGE\nSwedish"));
        assert!(request.text.contains("CANDIDATE: Jag heter Alex."));
        assert!(request.text.contains("topics_discussed: [intro]"));
        assert_eq!(request.model, "gpt-6-luna");
        let (second_id, _, _, _) = inner.plan_analysis(Some("xx".into())).unwrap();
        assert!(first_token.is_cancelled(), "a new report supersedes the running one");
        assert!(second_id > first_id);
        assert_eq!(inner.settings.report_language, "sv", "invalid language is ignored");
        inner.reset_interview(None);
        assert!(inner.analysis.is_none());
    }

    #[test]
    fn analysis_streams_the_report_and_reports_failures() {
        let (base, _seen) = serve_once(SSE_HEAD, vec![(delta("## Summary\n"), 0), (delta("Bra intervju."), 200), (completed(), 0)]);
        let request = ModelRequest {
            model: "gpt-test".into(),
            reasoning_effort: "none".into(),
            instructions: analysis::ANALYSIS_INSTRUCTIONS.into(),
            text: "TRANSCRIPT\nx".into(),
            image_data_url: None,
            cache_key: None,
        };
        let mut events = Vec::new();
        let last = run_analysis(7, &request, &CancelToken::default(), Ok(account()), CURL_BIN, &base, |event| events.push(event.clone()));
        assert_eq!(events.first().unwrap().status, AnalysisStatus::Running);
        assert_eq!(last.status, AnalysisStatus::Done);
        assert_eq!(last.text, "## Summary\nBra intervju.");
        assert_eq!(&last, events.last().unwrap());

        let last = run_analysis(8, &request, &CancelToken::default(), Err("No ChatGPT/Codex sign-in found".into()), CURL_BIN, &base, |_| {});
        assert_eq!(last.status, AnalysisStatus::Failed);
        assert!(last.error.unwrap().contains("sign-in"));
    }

    #[test]
    fn settings_changes_persist_and_reload() {
        let (mut inner, workspace) = inner();
        inner.settings.account_email = Some("me@example.com".into());
        inner.settings.answer_mode = AnswerMode::Short;
        inner.persist().unwrap();
        let reloaded = CopilotInner::new(workspace.path.join("settings.json"));
        assert_eq!(reloaded.settings.account_email.as_deref(), Some("me@example.com"));
        assert_eq!(reloaded.settings.answer_mode, AnswerMode::Short);
    }

    #[test]
    fn simulated_turns_join_the_current_session() {
        let (mut inner, _workspace) = inner();
        inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Hej och välkommen.", true, 0));
        let simulated = inner.simulated_event(Speaker::Candidate, "  Tack!  ").unwrap();
        assert_eq!(simulated.session_id, "s");
        assert_eq!(simulated.text, "Tack!");
        assert!(inner.simulated_event(Speaker::Interviewer, "   ").is_err());
        inner.on_transcript(&simulated);
        assert_eq!(inner.conversation.turns().len(), 2);
    }

    fn plan_with_profile() -> GenerationPlan {
        let (mut inner, _workspace) = inner();
        inner.profile = Some(Arc::new(
            profile::parse_profile("p", "candidate:\n  name: A\nstories:\n  password_reset_race_condition:\n    action: x\n".into()).unwrap(),
        ));
        generation(inner.on_transcript(&event("interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem du hittade.", true, 0))).0
    }

    fn account() -> Account {
        let json = serde_json::json!({ "tokens": { "access_token": codex_auth::test_token("me@example.com", "plus", u64::MAX / 2_000) } });
        codex_auth::accounts_from_json("test", &json).remove(0)
    }

    fn no_capture() -> Result<Screenshot, String> {
        panic!("no screenshot expected")
    }

    #[test]
    fn generation_streams_anchors_before_answer_and_checks_grounding() {
        let chunks = ["ANCHORS\n- password ", "reset\n- multiple tabs\n", "ANSWER\nNär jag ", "byggde det...\n", "SOURCES: password_reset_race_condition, invented_story\n"];
        let mut stream: Vec<(String, u64)> = chunks.iter().map(|chunk| (delta(chunk), 5)).collect();
        stream.push((completed(), 0));
        let (base, _seen) = serve_once(SSE_HEAD, stream);
        let plan = plan_with_profile();
        let mut events = Vec::new();
        let last = run_generation(&plan, &CancelToken::default(), Ok(account()), CURL_BIN, &base, no_capture, |event| events.push(event.clone()));

        assert_eq!(events.first().unwrap().status, SuggestionStatus::Preparing);
        let first_anchor = events.iter().position(|event| !event.anchors.is_empty()).unwrap();
        let first_answer = events.iter().position(|event| !event.answer.is_empty()).unwrap();
        assert!(first_anchor < first_answer, "anchors must arrive before answer text");
        assert_eq!(events[first_anchor].anchors[0], "password reset");
        assert_eq!(last.status, SuggestionStatus::Done);
        assert_eq!(last.anchors, vec!["password reset", "multiple tabs"]);
        assert_eq!(last.answer, "När jag byggde det...");
        assert_eq!(last.sources, vec!["password_reset_race_condition"]);
        assert_eq!(last.unverified_sources, vec!["invented_story"], "ids missing from profile.yaml are flagged");
        assert!(last.timing.first_anchor_at_ms.unwrap() <= last.timing.first_answer_at_ms.unwrap());
        assert_eq!(&last, events.last().unwrap());
    }

    #[test]
    fn generation_reports_skip_account_and_network_failures() {
        let plan = plan_with_profile();
        let (base, _seen) = serve_once(SSE_HEAD, vec![(delta("SKIP\n"), 0), (completed(), 0)]);
        let last = run_generation(&plan, &CancelToken::default(), Ok(account()), CURL_BIN, &base, no_capture, |_| {});
        assert_eq!(last.status, SuggestionStatus::Skipped);

        let last = run_generation(&plan, &CancelToken::default(), Err("No ChatGPT/Codex sign-in found".into()), CURL_BIN, &base, no_capture, |_| {});
        assert_eq!(last.status, SuggestionStatus::Failed);
        assert!(last.error.unwrap().contains("sign-in"));

        let last = run_generation(&plan, &CancelToken::default(), Ok(account()), CURL_BIN, &unused_base(), no_capture, |_| {});
        assert_eq!(last.status, SuggestionStatus::Failed);
        assert!(last.error.unwrap().contains("Could not reach"));
    }

    #[test]
    fn screen_generation_attaches_the_screenshot_or_explains_why_not() {
        let (mut inner, _workspace) = inner();
        let (plan, _) = generation(inner.plan_screen(Some("1".into()), true));
        let (base, seen) = serve_once(SSE_HEAD, vec![(delta("ANCHORS\n- off-by-one\nANSWER\nLoopen går ett steg för långt.\nSOURCES: none\n"), 0), (completed(), 0)]);
        let shot = || Ok(Screenshot { data_url: "data:image/jpeg;base64,QUJD".into(), width: 1, height: 1 });
        let last = run_generation(&plan, &CancelToken::default(), Ok(account()), CURL_BIN, &base, shot, |_| {});
        assert_eq!(last.status, SuggestionStatus::Done);
        assert!(last.screen);
        let (_, body) = seen.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["input"][0]["content"][1]["image_url"], "data:image/jpeg;base64,QUJD");

        let denied = || Err("Screen & System Audio Recording permission is not granted".to_string());
        let last = run_generation(&plan, &CancelToken::default(), Ok(account()), CURL_BIN, &base, denied, |_| {});
        assert_eq!(last.status, SuggestionStatus::Failed);
        assert!(last.error.unwrap().starts_with("Screen assistance is unavailable"));
    }
}
