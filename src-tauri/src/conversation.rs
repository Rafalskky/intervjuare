//! Speaker-aware conversation state built from transcript events.
//!
//! Speaker identity always comes from the capture source (`Speaker`) and is
//! never inferred here. Partial hypotheses are replaced by later revisions of
//! the same segment; finals are immutable. Consecutive segments from the same
//! speaker form one turn, so an interviewer question split by a short pause is
//! still one question.

use crate::{Speaker, TranscriptEvent};

/// Bound on retained segments. Older context belongs to interview memory
/// (Milestone 4), not to the raw recent-conversation buffer.
pub const MAX_SEGMENTS: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    id: String,
    speaker: Speaker,
    revision: u32,
    text: String,
    final_segment: bool,
    started_at_ms: u64,
    ended_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub speaker: Speaker,
    pub text: String,
    /// True when every segment of the turn is final.
    pub complete: bool,
    pub first_segment_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// Stale revision, revision of an immutable final, or empty text.
    Ignored,
    Changed { speaker: Speaker, segment_id: String, final_segment: bool },
}

#[derive(Debug, Default)]
pub struct Conversation {
    session_id: Option<String>,
    /// Ordered by `started_at_ms`, then arrival.
    segments: Vec<Segment>,
}

impl Conversation {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn reset(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
        self.segments.clear();
    }

    /// Applies one transcript event. A different `session_id` starts a new
    /// conversation, because a new capture session is a new interview.
    pub fn apply(&mut self, event: &TranscriptEvent) -> Update {
        if self.session_id.as_deref() != Some(event.session_id.as_str()) {
            self.reset(Some(event.session_id.clone()));
        }
        let text = normalize_whitespace(&event.text);
        if let Some(existing) = self.segments.iter_mut().find(|segment| segment.id == event.segment_id) {
            if existing.final_segment || event.revision <= existing.revision || existing.speaker != event.speaker {
                return Update::Ignored;
            }
            existing.revision = event.revision;
            // An empty final keeps the last valid hypothesis (same rule as
            // the transcript contract: never erase accepted text).
            if !text.is_empty() {
                existing.text = text;
            }
            existing.final_segment = event.final_segment;
            existing.ended_at_ms = event.ended_at_ms;
            return Update::Changed {
                speaker: event.speaker,
                segment_id: event.segment_id.clone(),
                final_segment: event.final_segment,
            };
        }
        if text.is_empty() {
            return Update::Ignored;
        }
        let segment = Segment {
            id: event.segment_id.clone(),
            speaker: event.speaker,
            revision: event.revision,
            text,
            final_segment: event.final_segment,
            started_at_ms: event.started_at_ms,
            ended_at_ms: event.ended_at_ms,
        };
        let index = self
            .segments
            .iter()
            .rposition(|other| other.started_at_ms <= segment.started_at_ms)
            .map_or(0, |position| position + 1);
        self.segments.insert(index, segment);
        if self.segments.len() > MAX_SEGMENTS {
            let excess = self.segments.len() - MAX_SEGMENTS;
            self.segments.drain(..excess);
        }
        Update::Changed {
            speaker: event.speaker,
            segment_id: event.segment_id.clone(),
            final_segment: event.final_segment,
        }
    }

    pub fn turns(&self) -> Vec<Turn> {
        let mut turns: Vec<Turn> = Vec::new();
        for segment in &self.segments {
            match turns.last_mut() {
                Some(turn) if turn.speaker == segment.speaker => {
                    turn.text.push(' ');
                    turn.text.push_str(&segment.text);
                    turn.complete &= segment.final_segment;
                    turn.ended_at_ms = turn.ended_at_ms.max(segment.ended_at_ms);
                }
                _ => turns.push(Turn {
                    speaker: segment.speaker,
                    text: segment.text.clone(),
                    complete: segment.final_segment,
                    first_segment_id: segment.id.clone(),
                    started_at_ms: segment.started_at_ms,
                    ended_at_ms: segment.ended_at_ms,
                }),
            }
        }
        turns
    }

