//! Context package sent to Hermes for one interviewer turn.
//!
//! Order follows README "Context Sent to Hermes": system rules, job context,
//! candidate profile, recent conversation, current interviewer turn, task.
//! Interview memory (Milestone 4) and screen context (Milestone 5) slot in
//! between profile and recent conversation later.

use crate::conversation::Turn;
use crate::profile::Profile;
use crate::Speaker;
use serde::{Deserialize, Serialize};

/// Recent-conversation budget. Older turns belong in interview memory.
pub const RECENT_TURNS_MAX: usize = 12;
pub const RECENT_CHARS_MAX: usize = 4_000;
pub const JOB_DESCRIPTION_CHARS_MAX: usize = 6_000;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPackage {
    pub system: String,
    pub user: String,
}

pub const SYSTEM_RULES: &str = "\
You are Intervjuare, a real-time interview copilot. The candidate is in a live job interview and will glance at your output while speaking. You write what the candidate can say out loud, in first person, in their own voice.

Truthfulness (most important):
- Personal claims (employment, projects, responsibilities, technologies used, achievements, education, results) must be grounded in CANDIDATE PROFILE. Never invent or embellish experience.
- The same real experience may be framed with a different angle for the role in JOB CONTEXT; the facts never change.
- If the question needs a personal example and the profile has none that fits, do not fabricate one. Help the candidate give an honest answer (explain the concept, how they would approach it, related real experience) and add the line NO_VERIFIED_EXAMPLE.
- Technical or conceptual questions (\"What is SSR?\") may be answered from general knowledge.

Context:
- Understand what the interviewer wants from the whole conversation, not just the last sentence. Resolve references such as \"det\", \"den lösningen\", \"då\", \"that\", \"it\" using RECENT CONVERSATION.
- Prefer experience that matches the role and important topics in JOB CONTEXT.
- Do not use tools or web search; answer immediately from the context given.

Language and style:
- Write anchors and answer in the interview language given in TASK.
- Spoken language: short sentences, natural and professional, easy to pronounce. Swedish should sound like natural spoken Swedish (\"Jag märkte att...\"), not formal written Swedish.

Output format (plain text, no markdown, no preamble). Start immediately with ANCHORS:
ANCHORS
- 3 to 5 speaking anchors, each 2 to 6 words, in the order the candidate should say them
ANSWER
The full spoken answer, about 20 to 40 seconds when read aloud, in short paragraphs.
SOURCES: comma-separated ids from CANDIDATE PROFILE (keys under projects/stories) the answer relies on, or none

If the interviewer's latest turn does not expect an answer from the candidate (filler, acknowledgement, the interviewer clearly still mid-thought, or talking to someone else), output only the single line:
SKIP";

pub fn build_context(job: &JobContext, profile: Option<&Profile>, history: &[Turn], current: &Turn) -> ContextPackage {
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

    user.push_str("\n\nRECENT CONVERSATION\n");
    let recent = format_recent(history);
    if recent.is_empty() {
        user.push_str("(This is the start of the conversation.)");
    } else {
        user.push_str(&recent);
    }

    user.push_str("\n\nCURRENT INTERVIEWER TURN\n");
    user.push_str(current.text.trim());

    user.push_str("\n\nTASK\n");
    user.push_str(match job.language {
        Language::Sv => "Interview language: Swedish (sv). Help the candidate answer the current interviewer turn.",
        Language::En => "Interview language: English (en). Help the candidate answer the current interviewer turn.",
    });

    ContextPackage { system: SYSTEM_RULES.to_string(), user }
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

/// Newest turns win the budget; output stays in chronological order.
fn format_recent(history: &[Turn]) -> String {
    let mut selected: Vec<String> = Vec::new();
    let mut used = 0;
    for turn in history.iter().rev().take(RECENT_TURNS_MAX) {
        let label = match turn.speaker {
            Speaker::Interviewer => "INTERVIEWER",
            Speaker::Candidate => "CANDIDATE",
        };
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

fn truncate_chars(text: &str, max: usize) -> String {
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

    #[test]
    fn package_contains_sections_in_readme_order() {
        let profile = crate::profile::parse_profile("p", "candidate:\n  name: Alex\nstories:\n  s1:\n    action: x\n".into()).unwrap();
        let history = vec![
            turn(Speaker::Interviewer, "Berätta om ett problem."),
            turn(Speaker::Candidate, "Jag byggde lösenordsåterställning."),
        ];
        let package = build_context(&job(), Some(&profile), &history, &turn(Speaker::Interviewer, "Hur testade du det?"));
        let order = ["JOB CONTEXT", "CANDIDATE PROFILE", "RECENT CONVERSATION", "CURRENT INTERVIEWER TURN", "TASK"];
        let positions: Vec<usize> = order.iter().map(|header| package.user.find(header).expect(header)).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}");
        assert!(package.user.contains("Role: Backend Developer"));
        assert!(package.user.contains("Important topics: APIs, testing"));
        assert!(package.user.contains("name: Alex"));
        assert!(package.user.contains("INTERVIEWER: Berätta om ett problem.\nCANDIDATE: Jag byggde lösenordsåterställning."));
        assert!(package.user.contains("CURRENT INTERVIEWER TURN\nHur testade du det?"));
        assert!(package.user.contains("Swedish (sv)"));
        assert!(package.system.contains("NO_VERIFIED_EXAMPLE"));
        assert!(package.system.contains("ANCHORS"));
    }

    #[test]
    fn missing_profile_and_job_are_explicit() {
        let package = build_context(
            &JobContext { language: Language::En, ..JobContext::default() },
            None,
            &[],
            &turn(Speaker::Interviewer, "What is SSR?"),
        );
        assert!(package.user.contains("(No job context given.)"));
        assert!(package.user.contains("No candidate profile loaded"));
        assert!(package.user.contains("(This is the start of the conversation.)"));
        assert!(package.user.contains("English (en)"));
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
    fn job_description_is_truncated() {
        let mut long_job = job();
        long_job.job_description = "x".repeat(JOB_DESCRIPTION_CHARS_MAX + 50);
        let formatted = format_job(&long_job);
        assert!(formatted.contains("[…truncated]"));
        assert!(formatted.chars().count() < JOB_DESCRIPTION_CHARS_MAX + 200);
    }

    #[test]
    fn job_context_deserializes_from_ui_shape() {
        let parsed: JobContext = serde_json::from_str(r#"{"company":"A","role":"R","importantTopics":["x"],"language":"en"}"#).unwrap();
        assert_eq!(parsed.language, Language::En);
        assert_eq!(parsed.important_topics, vec!["x"]);
        assert_eq!(parsed.job_description, "");
    }
}
