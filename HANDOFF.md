# Handoff: Intervjuare (state as of 2026-11-18, evening)

Read `README.md` (product) and `AGENTS.md` (implementation rules) first. This
file is the current state, the owner's decisions, and what to do next.

## Owner decisions that override earlier docs

1. **No Hermes at run time.** The app calls the ChatGPT Codex backend directly
   (`chatgpt.com/backend-api/codex/responses`) with the ChatGPT sign-in already
   on the Mac (Codex app / `codex login`; a Hermes-stored copy of the same login
   is read only as a fallback). See `docs/model-runtime.md`.
2. **Always the chosen account.** The account e-mail is in local settings. Never
   fall back to another signed-in account; never refresh or rewrite another
   tool's OAuth tokens; never log, show or pass tokens in argv.
3. The repository is **public** (MIT). `profile.yaml`, models, build output and
   anything personal stay out of Git. Do not write account e-mails or
   identities into tracked files.
4. The owner works in Polish; code, docs and commits stay in English.
5. The owner asked to continue past Milestone 1 before doing its live test.

## Milestones

| Milestone | State | Verified how |
|---|---|---|
| 1 Audio + speaker-separated transcription | Implemented | Unit tests; synthetic `say` speech through the bundled model: Swedish 0–12 % WER (proper nouns such as "Sagobo" → "Sago Bo"), English 0 %. **Live mic + meeting test by the owner still pending.** |
| 2 Conversation state, profile, job, anchors | Done | Unit + mock-server tests; live model: first anchor ≈ 1.0–1.8 s |
| 3 Streaming answer + answer modes + "other example" | Done | Unit tests; live: STAR + other example switched to a different real story |
| 4 Interview memory + stories already used | Done | Unit tests; live memory update ≈ 4.6 s, accurate |
| 5 "Låt oss se" / screen assistance | Implemented | Detection and request path unit-tested; Swift command compiles and returns the typed permission error. **Real capture not verified live.** |
| 6 Post-interview analysis | Done (part 1) | Unit tests; live Polish report in ≈ 28 s, grounded |
| 6 Interview history (SQLite) | Done (part 2) | Store, writer thread, schema versioning, disabled/enabled, delete and restart-then-analyse covered by tests. **UI clicked through only by the owner so far.** |
| 6 Web search, follow-up prediction | Not started | |

Branches: `milestone-2-hermes-anchors` (name outdated; Milestones 2–6 part 1),
`milestone-6-next` (adds history; work continues here in the git worktree
`~/.t3/worktrees/Intervjuare/milestone-6-next`). Nothing is merged; `main` =
Milestone 1 import.

## Owner checklist (what the owner still has to do)

1. Choose the ChatGPT account in the app (ChatGPT → Account → Save → Check
   connection). The e-mail the owner first named has **no sign-in on this
   Mac**: either sign in to the Codex app with it, or choose the signed-in
   Plus account.
2. `cp profile.example.yaml profile.yaml`, fill in real experience, then Load
   profile.
3. Fill in Job (company, role, topics, description) → Save job context.
4. Grant Microphone and Screen & System Audio Recording to the signed bundle
   on the first Start.
5. Test without audio (typed turns), then "Låt oss se" and Capture screen.
6. Milestone 1 live test: meeting app + headphones, Swedish and English,
   candidate-only, interviewer-only, both.
7. After a test interview: Analyse interview → read the report. Open
   **History**, check the interview is there, then quit and restart the app and
   use **Analyse again** on it. Turn "Save interviews to local history" off if
   you do not want interviews kept.
8. Decide whether to merge the branch into `main`.

## Build and test

```bash
export INTERVJUARE_CODESIGN_IDENTITY="<identity from: security find-identity -v -p codesigning>"
(cd src-tauri && cargo test --lib)     # 169 tests; build.rs fails closed without the identity
npm test && npx tsc -b && npm run lint # 39 TS tests
npm run test:signing                   # 45 signing tests
npm run bundle:stable && npm run bundle:verify
```