    /// The most recent interviewer turn plus every turn before it.
    pub fn latest_interviewer_turn(&self) -> Option<(Vec<Turn>, Turn)> {
        let mut turns = self.turns();
        let index = turns.iter().rposition(|turn| turn.speaker == Speaker::Interviewer)?;
        turns.truncate(index + 1);
        let current = turns.pop()?;
        Some((turns, current))
    }

    /// The turn containing `segment_id`, if it is still retained.
    pub fn turn_of_segment(&self, segment_id: &str) -> Option<Turn> {
        let position = self.segments.iter().position(|segment| segment.id == segment_id)?;
        let speaker = self.segments[position].speaker;
        let mut start = position;
        while start > 0 && self.segments[start - 1].speaker == speaker {
            start -= 1;
        }
        let first_id = &self.segments[start].id;
        self.turns().into_iter().find(|turn| &turn.first_segment_id == first_id)
    }
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Single-word acknowledgements that never expect an answer. This is a
/// deterministic filter for non-content tokens only, so an automatic Hermes
/// request is not spent on "Mm." while the candidate is talking. Anything
/// longer, and any real one-word question such as "Varför?", still goes to
/// Hermes, which decides whether an answer is expected.
pub fn is_backchannel(text: &str) -> bool {
    const BACKCHANNELS: [&str; 24] = [
        "mm", "mhm", "mmm", "hm", "ja", "jaa", "jo", "japp", "okej", "ok", "okay", "aha", "jaha", "precis",
        "absolut", "bra", "toppen", "yes", "yeah", "right", "sure", "great", "uh-huh", "alright",
    ];
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    !words.is_empty() && words.len() <= 2 && words.iter().all(|word| BACKCHANNELS.contains(&word.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(session: &str, id: &str, revision: u32, speaker: Speaker, text: &str, final_segment: bool, started: u64) -> TranscriptEvent {
        TranscriptEvent {
            session_id: session.into(),
            segment_id: id.into(),
            revision,
            speaker,
            text: text.into(),
            final_segment,
            started_at_ms: started,
            ended_at_ms: started + 1_000,
            captured_at_ms: started + 1_000,
            emitted_at_ms: started + 1_500,
            latency_ms: 500,
        }
    }

    #[test]
    fn revisions_replace_partials_and_finals_are_immutable() {
        let mut conversation = Conversation::default();
        assert!(matches!(
            conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Kan du", false, 0)),
            Update::Changed { final_segment: false, .. }
        ));
        conversation.apply(&event("s", "interviewer-1", 2, Speaker::Interviewer, "Kan du berätta", false, 0));
        assert_eq!(conversation.apply(&event("s", "interviewer-1", 2, Speaker::Interviewer, "stale", false, 0)), Update::Ignored);
        conversation.apply(&event("s", "interviewer-1", 3, Speaker::Interviewer, "Kan du berätta om dig själv?", true, 0));
        assert_eq!(conversation.apply(&event("s", "interviewer-1", 4, Speaker::Interviewer, "mutated", true, 0)), Update::Ignored);
        let turns = conversation.turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "Kan du berätta om dig själv?");
        assert!(turns[0].complete);
    }

    #[test]
    fn empty_final_keeps_last_valid_hypothesis() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Hur testade du det", false, 0));
        conversation.apply(&event("s", "interviewer-1", 2, Speaker::Interviewer, "  ", true, 0));
        let turn = &conversation.turns()[0];
        assert_eq!(turn.text, "Hur testade du det");
        assert!(turn.complete);
    }

    #[test]
    fn consecutive_same_speaker_segments_form_one_turn() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Berätta om ett problem.", true, 0));
        conversation.apply(&event("s", "interviewer-2", 1, Speaker::Interviewer, "Som du själv upptäckte.", false, 2_000));
        conversation.apply(&event("s", "candidate-1", 1, Speaker::Candidate, "Ja, absolut.", true, 5_000));
        let turns = conversation.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "Berätta om ett problem. Som du själv upptäckte.");
        assert!(!turns[0].complete, "a pending partial keeps the turn open");
        assert_eq!(turns[0].first_segment_id, "interviewer-1");
        assert_eq!(turns[1].speaker, Speaker::Candidate);
    }

    #[test]
    fn segments_are_ordered_by_capture_start_not_arrival() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "candidate-1", 1, Speaker::Candidate, "Ja.", true, 4_000));
        conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Vad heter du?", true, 1_000));
        let turns = conversation.turns();
        assert_eq!(turns[0].speaker, Speaker::Interviewer);
        assert_eq!(turns[1].speaker, Speaker::Candidate);
    }

    #[test]
    fn a_new_session_starts_a_new_conversation() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("a", "interviewer-1", 1, Speaker::Interviewer, "Hej", true, 0));
        conversation.apply(&event("b", "interviewer-1", 1, Speaker::Interviewer, "Ny intervju", true, 0));
        assert_eq!(conversation.session_id(), Some("b"));
        assert_eq!(conversation.turns().len(), 1);
        assert_eq!(conversation.turns()[0].text, "Ny intervju");
    }

    #[test]
    fn speaker_of_an_existing_segment_cannot_change() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "x-1", 1, Speaker::Interviewer, "Hej", false, 0));
        assert_eq!(conversation.apply(&event("s", "x-1", 2, Speaker::Candidate, "Hej", true, 0)), Update::Ignored);
    }

    #[test]
    fn latest_interviewer_turn_returns_history_before_it() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Berätta om Sagobo.", true, 0));
        conversation.apply(&event("s", "candidate-1", 1, Speaker::Candidate, "Jag byggde lösenordsåterställning.", true, 2_000));
        conversation.apply(&event("s", "interviewer-2", 1, Speaker::Interviewer, "Hur testade du det?", true, 6_000));
        conversation.apply(&event("s", "candidate-2", 1, Speaker::Candidate, "Jo", false, 8_000));
        let (history, current) = conversation.latest_interviewer_turn().expect("interviewer turn");
        assert_eq!(current.text, "Hur testade du det?");
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].text, "Jag byggde lösenordsåterställning.");
    }

    #[test]
    fn turn_of_segment_finds_the_containing_turn() {
        let mut conversation = Conversation::default();
        conversation.apply(&event("s", "interviewer-1", 1, Speaker::Interviewer, "Del ett.", true, 0));
        conversation.apply(&event("s", "interviewer-2", 1, Speaker::Interviewer, "Del två?", true, 1_500));
        let turn = conversation.turn_of_segment("interviewer-2").expect("turn");
        assert_eq!(turn.first_segment_id, "interviewer-1");
        assert_eq!(turn.text, "Del ett. Del två?");
    }

    #[test]
    fn retained_segments_are_bounded() {
        let mut conversation = Conversation::default();
        for index in 0..(MAX_SEGMENTS + 10) {
            let speaker = if index % 2 == 0 { Speaker::Interviewer } else { Speaker::Candidate };
            conversation.apply(&event("s", &format!("seg-{index}"), 1, speaker, "text", true, index as u64 * 1_000));
        }
        assert_eq!(conversation.segments.len(), MAX_SEGMENTS);
        assert_eq!(conversation.segments[0].id, "seg-10");
    }

    #[test]
    fn backchannel_filter_only_matches_non_content_tokens() {
        assert!(is_backchannel("Mm."));
        assert!(is_backchannel("Okej, precis."));
        assert!(is_backchannel("Yeah"));
        assert!(!is_backchannel("Varför?"));
        assert!(!is_backchannel("Okej, berätta mer."));
        assert!(!is_backchannel(""));
    }
}
