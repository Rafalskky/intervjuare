//! Local interview history in SQLite (Milestone 6, part 2; README
//! "Storage" / "Interview History").
//!
//! Stored per interview: job context (company, role, language, …), final
//! transcript segments, finished suggestions, interview memory and the
//! post-interview report. Never stored: raw audio, screenshots, tokens, or a
//! copy of `profile.yaml` (it stays the single source of truth).
//!
//! One writer thread owns the only connection. Callers on the live path
//! only `send` commands on an unbounded channel, which never blocks, so a
//! slow disk can never delay transcription or suggestion streaming. UI reads
//! go through the same thread with a reply channel.
//!
//! Dependency: `rusqlite` with `bundled` (compiles SQLite from source, so the
//! version is known and does not depend on the system library). The
//! MSRV-aware resolver picked 0.40.x for the declared rust-version.

use crate::context::{AnswerMode, JobContext, Language};
use crate::conversation::{Conversation, Turn};
use crate::{Speaker, TranscriptEvent};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

pub const SCHEMA_VERSION: i64 = 1;
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

const SCHEMA_V1: &str = "
CREATE TABLE interviews (
    id               INTEGER PRIMARY KEY,
    key              TEXT NOT NULL UNIQUE,
    started_at_ms    INTEGER NOT NULL,
    last_activity_ms INTEGER NOT NULL,
    company          TEXT NOT NULL DEFAULT '',
    role             TEXT NOT NULL DEFAULT '',
    language         TEXT NOT NULL DEFAULT 'sv',
    job_json         TEXT NOT NULL,
    memory           TEXT NOT NULL DEFAULT '',
    report           TEXT,
    report_language  TEXT,
    report_model     TEXT,
    report_at_ms     INTEGER
);
CREATE TABLE segments (
    interview_id  INTEGER NOT NULL REFERENCES interviews(id) ON DELETE CASCADE,
    segment_id    TEXT NOT NULL,
    speaker       TEXT NOT NULL CHECK (speaker IN ('candidate', 'interviewer')),
    text          TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms   INTEGER NOT NULL,
    simulated     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (interview_id, segment_id)
);
CREATE TABLE suggestions (
    interview_id  INTEGER NOT NULL REFERENCES interviews(id) ON DELETE CASCADE,
    suggestion_id INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    question      TEXT NOT NULL,
    anchors_json  TEXT NOT NULL,
    answer        TEXT NOT NULL,
    sources_json  TEXT NOT NULL,
    mode          TEXT NOT NULL,
    screen        INTEGER NOT NULL DEFAULT 0,
    model         TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (interview_id, suggestion_id)
);
CREATE INDEX segments_by_time ON segments (interview_id, started_at_ms);
CREATE INDEX interviews_by_start ON interviews (started_at_ms DESC);
";

