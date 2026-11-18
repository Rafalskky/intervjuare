# profile.yaml

`profile.yaml` is the single, human-edited source of truth for the candidate's
real experience. It is ignored by Git; start from `profile.example.yaml`.

Location: `INTERVJUARE_PROFILE` if set, otherwise `profile.yaml` in the
repository root the app was built from. Another path can be loaded from the
app ("Candidate profile" → Load profile).

## What the app checks

`src-tauri/src/profile.rs` validates the file and never rewrites it:

- must be UTF-8, ≤ 256 KiB, exactly one YAML document with a mapping at the
  top level (otherwise the profile is rejected and answers make no personal
  claims);
- warnings (still usable): missing `candidate`, `projects`/`stories` that are
  not mappings, a story that is not a mapping or has none of
  `situation`/`problem`/`action`/`result`, no stories at all, an id used in
  two sections.

Hermes receives the raw file unchanged. Keys under `projects`, `stories`,
`experience`, `employment` and `education` are **ids**: suggestions cite them in
`SOURCES`, and a cited id that does not exist is flagged in the UI.

## Writing good entries

- Write what actually happened. The same story will be framed differently for
  frontend, backend, design or product roles; the facts must stay fixed.
- Give each story `situation`, `problem`, `action`, `result` (and `testing`,
  `lessons` when they exist). Concrete details make better speaking anchors.
- Use `tags` for themes (initiative, debugging, testing, conflict…) so the
  right story is easier to pick for behavioral questions.
- Use stable, readable ids (`password_reset_race_condition`), since they are
  shown as sources.

The schema may evolve; unknown sections are passed to Hermes as-is.
