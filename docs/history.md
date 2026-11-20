# Interview history (Milestone 6, part 2)

Code: `src-tauri/src/history.rs` (store + writer thread), `copilot.rs`
(recording, `list_interviews`, `get_interview`, `delete_interview`,
`analyze_saved_interview`, `set_history_enabled`), `src/HistoryView.tsx`.

## What is stored

`~/Library/Application Support/com.rafal.intervjuare/history.sqlite`
(directory `0700`, database `0600`; SQLite gives its journal files the same
permissions as the database):

| Table | Contents |
|---|---|
| `interviews` | start and last activity time, company, role, interview language, full job context (JSON), interview memory, post-interview report (+ language, model, time) |
| `segments` | **final** transcript segments only: speaker, text, captured times, simulated flag |
| `suggestions` | **finished** suggestions: question, anchors, answer, verified sources, answer mode, screen flag, model |

Never stored: raw audio, screenshots, partial hypotheses, the "Låt oss se"
command, OAuth tokens, or a copy of `profile.yaml`. The profile stays the single
source of truth, and a report on a saved interview uses the profile as it is
when the report is generated.

An entry is created lazily on the first stored item, so starting capture
without speech leaves nothing behind. A new capture session or **Clear
conversation** starts a new entry. Typed test turns are stored with
`simulated = 1` and the list marks such interviews "simulated".

## On by default, and why

`historyEnabled` defaults to **on**:

- Post-interview analysis needs the transcript. With history off, an
  accidental quit or crash after an interview loses it for good.
- Everything stays on this Mac in owner-only files; nothing is uploaded or
  synced (README "Privacy": transcripts remain local).
- It is visible and reversible: the toggle "Save interviews to local history"
  is in the copilot panel, and every interview can be deleted from History.

Turning it off stops recording immediately (including the current interview).
Turning it back on starts a new entry; nothing said while it was off is
recorded. With history off, reports are not stored either. Deleting the
interview that is being recorded stops recording it.

## Not on the critical path

One writer thread owns the only SQLite connection. The live path (transcript
hook, suggestion completion, memory updates) only sends messages on an
unbounded channel, which never blocks, so disk I/O can never delay
transcription or suggestion streaming. UI reads go through the same thread
with a reply channel; the queue is FIFO, so a read sees every earlier write.
Write errors are kept and shown in the UI ("History: …") instead of being
swallowed.

## Schema versioning

`PRAGMA user_version` holds the schema version (currently 1). Opening creates
or migrates step by step (`if version < N`). A database from a **newer** build
is refused and left untouched, with a clear message.

## Dependency

`rusqlite` with the `bundled` feature compiles SQLite from source, so the
SQLite version and features are fixed and independent of the macOS system
library. The MSRV-aware resolver selected 0.40.x for the project's declared
`rust-version` (1.85). It adds a C compile step (≈ 20 s on first build) and
no runtime dependency.

## UI

**History** tab: list (newest first; company, role, date, duration, language,
counts, report and simulated badges), open an interview (report, transcript,
suggestions shown, memory), **Analyse interview / Analyse again** (works after
a restart; the new report replaces the stored one), **Delete** with a
confirmation step.