pub fn history_path(settings_path: &Path) -> PathBuf {
    settings_path.parent().unwrap_or_else(|| Path::new(".")).join("history.sqlite")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSegment {
    pub segment_id: String,
    pub speaker: Speaker,
    pub text: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub simulated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSuggestion {
    pub suggestion_id: u64,
    pub created_at_ms: u64,
    pub question: String,
    pub anchors: Vec<String>,
    pub answer: String,
    pub sources: Vec<String>,
    pub mode: AnswerMode,
    pub screen: bool,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterviewSummary {
    pub id: i64,
    pub started_at_ms: u64,
    pub last_activity_ms: u64,
    pub company: String,
    pub role: String,
    pub language: Language,
    pub segment_count: u64,
    pub suggestion_count: u64,
    pub has_report: bool,
    /// Every segment came from "Test without audio".
    pub simulated_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterviewDetail {
    #[serde(flatten)]
    pub summary: InterviewSummary,
    pub job: JobContext,
    pub memory: String,
    pub report: Option<String>,
    pub report_language: Option<String>,
    pub report_model: Option<String>,
    pub report_at_ms: Option<u64>,
    pub segments: Vec<StoredSegment>,
    pub suggestions: Vec<StoredSuggestion>,
}

impl InterviewDetail {
    /// Turns with exactly the live grouping rules (same speaker → one turn).
    pub fn turns(&self) -> Vec<Turn> {
        let mut conversation = Conversation::default();
        for segment in &self.segments {
            conversation.apply(&TranscriptEvent {
                session_id: "history".into(),
                segment_id: segment.segment_id.clone(),
                revision: 1,
                speaker: segment.speaker,
                text: segment.text.clone(),
                final_segment: true,
                started_at_ms: segment.started_at_ms,
                ended_at_ms: segment.ended_at_ms,
                captured_at_ms: segment.ended_at_ms,
                emitted_at_ms: segment.ended_at_ms,
                latency_ms: 0,
            });
        }
        conversation.turns()
    }

    /// Stories suggested during the interview, first use per id.
    pub fn stories_used(&self) -> Vec<crate::context::UsedStory> {
        let mut stories: Vec<crate::context::UsedStory> = Vec::new();
        for suggestion in &self.suggestions {
            for id in &suggestion.sources {
                if !stories.iter().any(|story| &story.id == id) {
                    stories.push(crate::context::UsedStory { id: id.clone(), question: suggestion.question.clone() });
                }
            }
        }
        stories
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Key(String),
    Id(i64),
}

pub struct HistoryStore {
    conn: Connection,
}

fn sql_error(error: rusqlite::Error) -> String {
    format!("history database error: {error}")
}

fn language_code(language: Language) -> &'static str {
    match language {
        Language::Sv => "sv",
        Language::En => "en",
    }
}

fn mode_code(mode: AnswerMode) -> String {
    serde_json::to_value(mode).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_else(|| "normal".into())
}

impl HistoryStore {
    /// Opens (creating owner-only if missing) and migrates the database.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(directory) = path.parent() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(directory)
                    .map_err(|error| format!("could not create the history folder: {error}"))?;
            }
        }
        // Create the file first with 0600: SQLite gives its journal files
        // the permissions of the database file.
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path).map_err(|error| format!("could not create the history database: {error}"))?;
        Self::from_connection(Connection::open(path).map_err(sql_error)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(sql_error)?)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        conn.execute_batch("PRAGMA foreign_keys = ON;").map_err(sql_error)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    fn interview_id(&self, key: &str) -> Result<Option<i64>, String> {
        self.conn
            .query_row("SELECT id FROM interviews WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
            .map_err(sql_error)
    }

    pub fn begin(&self, key: &str, started_at_ms: u64, job: &JobContext) -> Result<(), String> {
        let job_json = serde_json::to_string(job).map_err(|error| error.to_string())?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO interviews (key, started_at_ms, last_activity_ms, company, role, language, job_json)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)",
                params![key, started_at_ms as i64, job.company.trim(), job.role.trim(), language_code(job.language), job_json],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn set_job(&self, key: &str, job: &JobContext) -> Result<(), String> {
        let job_json = serde_json::to_string(job).map_err(|error| error.to_string())?;
        self.conn
            .execute(
                "UPDATE interviews SET company = ?2, role = ?3, language = ?4, job_json = ?5 WHERE key = ?1",
                params![key, job.company.trim(), job.role.trim(), language_code(job.language), job_json],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    /// Finals are immutable, so a repeated segment id is ignored. Returns
    /// false when the interview no longer exists (deleted meanwhile).
    pub fn add_segment(&self, key: &str, segment: &StoredSegment) -> Result<bool, String> {
        let Some(id) = self.interview_id(key)? else { return Ok(false) };
        let speaker = match segment.speaker {
            Speaker::Candidate => "candidate",
            Speaker::Interviewer => "interviewer",
        };
        self.conn
            .execute(
                "INSERT OR IGNORE INTO segments (interview_id, segment_id, speaker, text, started_at_ms, ended_at_ms, simulated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, segment.segment_id, speaker, segment.text, segment.started_at_ms as i64, segment.ended_at_ms as i64, segment.simulated],
            )
            .map_err(sql_error)?;
        self.touch(id, segment.ended_at_ms)?;
        Ok(true)
    }

    pub fn add_suggestion(&self, key: &str, suggestion: &StoredSuggestion) -> Result<bool, String> {
        let Some(id) = self.interview_id(key)? else { return Ok(false) };
        self.conn
            .execute(
                "INSERT OR REPLACE INTO suggestions
                 (interview_id, suggestion_id, created_at_ms, question, anchors_json, answer, sources_json, mode, screen, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id,
                    suggestion.suggestion_id as i64,
                    suggestion.created_at_ms as i64,
                    suggestion.question,
                    serde_json::to_string(&suggestion.anchors).unwrap_or_else(|_| "[]".into()),
                    suggestion.answer,
                    serde_json::to_string(&suggestion.sources).unwrap_or_else(|_| "[]".into()),
                    mode_code(suggestion.mode),
                    suggestion.screen,
                    suggestion.model,
                ],
            )
            .map_err(sql_error)?;
        self.touch(id, suggestion.created_at_ms)?;
        Ok(true)
    }

    pub fn set_memory(&self, key: &str, text: &str) -> Result<(), String> {
        self.conn.execute("UPDATE interviews SET memory = ?2 WHERE key = ?1", params![key, text]).map_err(sql_error)?;
        Ok(())
    }

    pub fn set_report(&self, target: &Target, text: &str, language: &str, model: &str, at_ms: u64) -> Result<(), String> {
        let (column, value): (&str, rusqlite::types::Value) = match target {
            Target::Key(key) => ("key", key.clone().into()),
            Target::Id(id) => ("id", (*id).into()),
        };
        self.conn
            .execute(
                &format!("UPDATE interviews SET report = ?2, report_language = ?3, report_model = ?4, report_at_ms = ?5 WHERE {column} = ?1"),
                params![value, text, language, model, at_ms as i64],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    fn touch(&self, id: i64, at_ms: u64) -> Result<(), String> {
        self.conn
            .execute("UPDATE interviews SET last_activity_ms = MAX(last_activity_ms, ?2) WHERE id = ?1", params![id, at_ms as i64])
            .map_err(sql_error)?;
        Ok(())
    }

    const SUMMARY_SELECT: &'static str = "
        SELECT i.id, i.started_at_ms, i.last_activity_ms, i.company, i.role, i.language,
               (SELECT COUNT(*) FROM segments s WHERE s.interview_id = i.id),
               (SELECT COUNT(*) FROM suggestions g WHERE g.interview_id = i.id),
               i.report IS NOT NULL,
               (SELECT COUNT(*) FROM segments s WHERE s.interview_id = i.id AND s.simulated = 0)
        FROM interviews i";

    fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InterviewSummary> {
        let language: String = row.get(5)?;
        let segment_count: i64 = row.get(6)?;
        let real_segments: i64 = row.get(9)?;
        Ok(InterviewSummary {
            id: row.get(0)?,
            started_at_ms: row.get::<_, i64>(1)? as u64,
            last_activity_ms: row.get::<_, i64>(2)? as u64,
            company: row.get(3)?,
            role: row.get(4)?,
            language: if language == "en" { Language::En } else { Language::Sv },
            segment_count: segment_count as u64,
            suggestion_count: row.get::<_, i64>(7)? as u64,
            has_report: row.get(8)?,
            simulated_only: segment_count > 0 && real_segments == 0,
        })
    }

    /// Newest first.
    pub fn list(&self) -> Result<Vec<InterviewSummary>, String> {
        let mut statement = self.conn.prepare(&format!("{} ORDER BY i.started_at_ms DESC, i.id DESC", Self::SUMMARY_SELECT)).map_err(sql_error)?;
        let rows = statement.query_map([], Self::summary_from_row).map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn get(&self, id: i64) -> Result<Option<InterviewDetail>, String> {
        let Some(summary) = self
            .conn
            .query_row(&format!("{} WHERE i.id = ?1", Self::SUMMARY_SELECT), params![id], Self::summary_from_row)
            .optional()
            .map_err(sql_error)?
        else {
            return Ok(None);
        };
        let (job_json, memory, report, report_language, report_model, report_at_ms): (String, String, Option<String>, Option<String>, Option<String>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT job_json, memory, report, report_language, report_model, report_at_ms FROM interviews WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .map_err(sql_error)?;

        let mut statement = self
            .conn
            .prepare("SELECT segment_id, speaker, text, started_at_ms, ended_at_ms, simulated FROM segments WHERE interview_id = ?1 ORDER BY started_at_ms, rowid")
            .map_err(sql_error)?;
        let segments = statement
            .query_map(params![id], |row| {
                let speaker: String = row.get(1)?;
                Ok(StoredSegment {
                    segment_id: row.get(0)?,
                    speaker: if speaker == "interviewer" { Speaker::Interviewer } else { Speaker::Candidate },
                    text: row.get(2)?,
                    started_at_ms: row.get::<_, i64>(3)? as u64,
                    ended_at_ms: row.get::<_, i64>(4)? as u64,
                    simulated: row.get(5)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;

        let mut statement = self
            .conn
            .prepare(
                "SELECT suggestion_id, created_at_ms, question, anchors_json, answer, sources_json, mode, screen, model
                 FROM suggestions WHERE interview_id = ?1 ORDER BY created_at_ms, suggestion_id",
            )
            .map_err(sql_error)?;
        let suggestions = statement
            .query_map(params![id], |row| {
                let anchors: String = row.get(3)?;
                let sources: String = row.get(5)?;
                let mode: String = row.get(6)?;
                Ok(StoredSuggestion {
                    suggestion_id: row.get::<_, i64>(0)? as u64,
                    created_at_ms: row.get::<_, i64>(1)? as u64,
                    question: row.get(2)?,
                    anchors: serde_json::from_str(&anchors).unwrap_or_default(),
                    answer: row.get(4)?,
                    sources: serde_json::from_str(&sources).unwrap_or_default(),
                    mode: serde_json::from_value(serde_json::Value::String(mode)).unwrap_or_default(),
                    screen: row.get(7)?,
                    model: row.get(8)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;

        Ok(Some(InterviewDetail {
            summary,
            job: serde_json::from_str(&job_json).unwrap_or_default(),
            memory,
            report,
            report_language,
            report_model,
            report_at_ms: report_at_ms.map(|value| value as u64),
            segments,
            suggestions,
        }))
    }

    /// Deletes the interview and (by cascade) all its rows.
    pub fn delete(&self, id: i64) -> Result<bool, String> {
        Ok(self.conn.execute("DELETE FROM interviews WHERE id = ?1", params![id]).map_err(sql_error)? > 0)
    }
}

fn migrate(conn: &Connection) -> Result<(), String> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(sql_error)?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "The history database was written by a newer Intervjuare (schema {version}, this build knows {SCHEMA_VERSION}). It was left untouched."
        ));
    }
    if version < 1 {
        conn.execute_batch(&format!("BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;")).map_err(sql_error)?;
    }
    // Future migrations: `if version < 2 { … PRAGMA user_version = 2; }`.
    Ok(())
}

pub enum Command {
    Begin { key: String, started_at_ms: u64, job: JobContext },
    Job { key: String, job: JobContext },
    Segment { key: String, segment: StoredSegment },
    Suggestion { key: String, suggestion: StoredSuggestion },
    Memory { key: String, text: String },
    Report { target: Target, text: String, language: String, model: String, at_ms: u64 },
    List(mpsc::Sender<Result<Vec<InterviewSummary>, String>>),
    Get(i64, mpsc::Sender<Result<Option<InterviewDetail>, String>>),
    Delete(i64, mpsc::Sender<Result<bool, String>>),
}

/// Handle to the history writer thread. Cheap to clone.
#[derive(Clone)]
pub struct History {
    sender: mpsc::Sender<Command>,
    error: Arc<Mutex<Option<String>>>,
}

impl History {
    pub fn spawn(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<Command>();
        let error = Arc::new(Mutex::new(None));
        let thread_error = error.clone();
        thread::Builder::new()
            .name("intervjuare-history".into())
            .spawn(move || run_writer(&path, &receiver, &thread_error))
            .expect("spawn history writer");
        Self { sender, error }
    }

    /// Non-blocking; safe to call from the live path.
    pub fn send(&self, command: Command) {
        if self.sender.send(command).is_err() {
            self.set_error("The history writer stopped; interviews are not being saved.".into());
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }

    fn set_error(&self, message: String) {
        if let Ok(mut error) = self.error.lock() {
            *error = Some(message);
        }
    }

    fn request<T>(&self, make: impl FnOnce(mpsc::Sender<Result<T, String>>) -> Command) -> Result<T, String> {
        let (reply, response) = mpsc::channel();
        self.sender.send(make(reply)).map_err(|_| "The history writer is not running.".to_string())?;
        response.recv_timeout(REPLY_TIMEOUT).map_err(|_| "The history database did not answer in time.".to_string())?
    }

    pub fn list(&self) -> Result<Vec<InterviewSummary>, String> {
        self.request(Command::List)
    }

    pub fn get(&self, id: i64) -> Result<Option<InterviewDetail>, String> {
        self.request(|reply| Command::Get(id, reply))
    }

    pub fn delete(&self, id: i64) -> Result<bool, String> {
        self.request(|reply| Command::Delete(id, reply))
    }
}

fn run_writer(path: &Path, receiver: &mpsc::Receiver<Command>, error: &Mutex<Option<String>>) {
    let record = |message: String| {
        if let Ok(mut slot) = error.lock() {
            *slot = Some(message);
        }
    };
    let store = match HistoryStore::open(path) {
        Ok(store) => Some(store),
        Err(message) => {
            record(message);
            None
        }
    };
    for command in receiver {
        let Some(store) = &store else {
            let unavailable = || "History is unavailable: the database could not be opened.".to_string();
            match command {
                Command::List(reply) => drop(reply.send(Err(unavailable()))),
                Command::Get(_, reply) => drop(reply.send(Err(unavailable()))),
                Command::Delete(_, reply) => drop(reply.send(Err(unavailable()))),
                _ => {}
            }
            continue;
        };
        let result = match command {
            Command::Begin { key, started_at_ms, job } => store.begin(&key, started_at_ms, &job),
            Command::Job { key, job } => store.set_job(&key, &job),
            Command::Segment { key, segment } => store.add_segment(&key, &segment).map(drop),
            Command::Suggestion { key, suggestion } => store.add_suggestion(&key, &suggestion).map(drop),
            Command::Memory { key, text } => store.set_memory(&key, &text),
            Command::Report { target, text, language, model, at_ms } => store.set_report(&target, &text, &language, &model, at_ms),
            Command::List(reply) => {
                let _ = reply.send(store.list());
                Ok(())
            }
            Command::Get(id, reply) => {
                let _ = reply.send(store.get(id));
                Ok(())
            }
            Command::Delete(id, reply) => {
                let _ = reply.send(store.delete(id));
                Ok(())
            }
        };
        if let Err(message) = result {
            record(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> JobContext {
        JobContext { company: "Example AB".into(), role: "Backend".into(), language: Language::Sv, important_topics: vec!["APIs".into()], ..JobContext::default() }
    }

    fn segment(id: &str, speaker: Speaker, text: &str, started: u64, simulated: bool) -> StoredSegment {
        StoredSegment { segment_id: id.into(), speaker, text: text.into(), started_at_ms: started, ended_at_ms: started + 900, simulated }
    }

    fn suggestion(id: u64) -> StoredSuggestion {
        StoredSuggestion {
            suggestion_id: id,
            created_at_ms: 5_000,
            question: "Berätta om ett problem.".into(),
            anchors: vec!["password reset".into(), "Jest".into()],
            answer: "När jag…".into(),
            sources: vec!["password_reset_race_condition".into()],
            mode: AnswerMode::Star,
            screen: false,
            model: "gpt-6-luna".into(),
        }
    }

    #[test]
    fn round_trip_keeps_everything_needed_for_review_and_analysis() {
        let store = HistoryStore::open_in_memory().unwrap();
        store.begin("k1", 1_000, &job()).unwrap();
        store.begin("k1", 9_999, &JobContext::default()).unwrap();
        assert!(store.add_segment("k1", &segment("interviewer-1", Speaker::Interviewer, "Berätta om ett problem.", 1_000, false)).unwrap());
        assert!(store.add_segment("k1", &segment("interviewer-2", Speaker::Interviewer, "Som du hittade.", 2_000, false)).unwrap());
        assert!(store.add_segment("k1", &segment("candidate-1", Speaker::Candidate, "Ja, i Sagobo…", 4_000, false)).unwrap());
        store.add_segment("k1", &segment("candidate-1", Speaker::Candidate, "mutated", 4_000, false)).unwrap();
        assert!(store.add_suggestion("k1", &suggestion(3)).unwrap());
        store.set_memory("k1", "topics_discussed: [Sagobo]").unwrap();
        store.set_report(&Target::Key("k1".into()), "## Sammanfattning", "pl", "gpt-6-luna", 9_000).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        let summary = &list[0];
        assert_eq!((summary.company.as_str(), summary.role.as_str(), summary.language), ("Example AB", "Backend", Language::Sv));
        assert_eq!((summary.segment_count, summary.suggestion_count, summary.has_report, summary.simulated_only), (3, 1, true, false));
        assert_eq!(summary.started_at_ms, 1_000, "a repeated begin does not reset the interview");
        assert_eq!(summary.last_activity_ms, 5_000);

        let detail = store.get(summary.id).unwrap().unwrap();
        assert_eq!(detail.job, job());
        assert_eq!(detail.segments[2].text, "Ja, i Sagobo…", "finals are immutable");
        assert_eq!(detail.suggestions, vec![suggestion(3)]);
        assert_eq!(detail.memory, "topics_discussed: [Sagobo]");
        assert_eq!(detail.report.as_deref(), Some("## Sammanfattning"));
        assert_eq!(detail.report_language.as_deref(), Some("pl"));
        let turns = detail.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "Berätta om ett problem. Som du hittade.");
        assert_eq!(detail.stories_used()[0].id, "password_reset_race_condition");
        assert!(store.get(summary.id + 100).unwrap().is_none());
    }

    #[test]
    fn delete_cascades_and_later_writes_for_it_are_dropped() {
        let store = HistoryStore::open_in_memory().unwrap();
        store.begin("k1", 1_000, &job()).unwrap();
        store.add_segment("k1", &segment("c-1", Speaker::Candidate, "Hej", 1_000, true)).unwrap();
        store.add_suggestion("k1", &suggestion(1)).unwrap();
        let id = store.list().unwrap()[0].id;
        assert!(store.list().unwrap()[0].simulated_only);
        assert!(store.delete(id).unwrap());
        assert!(!store.delete(id).unwrap());
        assert!(store.list().unwrap().is_empty());
        let orphans: i64 = store.conn.query_row("SELECT (SELECT COUNT(*) FROM segments) + (SELECT COUNT(*) FROM suggestions)", [], |row| row.get(0)).unwrap();
        assert_eq!(orphans, 0);
        assert!(!store.add_segment("k1", &segment("c-2", Speaker::Candidate, "Igen", 2_000, false)).unwrap());
        assert!(store.list().unwrap().is_empty(), "a deleted interview is not recreated");
    }

    #[test]
    fn schema_is_versioned_and_newer_databases_are_left_untouched() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let path = workspace.path.join("nested/history.sqlite");
        {
            let store = HistoryStore::open(&path).unwrap();
            store.begin("k1", 1, &job()).unwrap();
            let version: i64 = store.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
            assert_eq!(version, SCHEMA_VERSION);
        }
        assert_eq!(HistoryStore::open(&path).unwrap().list().unwrap().len(), 1, "reopening keeps data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 99;").unwrap();
        let error = HistoryStore::open(&path).err().expect("newer schema");
        assert!(error.contains("newer Intervjuare"), "{error}");
    }

    #[test]
    fn writer_thread_applies_commands_in_order_and_answers_reads() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let history = History::spawn(workspace.path.join("history.sqlite"));
        history.send(Command::Begin { key: "k".into(), started_at_ms: 10, job: job() });
        history.send(Command::Segment { key: "k".into(), segment: segment("i-1", Speaker::Interviewer, "Fråga?", 10, false) });
        history.send(Command::Report { target: Target::Key("k".into()), text: "r".into(), language: "sv".into(), model: "m".into(), at_ms: 20 });
        let list = history.list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].has_report);
        assert!(history.delete(list[0].id).unwrap());
        assert!(history.list().unwrap().is_empty());
        assert!(history.last_error().is_none());
    }

    #[test]
    fn unopenable_database_reports_errors_instead_of_failing_silently() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let path = workspace.path.join("history.sqlite");
        fs::create_dir(&path).unwrap();
        let history = History::spawn(path);
        assert!(history.list().unwrap_err().contains("unavailable"));
        assert!(history.last_error().unwrap().contains("history database"));
    }
}
