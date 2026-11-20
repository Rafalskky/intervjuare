# Post-interview analysis (Milestone 6, part 1)

Code: `src-tauri/src/analysis.rs`, `copilot.rs` (`analyze_interview`,
`cancel_analysis`, `save_analysis_report`, `set_report_settings`),
`src/AnalysisPanel.tsx`.

## What it does

"After the interview → Analyse interview" sends one request with:

- the retained transcript (≤ 40,000 characters, newest kept; older turns are
  represented by interview memory),
- interview memory, stories suggested during the interview,
- job context (with interview language) and the full `profile.yaml`.

The model streams a Markdown report in the chosen language (Polish by
default; Swedish and English available) with these sections: summary,
questions asked, strong answers, weak answers (with a better spoken version in
the interview language), difficult questions, stories (worked / overused /
unused but relevant), technical areas to review, likely follow-up questions,
suggested additions to `profile.yaml`, practice questions.

Rules in the instructions: every statement must come from the transcript,
memory, job or profile; recognition errors are not criticised; profile
suggestions only cover facts the candidate actually said, as YAML snippets
marked "to verify".

## What it never does

- It never edits `profile.yaml` (README: the user decides).
- It never runs during the interview path.
- With history on (default, see `docs/history.md`), a finished report is
  stored with its interview in the local history database. **Save as
  Markdown** additionally exports a file to
  `~/Library/Application Support/com.rafal.intervjuare/reports/interview-YYYY-MM-DD-HHMM-<company>.md`
  (UTC, owner-only `0600`, never overwritten).

## Measured (2026-11-18, `gpt-6-luna`, reasoning none, short Swedish interview)

First text after 2.0 s; full report (~4,900 characters, Polish) in 27.6 s. The
report flagged the honest Java gap, gave better Swedish phrasings grounded in
the profile, ignored the recognition error "Sago Bo", and turned "Docker",
mentioned in the interview but missing from the profile, into a suggestion
marked "to verify".

## Saved interviews

Interviews in **History** can be analysed (again) at any time, including after
a restart. The report uses the stored transcript, memory, suggestions and job
context together with the current `profile.yaml`, and replaces the stored
report for that interview.
