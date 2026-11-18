//! Post-interview analysis (Milestone 6, README "Post-Interview Mode").
//!
//! After the interview, one model request turns the retained transcript,
//! interview memory, stories used, job context and profile into a practical
//! Markdown report. It never runs during the interview path and never edits
//! `profile.yaml`: profile changes are only suggested, for the owner to
//! verify and apply by hand.

use crate::context::{format_job_for_analysis, speaker_label, JobContext, UsedStory};
use crate::conversation::Turn;
use crate::llm::ModelRequest;
use crate::profile::Profile;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Transcript budget for one analysis request; older turns beyond it are
/// represented by interview memory.
pub const TRANSCRIPT_CHARS_MAX: usize = 40_000;

pub const ANALYSIS_INSTRUCTIONS: &str = "\
You review a finished job interview for the candidate so they can improve before the next one.
Write a concise, practical Markdown report in the REPORT LANGUAGE. Use ## headings in this order:
1. Summary
2. Questions asked: numbered, one line each on what was asked and how the candidate answered
3. Strong answers: what worked and why
4. Weak answers: why, and a better spoken version in the interview language, grounded only in CANDIDATE PROFILE and what the candidate actually said
5. Difficult questions
6. Stories: worked well / overused / unused but relevant (use profile ids)
7. Technical areas to review
8. Likely follow-up questions for the next round
9. Suggested additions to profile.yaml: only facts the candidate actually stated in the interview that are missing from the profile, as short YAML snippets, each marked as a suggestion to verify. Never add anything that was not said. Write \"None\" if there is nothing.
10. Practice questions
Rules: base every statement on TRANSCRIPT, INTERVIEW MEMORY, JOB CONTEXT and CANDIDATE PROFILE; never invent what was said. The transcript comes from speech recognition and may contain recognition errors; do not criticise wording that is likely a transcription error. Be specific and kind, not generic.";

pub fn language_name(code: &str) -> &'static str {
    match code {
        "sv" => "Swedish",
        "en" => "English",
        _ => "Polish",
    }
}

/// Chronological transcript within `max_chars`; when too long, the oldest
/// turns are dropped (interview memory covers them). Returns whether it cut.
pub fn format_transcript(turns: &[Turn], max_chars: usize) -> (String, bool) {
    let lines: Vec<String> = turns.iter().map(|turn| format!("{}: {}", speaker_label(turn.speaker), turn.text.trim())).collect();
    let mut kept: Vec<&String> = Vec::new();
    let mut used = 0;
    for line in lines.iter().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > max_chars {
            break;
        }
        used += cost;
        kept.push(line);
    }
    let truncated = kept.len() < lines.len();
    kept.reverse();
    (kept.into_iter().cloned().collect::<Vec<_>>().join("\n"), truncated)
}

#[allow(clippy::too_many_arguments)]
pub fn build_request(
    job: &JobContext,
    profile: Option<&Profile>,
    memory: &str,
    stories_used: &[UsedStory],
    turns: &[Turn],
    report_language: &str,
    model: &str,
    reasoning_effort: &str,
) -> Result<ModelRequest, String> {
    if turns.is_empty() {
        return Err("Nothing to analyse yet: the conversation is empty.".into());
    }
    let (transcript, truncated) = format_transcript(turns, TRANSCRIPT_CHARS_MAX);
    let stories = if stories_used.is_empty() {
        "(none recorded)".to_string()
    } else {
        stories_used.iter().map(|story| format!("- {} (suggested for: {})", story.id, story.question)).collect::<Vec<_>>().join("\n")
    };
    let text = format!(
        "REPORT LANGUAGE\n{}\n\nJOB CONTEXT\n{}\n\nCANDIDATE PROFILE\n{}\n\nINTERVIEW MEMORY\n{}\n\nSTORIES SUGGESTED DURING THE INTERVIEW\n{}\n\nTRANSCRIPT{}\n{}",
        language_name(report_language),
        format_job_for_analysis(job),
        profile.map_or("(No candidate profile loaded.)", |profile| profile.raw.trim_end()),
        if memory.trim().is_empty() { "(empty)" } else { memory.trim() },
        stories,
        if truncated { " (oldest turns omitted; see INTERVIEW MEMORY)" } else { "" },
        transcript,
    );
    Ok(ModelRequest {
        model: model.to_string(),
        reasoning_effort: reasoning_effort.to_string(),
        instructions: ANALYSIS_INSTRUCTIONS.to_string(),
        text,
        image_data_url: None,
        cache_key: None,
    })
}

/// `~/Library/Application Support/com.rafal.intervjuare/reports`, next to settings.
pub fn reports_dir(settings_path: &Path) -> PathBuf {
    settings_path.parent().unwrap_or_else(|| Path::new(".")).join("reports")
}

/// `interview-YYYY-MM-DD-HHMM-<company>.md` (UTC), filesystem-safe.
pub fn report_filename(company: &str, now_ms: u64) -> String {
    let (year, month, day, hour, minute) = utc_parts(now_ms);
    let slug: String = company
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(40)
        .collect();
    let suffix = if slug.is_empty() { String::new() } else { format!("-{slug}") };
    format!("interview-{year:04}-{month:02}-{day:02}-{hour:02}{minute:02}{suffix}.md")
}

