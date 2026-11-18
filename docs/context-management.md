# Conversation state, context and memory

Code: `src-tauri/src/conversation.rs`, `context.rs`, `memory.rs`,
`voice_command.rs`, `copilot.rs`.

## Conversation state

Every `transcript-event` the STT dispatcher emits is also applied, in Rust, to
one `Conversation`:

- Segments are keyed by `segmentId`. A higher revision replaces the text of a
  partial; a final is immutable; stale revisions are ignored; an empty final
  keeps the last valid hypothesis; a segment can never change speaker.
- Segments are ordered by captured start time, and consecutive segments from
  the same speaker form one **turn**. An interviewer question split by a pause
  is one turn.
- A transcript event from a new `sessionId`, Start, or "Clear conversation"
  starts a new interview: conversation, memory, stories used and pending work
  are reset.
- At most 240 segments are retained.

Typed test turns ("Test without audio") become synthetic final segments in
the current session and take exactly the same path.

## When a suggestion is requested

| Event | Effect |
|---|---|
| Interviewer final that completes the latest interviewer turn | New suggestion (if auto-answer is on) |
| …but the whole turn is only a backchannel (`Mm.`, `Okej, precis.`, `Yeah`) | No request. Deterministic list of non-content tokens only; `Varför?` still triggers |
| Interviewer partial that continues the turn being answered | Cancel it; the next final asks again with the full turn |
| Interviewer partial in a new turn (after the candidate spoke) | Keep the current suggestion; the UI dims it |
| Candidate speech | Never cancels |
| Candidate says only **"Låt oss se" / "Let's see"** | Screen-aware suggestion (see `docs/screenshot.md`); the command is removed from the dialogue |
| Newer suggestion starts | Older one is cancelled |
| **Force answer** / **Shorter, STAR, Technical, Simpler, Explain question** | Suggestion for the latest interviewer turn with that one-off style |
| **Other example** | Same, but the answer must not rely on the sources of the last finished suggestion |

Whether the interviewer actually expects an answer is decided by the model in
the same request (`SKIP`), not by a separate classification request. On `SKIP`
the UI restores the previous useful suggestion.

## Context package

Built per request by `context::build_context`:

```text
SYSTEM RULES              (instructions)
JOB CONTEXT               company, role, topics, description (≤ 6,000 chars)   ┐ static prefix,
CANDIDATE PROFILE         full raw profile.yaml (≤ 256 KiB)                    ┘ prompt-cached
INTERVIEW MEMORY          compact YAML summary (≤ 4,000 chars), when present
STORIES ALREADY USED      profile ids suggested earlier + the question they answered
RECENT CONVERSATION       last 12 turns before the current one, ≤ 4,000 chars, newest kept
CURRENT INTERVIEWER TURN
SCREEN CONTEXT            only for "Låt oss se" (+ screenshot image)
TASK                      language; answer mode; alternative-example constraint
```

## Answer modes (Milestone 3)

`normal` (20–40 s), `short` (1–2 sentences), `star` (situation, task, action,
result without labels), `technical`, `simple` (simple Swedish/English),
`explain` (what the interviewer wants and how to approach it). The default is
saved in settings; the buttons under a suggestion regenerate once in another
mode without changing the default.

## Interview memory and story reuse (Milestone 4)

Two complementary mechanisms:

1. **Stories already used** (deterministic, no extra request): when a
   suggestion finishes, its verified `SOURCES` ids are recorded with the
   question. Later prompts list them, and the rules tell the model to prefer an
   unused example unless the question follows up on that one.
2. **Interview memory** (background model request): when a new suggestion
   starts and at least 4 complete turns are not yet in memory, one background
   request merges them into a short YAML memory: language, current topic,
   topics discussed, stories actually told, interviewer focus, candidate
   claims, open questions. It runs in parallel and never delays anchors. There
   is one job at a time, and results that arrive after a reset are discarded.
   The current question is never absorbed early.

Recent turns stay verbatim and overlap with memory on purpose: memory may lag
a turn or two behind, and the newest turns are always in RECENT CONVERSATION.
The memory is visible in the app ("Interview memory").

Measured: memory update ≈ 4.6 s in the background (`gpt-6-luna`); the output
correctly listed stories used and candidate claims.

## Not yet implemented

- Persistence of transcripts/suggestions/history (SQLite) and post-interview
  analysis (Milestone 6).
- Web search and follow-up prediction (Milestone 6).
- Keyboard shortcuts.
