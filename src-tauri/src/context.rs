//! Context package sent to the model for one interviewer turn.
//!
//! Order follows README "Context Sent to the Model":
//! system rules, job context, candidate profile, interview memory, recent
//! conversation, current interviewer turn, optional screen context, task.
//! The static part (rules, job, profile) comes first so the provider's
//! prompt cache can reuse it across turns.

use crate::conversation::Turn;
use crate::profile::Profile;
use crate::Speaker;
use serde::{Deserialize, Serialize};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Recent-conversation budget. Older turns belong in interview memory.
pub const RECENT_TURNS_MAX: usize = 12;
pub const RECENT_CHARS_MAX: usize = 4_000;
pub const JOB_DESCRIPTION_CHARS_MAX: usize = 6_000;
pub const MEMORY_CHARS_MAX: usize = 4_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Sv,
    En,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobContext {
    pub company: String,
    pub role: String,
    pub job_description: String,
    pub important_topics: Vec<String>,
    pub language: Language,
}

/// README "Answer Modes" plus "Explain the question".
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AnswerMode {
    #[default]
    Normal,
    Short,
    Star,
    Technical,
    Simple,
    Explain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsedStory {
    pub id: String,
    pub question: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnswerOptions {
    pub mode: AnswerMode,
    /// "Alternative example": profile ids the answer must not rely on.
    pub avoid_sources: Vec<String>,
    /// A screenshot is attached to this request ("Låt oss se").
    pub screen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPackage {
    pub system: String,
    pub user: String,
    /// Stable for an unchanged job + profile, so the prompt cache is reused.
    pub cache_key: String,
}

pub const SYSTEM_RULES: &str = "\
You are Intervjuare, a real-time interview copilot. The candidate is in a live job interview and will glance at your output while speaking. You write what the candidate can say out loud, in first person, in their own voice.

Truthfulness (most important):
- Personal claims (employment, projects, responsibilities, technologies used, achievements, education, results) must be grounded in CANDIDATE PROFILE. Never invent or embellish experience.
- The same real experience may be framed with a different angle for the role in JOB CONTEXT; the facts never change.
- If the question needs a personal example and the profile has none that fits, do not fabricate one. Help the candidate give an honest answer (explain the concept, how they would approach it, related real experience) and add the line NO_VERIFIED_EXAMPLE.
- Technical or conceptual questions (\"What is SSR?\") may be answered from general knowledge.
- Stay consistent with what the candidate already said (INTERVIEW MEMORY, RECENT CONVERSATION).

Context:
- Understand what the interviewer wants from the whole conversation, not just the last sentence. Resolve references such as \"det\", \"den lösningen\", \"då\", \"that\", \"it\" using RECENT CONVERSATION and INTERVIEW MEMORY.
- Prefer experience that matches the role and important topics in JOB CONTEXT.
- Prefer an example that has not been used yet (STORIES ALREADY USED), unless the question follows up on that example.
- When SCREEN CONTEXT is attached, read the screenshot (code, diagram, error, task or question on screen) and answer about what is shown, in the context of the interview.

Language and style:
- Write anchors and answer in the interview language given in TASK.
- Spoken language: short sentences, natural and professional, easy to pronounce. Swedish should sound like natural spoken Swedish (\"Jag märkte att...\"), not formal written Swedish.

Output format (plain text, no markdown, no preamble). Start immediately with ANCHORS:
ANCHORS
- 3 to 5 speaking anchors, each 2 to 6 words, in the order the candidate should say them
ANSWER
The full spoken answer, about 20 to 40 seconds when read aloud, in short paragraphs, unless TASK asks for another length or structure.
NO_VERIFIED_EXAMPLE   (this line only when the question asked for personal experience the profile does not contain)
SOURCES: comma-separated ids from CANDIDATE PROFILE (keys under projects/stories) the answer relies on, or none

If the interviewer's latest turn does not expect an answer from the candidate (filler, acknowledgement, the interviewer clearly still mid-thought, or talking to someone else), output only the single line:
SKIP";

pub fn build_context(
    job: &JobContext,
    profile: Option<&Profile>,
    memory: Option<&str>,
    stories_used: &[UsedStory],
    history: &[Turn],
    current: &Turn,
    options: &AnswerOptions,
) -> ContextPackage {
    let mut user = String::new();

    user.push_str("JOB CONTEXT\n");
    user.push_str(&format_job(job));

    user.push_str("\n\nCANDIDATE PROFILE\n");
    match profile {
        Some(profile) => user.push_str(profile.raw.trim_end()),
        None => user.push_str(
            "(No candidate profile loaded. Make no personal claims; for personal questions add NO_VERIFIED_EXAMPLE.)",
        ),
    }
    let static_prefix_len = user.len();

    if let Some(memory) = memory.map(str::trim).filter(|memory| !memory.is_empty()) {
        user.push_str("\n\nINTERVIEW MEMORY\n");
        user.push_str(&truncate_chars(memory, MEMORY_CHARS_MAX));
    }

    if !stories_used.is_empty() {
        user.push_str("\n\nSTORIES ALREADY USED (suggested earlier in this interview)\n");
        let lines: Vec<String> = stories_used.iter().map(|story| format!("- {} (for: {})", story.id, one_line(&story.question, 120))).collect();
        user.push_str(&lines.join("\n"));
    }

    user.push_str("\n\nRECENT CONVERSATION\n");
    let recent = format_recent(history);
    if recent.is_empty() {
        user.push_str("(This is the start of the conversation.)");
    } else {
        user.push_str(&recent);
    }

    user.push_str("\n\nCURRENT INTERVIEWER TURN\n");
    user.push_str(current.text.trim());

    if options.screen {
        user.push_str("\n\nSCREEN CONTEXT\nThe candidate said \"Låt oss se\" / \"Let's see\" and a screenshot of the meeting is attached. Use what is shown on screen together with the conversation.");
    }

    user.push_str("\n\nTASK\n");
    user.push_str(match job.language {
        Language::Sv => "Interview language: Swedish (sv). Help the candidate answer the current interviewer turn.",
        Language::En => "Interview language: English (en). Help the candidate answer the current interviewer turn.",
    });
    if let Some(instruction) = mode_instruction(options.mode, job.language) {
        user.push('\n');
        user.push_str(instruction);
    }
    if !options.avoid_sources.is_empty() {
        user.push_str(&format!(
            "\nThe candidate asked for an alternative example: do not rely on {}. If no other fitting example exists in the profile, say so honestly.",
            options.avoid_sources.join(", ")
        ));
    }

    let mut hasher = DefaultHasher::new();
    SYSTEM_RULES.hash(&mut hasher);
    user[..static_prefix_len].hash(&mut hasher);
    ContextPackage {
        system: SYSTEM_RULES.to_string(),
        user,
        cache_key: format!("intervjuare-{:016x}", hasher.finish()),
    }
}

fn mode_instruction(mode: AnswerMode, language: Language) -> Option<&'static str> {
    Some(match mode {
        AnswerMode::Normal => return None,
        AnswerMode::Short => "Mode: SHORT. The answer is one or two spoken sentences; 2 to 3 anchors.",
        AnswerMode::Star => "Mode: STAR. Structure the answer as situation, task, action and result, without saying those labels out loud. One anchor per step.",
        AnswerMode::Technical => "Mode: TECHNICAL. Go deeper: concrete mechanisms, trade-offs and how it was or would be verified. Keep it speakable.",
        AnswerMode::Simple => match language {
            Language::Sv => "Mode: SIMPLE SWEDISH. Very short sentences and common words, still professional. Avoid long compound words where a simpler phrase exists.",
            Language::En => "Mode: SIMPLE ENGLISH. Very short sentences and common words, still professional.",
        },
        AnswerMode::Explain => "Mode: EXPLAIN THE QUESTION. Do not answer it yet. ANCHORS: what the interviewer is really asking for. ANSWER: in simple words, what they want to hear and how the candidate could approach it, then one natural opening sentence. SOURCES as usual.",
    })
}

fn format_job(job: &JobContext) -> String {
    let mut lines = Vec::new();
    let field = |label: &str, value: &str| (!value.trim().is_empty()).then(|| format!("{label}: {}", value.trim()));
    lines.extend(field("Company", &job.company));
    lines.extend(field("Role", &job.role));
    let topics: Vec<&str> = job.important_topics.iter().map(|topic| topic.trim()).filter(|topic| !topic.is_empty()).collect();
    if !topics.is_empty() {
        lines.push(format!("Important topics: {}", topics.join(", ")));
    }
    let description = job.job_description.trim();
    if !description.is_empty() {
        lines.push(format!("Job description:\n{}", truncate_chars(description, JOB_DESCRIPTION_CHARS_MAX)));
    }
    if lines.is_empty() {
        "(No job context given.)".to_string()
    } else {
        lines.join("\n")
    }
}

/// Job context plus the interview language, for the post-interview report.
pub fn format_job_for_analysis(job: &JobContext) -> String {
    let language = match job.language {
        Language::Sv => "sv",
        Language::En => "en",
    };
    format!("{}\nInterview language: {language}", format_job(job))
}

pub fn speaker_label(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Interviewer => "INTERVIEWER",
        Speaker::Candidate => "CANDIDATE",
    }
}

/// Newest turns win the budget; output stays in chronological order.
pub fn format_recent(history: &[Turn]) -> String {
    let mut selected: Vec<String> = Vec::new();
    let mut used = 0;
    for turn in history.iter().rev().take(RECENT_TURNS_MAX) {
        let label = speaker_label(turn.speaker);
        let line = format!("{label}: {}", turn.text.trim());
        let cost = line.chars().count() + 1;
        if used + cost > RECENT_CHARS_MAX {
            if selected.is_empty() {
                selected.push(format!("{label}: …{}", tail_chars(turn.text.trim(), RECENT_CHARS_MAX.saturating_sub(16))));
            }
            break;
        }
        used += cost;
        selected.push(line);
    }
    selected.reverse();
    selected.join("\n")
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max).collect();
    truncated.push_str("\n[…truncated]");
    truncated
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(speaker: Speaker, text: &str) -> Turn {
        Turn {
            speaker,
            text: text.into(),
            complete: true,
            first_segment_id: format!("{text}-id"),
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn job() -> JobContext {
        JobContext {
            company: "Example AB".into(),
            role: "Backend Developer".into(),
            job_description: "Build APIs.".into(),
            important_topics: vec!["APIs".into(), " ".into(), "testing".into()],
            language: Language::Sv,
        }
    }

    fn profile() -> Profile {
        crate::profile::parse_profile("p", "candidate:\n  name: Alex\nstories:\n  s1:\n    action: x\n".into()).unwrap()
    }

    #[test]
    fn package_contains_sections_in_readme_order() {
        let history = vec![
            turn(Speaker::Interviewer, "Berätta om ett problem."),
            turn(Speaker::Candidate, "Jag byggde lösenordsåterställning."),
        ];
        let used = vec![UsedStory { id: "s1".into(), question: "Berätta om ett problem.".into() }];
        let package = build_context(
            &job(),
            Some(&profile()),
            Some("topics_discussed: [testing]"),
            &used,
            &history,
            &turn(Speaker::Interviewer, "Hur testade du det?"),
            &AnswerOptions { screen: true, ..AnswerOptions::default() },
        );
        let order = [
            "JOB CONTEXT",
            "CANDIDATE PROFILE",
            "INTERVIEW MEMORY",
            "STORIES ALREADY USED",
            "RECENT CONVERSATION",
            "CURRENT INTERVIEWER TURN",
            "SCREEN CONTEXT",
            "TASK",
        ];
        let positions: Vec<usize> = order.iter().map(|header| package.user.find(header).expect(header)).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}");
        assert!(package.user.contains("Role: Backend Developer"));
        assert!(package.user.contains("Important topics: APIs, testing"));
        assert!(package.user.contains("- s1 (for: Berätta om ett problem.)"));
        assert!(package.user.contains("INTERVIEWER: Berätta om ett problem.\nCANDIDATE: Jag byggde lösenordsåterställning."));
        assert!(package.user.contains("CURRENT INTERVIEWER TURN\nHur testade du det?"));
        assert!(package.user.contains("Swedish (sv)"));
        assert!(package.system.contains("NO_VERIFIED_EXAMPLE"));
    }

    #[test]
    fn cache_key_depends_only_on_the_static_prefix() {
        let options = AnswerOptions::default();
        let a = build_context(&job(), Some(&profile()), None, &[], &[], &turn(Speaker::Interviewer, "Fråga ett?"), &options);
        let b = build_context(&job(), Some(&profile()), Some("memory"), &[], &[], &turn(Speaker::Interviewer, "Fråga två?"), &options);
        assert_eq!(a.cache_key, b.cache_key, "conversation changes keep the cache");
        let mut other_job = job();
        other_job.role = "Designer".into();
        let c = build_context(&other_job, Some(&profile()), None, &[], &[], &turn(Speaker::Interviewer, "Fråga ett?"), &options);
        assert_ne!(a.cache_key, c.cache_key);
    }

    #[test]
    fn missing_profile_job_and_memory_are_explicit_or_omitted() {
        let package = build_context(
            &JobContext { language: Language::En, ..JobContext::default() },
            None,
            Some("  "),
            &[],
            &[],
            &turn(Speaker::Interviewer, "What is SSR?"),
            &AnswerOptions::default(),
        );
        assert!(package.user.contains("(No job context given.)"));
        assert!(package.user.contains("No candidate profile loaded"));
        assert!(package.user.contains("(This is the start of the conversation.)"));
        assert!(!package.user.contains("INTERVIEW MEMORY"));
        assert!(!package.user.contains("STORIES ALREADY USED"));
        assert!(!package.user.contains("SCREEN CONTEXT"));
        assert!(!package.user.contains("Mode:"));
        assert!(package.user.contains("English (en)"));
    }

    #[test]
    fn modes_and_alternative_example_shape_the_task() {
        let current = turn(Speaker::Interviewer, "Berätta om ett problem.");
        let build = |options: AnswerOptions, language: Language| {
            build_context(&JobContext { language, ..job() }, Some(&profile()), None, &[], &[], &current, &options).user
        };
        assert!(build(AnswerOptions { mode: AnswerMode::Short, ..Default::default() }, Language::Sv).contains("Mode: SHORT"));
        assert!(build(AnswerOptions { mode: AnswerMode::Star, ..Default::default() }, Language::Sv).contains("Mode: STAR"));
        assert!(build(AnswerOptions { mode: AnswerMode::Simple, ..Default::default() }, Language::Sv).contains("SIMPLE SWEDISH"));
        assert!(build(AnswerOptions { mode: AnswerMode::Simple, ..Default::default() }, Language::En).contains("SIMPLE ENGLISH"));
        assert!(build(AnswerOptions { mode: AnswerMode::Explain, ..Default::default() }, Language::Sv).contains("Do not answer it yet"));
        let alternative = build(AnswerOptions { avoid_sources: vec!["s1".into()], ..Default::default() }, Language::Sv);
        assert!(alternative.contains("alternative example: do not rely on s1"));
    }

    #[test]
    fn recent_conversation_keeps_newest_turns_within_budget() {
        let history: Vec<Turn> = (0..30)
            .map(|index| turn(if index % 2 == 0 { Speaker::Interviewer } else { Speaker::Candidate }, &format!("turn {index}")))
            .collect();
        let recent = format_recent(&history);
        assert_eq!(recent.lines().count(), RECENT_TURNS_MAX);
        assert!(recent.starts_with("INTERVIEWER: turn 18"));
        assert!(recent.ends_with("CANDIDATE: turn 29"));

        let long = vec![turn(Speaker::Candidate, &"ord ".repeat(2_000))];
        let recent = format_recent(&long);
        assert!(recent.chars().count() <= RECENT_CHARS_MAX);
        assert!(recent.starts_with("CANDIDATE: …"));
    }

    #[test]
    fn long_inputs_are_truncated() {
        let mut long_job = job();
        long_job.job_description = "x".repeat(JOB_DESCRIPTION_CHARS_MAX + 50);
        assert!(format_job(&long_job).contains("[…truncated]"));
        let package = build_context(&job(), None, Some(&"m".repeat(MEMORY_CHARS_MAX + 10)), &[], &[], &turn(Speaker::Interviewer, "?"), &AnswerOptions::default());
        assert!(package.user.contains("[…truncated]"));
    }

    #[test]
    fn settings_shapes_deserialize_from_the_ui() {
        let parsed: JobContext = serde_json::from_str(r#"{"company":"A","role":"R","importantTopics":["x"],"language":"en"}"#).unwrap();
        assert_eq!(parsed.language, Language::En);
        assert_eq!(parsed.important_topics, vec!["x"]);
        assert_eq!(parsed.job_description, "");
        assert_eq!(serde_json::from_str::<AnswerMode>("\"star\"").unwrap(), AnswerMode::Star);
        assert_eq!(serde_json::to_string(&AnswerMode::Simple).unwrap(), "\"simple\"");
    }
}
