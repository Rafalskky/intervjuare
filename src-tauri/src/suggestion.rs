//! Progressive parser for the model's plain-text response contract:
//!
//! ```text
//! ANCHORS
//! - anchor
//! ANSWER
//! spoken answer…
//! SOURCES: id, id
//! ```
//!
//! `parse` is re-run on the accumulated text after every streamed delta. With
//! `complete = false` the trailing unterminated line is treated as in flight:
//! an anchor appears only once its line ends, while answer text streams
//! immediately unless the tail could still become a marker such as `SOURCES:`.
//! The parser is tolerant of markdown decoration and of a missing header.

use serde::Serialize;

pub const MAX_ANCHORS: usize = 6;
const MAX_ANCHOR_CHARS: usize = 120;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSuggestion {
    pub anchors: Vec<String>,
    pub answer: String,
    pub sources: Vec<String>,
    pub skip: bool,
    pub no_verified_example: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Start,
    Anchors,
    Answer,
    Sources,
}

enum Marker<'a> {
    Anchors,
    Answer,
    Skip,
    NoVerifiedExample,
    Sources(&'a str),
}

pub fn parse(raw: &str, complete: bool) -> ParsedSuggestion {
    let mut lines: Vec<&str> = raw.split('\n').collect();
    let tail = if complete { None } else { lines.pop() };

    let mut parsed = ParsedSuggestion::default();
    let mut answer_lines: Vec<&str> = Vec::new();
    let mut saw_skip = false;
    let mut section = Section::Start;

    for line in lines {
        let line = line.trim_end_matches('\r');
        match marker(line) {
            Some(Marker::Anchors) => section = Section::Anchors,
            Some(Marker::Answer) => section = Section::Answer,
            Some(Marker::Skip) => saw_skip = true,
            Some(Marker::NoVerifiedExample) => parsed.no_verified_example = true,
            Some(Marker::Sources(rest)) => {
                parsed.sources.extend(parse_sources(rest));
                section = Section::Sources;
            }
            None => {
                if line.trim().is_empty() {
                    if section == Section::Answer {
                        answer_lines.push("");
                    }
                    continue;
                }
                match section {
                    Section::Start if bullet_text(line).is_some() => {
                        section = Section::Anchors;
                        push_anchor(&mut parsed.anchors, line);
                    }
                    Section::Start => {
                        section = Section::Answer;
                        answer_lines.push(line.trim());
                    }
                    Section::Anchors => {
                        let short = line.trim().chars().count() <= MAX_ANCHOR_CHARS / 2;
                        if bullet_text(line).is_some() || (short && parsed.anchors.len() < MAX_ANCHORS) {
                            push_anchor(&mut parsed.anchors, line);
                        } else {
                            section = Section::Answer;
                            answer_lines.push(line.trim());
                        }
                    }
                    Section::Answer => answer_lines.push(line.trim()),
                    Section::Sources => {}
                }
            }
        }
    }

    if let Some(tail) = tail.map(|tail| tail.trim_end_matches('\r')) {
        let streams_into_answer = matches!(section, Section::Answer)
            || (section == Section::Start && !tail.trim().is_empty() && bullet_text(tail).is_none());
        if streams_into_answer && !tail.trim().is_empty() && !could_become_marker(tail) {
            answer_lines.push(tail.trim());
        }
    }

    parsed.answer = answer_lines.join("\n").trim().to_string();
    parsed.skip = saw_skip && parsed.anchors.is_empty() && parsed.answer.is_empty();
    parsed
}

/// Markdown-insensitive view of a line used for marker detection.
fn normalized(line: &str) -> String {
    line.trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches('*')
        .trim_matches('_')
        .trim()
        .to_string()
}

fn marker(line: &str) -> Option<Marker<'_>> {
    let normalized = normalized(line);
    let upper = normalized.trim_end_matches(':').trim().to_uppercase();
    match upper.as_str() {
        "ANCHORS" | "SPEAKING ANCHORS" | "START HERE" => return Some(Marker::Anchors),
        "ANSWER" | "FULL ANSWER" => return Some(Marker::Answer),
        "SKIP" => return Some(Marker::Skip),
        "NO_VERIFIED_EXAMPLE" | "NO VERIFIED EXAMPLE" | "NO VERIFIED PERSONAL EXAMPLE" => {
            return Some(Marker::NoVerifiedExample)
        }
        _ => {}
    }
    let trimmed = line.trim().trim_start_matches(['*', '#', '_']).trim_start();
    let prefix_len = "SOURCES".len();
    if trimmed.len() > prefix_len && trimmed.is_char_boundary(prefix_len) && trimmed[..prefix_len].eq_ignore_ascii_case("SOURCES") {
        let rest = trimmed[prefix_len..].trim_start_matches(['*', '_']);
        if let Some(rest) = rest.strip_prefix(':') {
            return Some(Marker::Sources(rest));
        }
    }
    None
}

/// True while an unterminated line might still turn into a marker line, so
/// it must not flash into the visible answer.
fn could_become_marker(tail: &str) -> bool {
    let candidate = normalized(tail).to_uppercase();
    if candidate.is_empty() {
        return true;
    }
    ["SOURCES:", "NO_VERIFIED_EXAMPLE", "NO VERIFIED", "ANSWER", "ANCHORS", "SKIP"]
        .iter()
        .any(|marker| marker.starts_with(candidate.as_str()) || candidate.starts_with("SOURCES:"))
}

fn bullet_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for bullet in ["- ", "• ", "* ", "– ", "— "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return Some(rest);
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 2 {
        let rest = &trimmed[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(rest);
        }
    }
    None
}

