//! Compact interview memory (Milestone 4).
//!
//! Recent conversation (last turns, verbatim) and long-term memory are
//! separate. Memory is a short YAML summary that the model updates in the
//! background from turns it has not absorbed yet. It never blocks an answer:
//! a suggestion always uses the memory that exists at that moment plus the
//! verbatim recent turns, which overlap with what memory is still absorbing.

use crate::context::{format_recent, Language};
use crate::conversation::Turn;

/// Absorb only after this many complete new turns (≈ two exchanges), so a
/// background request is not spent on every sentence.
pub const MIN_NEW_TURNS: usize = 4;
pub const MAX_MEMORY_CHARS: usize = 3_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InterviewMemory {
    pub text: String,
    /// Captured end time of the last absorbed turn.
    pub covered_until_ms: u64,
    pub absorbed_turns: usize,
}

pub const MEMORY_INSTRUCTIONS: &str = "\
You maintain the compact memory of a live job interview for the candidate's real-time copilot.
Merge NEW TURNS into CURRENT MEMORY and output only the updated memory as YAML, no code fences, no commentary, under 250 words, with these keys:
language: sv or en
current_topic: short phrase
topics_discussed: list
stories_used: profile story ids the candidate actually told (use ids from PROFILE IDS)
interviewer_focus: what the interviewer seems to care about
candidate_claims: facts the candidate stated about themselves, short
open_questions: things asked but not answered fully, or promised follow-ups
Record only what was said. Never invent facts. Keep earlier entries unless they were corrected.";

/// Complete turns not yet in memory, oldest first, stopping at the first
/// incomplete turn so nothing is absorbed out of order.
pub fn turns_to_absorb(history: &[Turn], covered_until_ms: u64) -> Vec<Turn> {
    history
        .iter()
        .filter(|turn| turn.ended_at_ms > covered_until_ms)
        .take_while(|turn| turn.complete)
        .cloned()
        .collect()
}

pub fn request_text(previous: &str, new_turns: &[Turn], profile_ids: &[String], language: Language) -> String {
    let previous = previous.trim();
    format!(
        "INTERVIEW LANGUAGE\n{}\n\nPROFILE IDS\n{}\n\nCURRENT MEMORY\n{}\n\nNEW TURNS\n{}",
        match language {
            Language::Sv => "sv",
            Language::En => "en",
        },
        if profile_ids.is_empty() { "(none)".to_string() } else { profile_ids.join(", ") },
        if previous.is_empty() { "(empty)" } else { previous },
        format_recent(new_turns),
    )
}

/// Accepts the model output as memory text: strips code fences, bounds size,
/// rejects empty output and a bare `SKIP`.
pub fn sanitize(output: &str) -> Option<String> {
    let mut text = output.trim();
    if let Some(rest) = text.strip_prefix("```") {
        text = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic()).trim_start();
        text = text.strip_suffix("```").unwrap_or(text).trim();
    }
    if text.is_empty() || text.eq_ignore_ascii_case("skip") {
        return None;
    }
    Some(text.chars().take(MAX_MEMORY_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Speaker;

    fn turn(speaker: Speaker, text: &str, ended: u64, complete: bool) -> Turn {
        Turn { speaker, text: text.into(), complete, first_segment_id: format!("{ended}"), started_at_ms: ended - 500, ended_at_ms: ended }
    }

    #[test]
    fn absorbs_only_new_complete_turns_in_order() {
        let history = vec![
            turn(Speaker::Interviewer, "old", 1_000, true),
            turn(Speaker::Candidate, "new 1", 2_000, true),
            turn(Speaker::Interviewer, "new 2", 3_000, true),
            turn(Speaker::Candidate, "still speaking", 4_000, false),
            turn(Speaker::Interviewer, "after gap", 5_000, true),
        ];
        let absorbed = turns_to_absorb(&history, 1_000);
        assert_eq!(absorbed.iter().map(|turn| turn.text.as_str()).collect::<Vec<_>>(), vec!["new 1", "new 2"]);
        assert!(turns_to_absorb(&history, 5_000).is_empty());
    }

    #[test]
    fn request_contains_previous_memory_ids_and_new_turns() {
        let text = request_text(
            "topics_discussed: [React]",
            &[turn(Speaker::Interviewer, "Hur testade du det?", 2_000, true), turn(Speaker::Candidate, "Med Jest.", 3_000, true)],
            &["password_reset_race_condition".into()],
            Language::Sv,
        );
        assert!(text.contains("INTERVIEW LANGUAGE\nsv"));
        assert!(text.contains("PROFILE IDS\npassword_reset_race_condition"));
        assert!(text.contains("CURRENT MEMORY\ntopics_discussed: [React]"));
        assert!(text.contains("INTERVIEWER: Hur testade du det?\nCANDIDATE: Med Jest."));
        assert!(request_text("", &[], &[], Language::En).contains("CURRENT MEMORY\n(empty)"));
    }

    #[test]
    fn sanitize_strips_fences_and_rejects_empty_output() {
        assert_eq!(sanitize("```yaml\nlanguage: sv\n```").as_deref(), Some("language: sv"));
        assert_eq!(sanitize("  language: en  ").as_deref(), Some("language: en"));
        assert!(sanitize("   ").is_none());
        assert!(sanitize("SKIP").is_none());
        assert_eq!(sanitize(&"x".repeat(MAX_MEMORY_CHARS + 5)).unwrap().len(), MAX_MEMORY_CHARS);
    }
}