The bundle is `src-tauri/target/debug/bundle/macos/Intervjuare.app`. Keep the
same path and identity, or macOS asks for permissions again. `cargo clippy`
and `cargo fmt` are not installed in this toolchain.

### Working in a git worktree

Gitignored inputs do not exist in a fresh worktree. From the worktree root,
with `MAIN` = the main checkout:

```bash
npm install
ln -s "$MAIN/assets/models"/*.bin assets/models/          # ~574 MB model, not in Git
ln -s "$MAIN/vendor/whisper.cpp/build" vendor/whisper.cpp/build   # build.rs resolves the link (rpaths name the real path)
cp "$MAIN/profile.yaml" . 2>/dev/null || true             # if the owner created one
```

## Code map (Rust core, `src-tauri/src/`)

- `lib.rs`: Milestone 1 audio pipeline (unchanged apart from two hooks:
  every emitted transcript event goes to `copilot::on_transcript`, and Start
  records the meeting app for screenshots).
- `conversation.rs`: turns from transcript events (revisions, finals, ordering).
- `context.rs`: context package, system rules, answer modes, prompt-cache key.
- `codex_auth.rs`: read-only discovery and exact selection of ChatGPT sign-ins.
- `llm.rs`: streaming Responses client over `/usr/bin/curl`; test SSE server.
- `suggestion.rs`: progressive ANCHORS / ANSWER / SOURCES parser.
- `memory.rs`: background interview memory.
- `voice_command.rs`: "Låt oss se" / "Let's see" detection.
- `screen.rs` + Swift `screenshot` command in `native/macos/.../main.swift`.
- `analysis.rs`: post-interview report and saving.
- `history.rs`: SQLite history (`history.sqlite`), single writer thread, `PRAGMA user_version` schema.
- `settings.rs`: `~/Library/Application Support/com.rafal.intervjuare/settings.json`.
- `copilot.rs`: orchestration, trigger rules, Tauri commands, events
  (`suggestion-event`, `memory-event`, `voice-command`, `analysis-event`).

UI (`src/`): `App.tsx` (audio setup + layout, Interview/History tabs),
`CopilotSetup.tsx`, `SuggestionPanel.tsx`, `AnalysisPanel.tsx`,
`HistoryView.tsx`, `useCopilot.ts`,
`suggestionView.ts` (pure, tested).

Docs: `docs/model-runtime.md`, `docs/context-management.md`,
`docs/screenshot.md`, `docs/post-interview.md`, `docs/history.md`, `docs/profile-format.md`,
`docs/audio-pipeline.md`.

## Known issues and gotchas

- The Codex app token is short-lived (≈ 1 h) and has no workspace id; while
  Codex is closed, the Hermes copy of the same login is used. If both expire,
  opening the Codex app once refreshes it. The app must never refresh tokens
  itself.
- A second, apparently unrelated account exists in the Hermes credential pool.
  Never select it automatically.
- The model does not always emit `NO_VERIFIED_EXAMPLE`, although answers stayed
  honest in tests.
- Proper nouns from the profile are sometimes split by Whisper. A Whisper
  vocabulary prompt built from profile ids/skills could help, but it touches
  the Milestone 1 STT path, which deliberately sends an empty prompt and
  `--no-context`. Measure before changing it.
- History stores final segments only, so a segment that never reached its
  final (for example app killed mid-sentence) is not in history.

## Suggested next steps

1. Owner checklist above, especially the Milestone 1 live test and the account
   choice.
2. Rest of Milestone 6: follow-up prediction (prepare likely follow-ups while
   the candidate answers, off the critical path) and web search only when
   needed (never delaying anchors).
3. Whisper vocabulary prompt from the profile (measure WER and latency first).
4. Keyboard shortcuts for Force / Shorter / Other example / Capture screen.