fn push_anchor(anchors: &mut Vec<String>, line: &str) {
    if anchors.len() >= MAX_ANCHORS {
        return;
    }
    let text = bullet_text(line).unwrap_or(line).trim().trim_matches('*').trim().trim_end_matches('.').trim();
    if text.is_empty() {
        return;
    }
    anchors.push(text.chars().take(MAX_ANCHOR_CHARS).collect());
}

fn parse_sources(rest: &str) -> Vec<String> {
    rest.split([',', ';'])
        .map(|id| id.trim().trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '[' | ']' | '*' | '.')).trim())
        .filter(|id| !id.is_empty() && !matches!(id.to_lowercase().as_str(), "none" | "ingen" | "inga" | "-" | "n/a"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "ANCHORS\n- Sagobo password reset\n- multiple tabs\n- server-side protection\n- Jest tests\nANSWER\nNär jag arbetade med Sagobo märkte jag att...\n\nJag flyttade därför kontrollen till servern.\nSOURCES: password_reset_race_condition, sagobo\n";

    #[test]
    fn parses_the_complete_contract() {
        let parsed = parse(FULL, true);
        assert_eq!(parsed.anchors, vec!["Sagobo password reset", "multiple tabs", "server-side protection", "Jest tests"]);
        assert_eq!(parsed.answer, "När jag arbetade med Sagobo märkte jag att...\n\nJag flyttade därför kontrollen till servern.");
        assert_eq!(parsed.sources, vec!["password_reset_race_condition", "sagobo"]);
        assert!(!parsed.skip);
        assert!(!parsed.no_verified_example);
    }

    #[test]
    fn every_streaming_prefix_is_monotonic_and_never_leaks_markers() {
        let mut previous = ParsedSuggestion::default();
        let chars: Vec<char> = FULL.chars().collect();
        for end in 0..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            let parsed = parse(&prefix, false);
            assert!(parsed.anchors.len() >= previous.anchors.len(), "anchors shrank at {end}");
            assert!(parsed.anchors.starts_with(&previous.anchors), "anchors changed at {end}");
            assert!(!parsed.answer.contains("SOURCES"), "marker leaked at {end}: {:?}", parsed.answer);
            assert!(!parsed.answer.to_uppercase().contains("ANSWER"), "header leaked at {end}");
            for anchor in &parsed.anchors {
                assert!(FULL.contains(&format!("- {anchor}\n")), "partial anchor {anchor:?} at {end}");
            }
            previous = parsed;
        }
        assert_eq!(previous, parse(FULL, true));
    }

    #[test]
    fn anchors_appear_line_by_line_before_the_answer() {
        let parsed = parse("ANCHORS\n- Google Analytics\n- adbl", false);
        assert_eq!(parsed.anchors, vec!["Google Analytics"]);
        assert!(parsed.answer.is_empty());
        let parsed = parse("ANCHORS\n- Google Analytics\n- adblockers\nANSWER\nNär jag implementerade", false);
        assert_eq!(parsed.anchors.len(), 2);
        assert_eq!(parsed.answer, "När jag implementerade");
    }

    #[test]
    fn skip_only_when_nothing_else_was_produced() {
        assert!(parse("SKIP\n", true).skip);
        assert!(parse("SKIP", true).skip);
        assert!(!parse("SKIP", false).skip, "unterminated SKIP is not final yet");
        assert!(!parse("SKIP\nANCHORS\n- x\n", true).skip);
    }

    #[test]
    fn tolerates_markdown_decoration_and_missing_header() {
        let parsed = parse("**ANCHORS:**\n1. SSR basics\n2) hydration\n### Answer\nSSR betyder att...\n**Sources:** none\n", true);
        assert_eq!(parsed.anchors, vec!["SSR basics", "hydration"]);
        assert_eq!(parsed.answer, "SSR betyder att...");
        assert!(parsed.sources.is_empty());

        let parsed = parse("- direct bullet\n- second\nANSWER\ntext\n", true);
        assert_eq!(parsed.anchors, vec!["direct bullet", "second"]);

        let parsed = parse("Plain answer without any format.", true);
        assert!(parsed.anchors.is_empty());
        assert_eq!(parsed.answer, "Plain answer without any format.");
    }

    #[test]
    fn flags_missing_verified_example() {
        let parsed = parse("ANCHORS\n- no Java at work yet\nANSWER\nJag har inte arbetat med Java i jobbet.\nNO_VERIFIED_EXAMPLE\nSOURCES: none\n", true);
        assert!(parsed.no_verified_example);
        assert!(parsed.sources.is_empty());
        assert_eq!(parsed.answer, "Jag har inte arbetat med Java i jobbet.");
    }

    #[test]
    fn caps_anchor_count_and_length() {
        let many = format!("ANCHORS\n{}ANSWER\nx\n", (0..10).map(|i| format!("- anchor {i}\n")).collect::<String>());
        assert_eq!(parse(&many, true).anchors.len(), MAX_ANCHORS);
        let long = format!("ANCHORS\n- {}\n", "a".repeat(500));
        assert_eq!(parse(&long, true).anchors[0].chars().count(), MAX_ANCHOR_CHARS);
    }

    #[test]
    fn sources_accept_decorated_ids() {
        assert_eq!(parse_sources(" `sagobo`, \"analytics_adblock\"; [x]."), vec!["sagobo", "analytics_adblock", "x"]);
        assert!(parse_sources(" ingen ").is_empty());
    }
}