/// Saves a report only on explicit owner action; owner-only permissions,
/// never overwrites an existing report.
pub fn save_report(dir: &Path, filename: &str, text: &str) -> Result<PathBuf, String> {
    if text.trim().is_empty() {
        return Err("The report is empty.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|error| format!("could not create the reports folder: {error}"))?;
    }
    let stem = filename.trim_end_matches(".md");
    for attempt in 0..100 {
        let name = if attempt == 0 { format!("{stem}.md") } else { format!("{stem}-{attempt}.md") };
        let path = dir.join(name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes()).map_err(|error| format!("could not write the report: {error}"))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not save the report: {error}")),
        }
    }
    Err("could not find a free report file name".into())
}

/// Civil date from Unix milliseconds (UTC), without a date dependency.
fn utc_parts(now_ms: u64) -> (i64, u32, u32, u32, u32) {
    let secs = (now_ms / 1_000) as i64;
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    // Howard Hinnant's days-from-civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day, (seconds_of_day / 3_600) as u32, ((seconds_of_day % 3_600) / 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Language;
    use crate::Speaker;

    fn turn(speaker: Speaker, text: &str) -> Turn {
        Turn { speaker, text: text.into(), complete: true, first_segment_id: text.into(), started_at_ms: 0, ended_at_ms: 0 }
    }

    #[test]
    fn request_contains_every_source_and_the_report_language() {
        let profile = crate::profile::parse_profile("p", "candidate:\n  name: Alex\nstories:\n  s1:\n    action: x\n".into()).unwrap();
        let job = JobContext { company: "Example AB".into(), role: "Backend".into(), language: Language::Sv, ..JobContext::default() };
        let used = vec![UsedStory { id: "s1".into(), question: "Berätta om ett problem.".into() }];
        let turns = vec![turn(Speaker::Interviewer, "Berätta om ett problem."), turn(Speaker::Candidate, "Jag byggde X.")];
        let request = build_request(&job, Some(&profile), "topics_discussed: [X]", &used, &turns, "pl", "gpt-test", "none").unwrap();
        assert_eq!(request.instructions, ANALYSIS_INSTRUCTIONS);
        assert!(request.text.starts_with("REPORT LANGUAGE\nPolish"));
        assert!(request.text.contains("Company: Example AB"));
        assert!(request.text.contains("Interview language: sv"));
        assert!(request.text.contains("name: Alex"));
        assert!(request.text.contains("INTERVIEW MEMORY\ntopics_discussed: [X]"));
        assert!(request.text.contains("- s1 (suggested for: Berätta om ett problem.)"));
        assert!(request.text.contains("TRANSCRIPT\nINTERVIEWER: Berätta om ett problem.\nCANDIDATE: Jag byggde X."));
        assert!(request.image_data_url.is_none());
        assert!(ANALYSIS_INSTRUCTIONS.contains("Never add anything that was not said"));
    }

    #[test]
    fn empty_conversation_is_rejected() {
        assert!(build_request(&JobContext::default(), None, "", &[], &[], "pl", "m", "").unwrap_err().contains("Nothing to analyse"));
    }

    #[test]
    fn long_transcripts_keep_the_newest_turns() {
        let turns: Vec<Turn> = (0..100).map(|index| turn(Speaker::Candidate, &format!("turn {index:03} {}", "x".repeat(50)))).collect();
        let (text, truncated) = format_transcript(&turns, 600);
        assert!(truncated);
        assert!(text.chars().count() <= 600);
        assert!(text.ends_with(&format!("turn 099 {}", "x".repeat(50))));
        assert!(!text.contains("turn 000"));
        let (_, truncated) = format_transcript(&turns[..2], 10_000);
        assert!(!truncated);
        let request = build_request(&JobContext::default(), None, "", &[], &turns, "en", "m", "").unwrap();
        assert!(!request.text.contains("oldest turns omitted"), "100 short turns fit the real budget");
    }

    #[test]
    fn filenames_are_dated_and_safe() {
        // 2026-11-18 19:49 UTC
        assert_eq!(report_filename("Example AB / Stockholm", 1_795_031_340_000), "interview-2026-11-18-1949-example-ab-stockholm.md");
        assert_eq!(report_filename("  ", 0), "interview-1970-01-01-0000.md");
        assert_eq!(utc_parts(951_782_400_000), (2000, 2, 29, 0, 0), "leap day");
    }

    #[test]
    fn reports_are_saved_privately_without_overwriting() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let dir = reports_dir(&workspace.path.join("settings.json"));
        let first = save_report(&dir, "interview-x.md", "# Report").unwrap();
        let second = save_report(&dir, "interview-x.md", "# Report 2").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(&first).unwrap(), "# Report");
        assert!(second.ends_with("interview-x-1.md"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&first).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(save_report(&dir, "y.md", "  ").is_err());
    }
}
