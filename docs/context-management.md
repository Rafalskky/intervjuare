# Conversation state and context (Milestone 2)

Code: `src-tauri/src/conversation.rs`, `context.rs`, `copilot.rs`.

## Conversation state

Every `transcript-event` the STT dispatcher emits is also applied, in Rust, to
one `Conversation`:

- Segments are keyed by `segmentId`. A higher revision replaces the text of a
  partial; a final is immutable; stale revisions are ignored; an empty final
  keeps the last valid hypothesis; a segment can never change speaker.
- Segments are ordered by captured start time, and consecutive segments from
  the same speaker form one **turn**. An interviewer question split by a pause
  is one turn.
- A transcript event from a new `sessionId` starts a new conversation (a new
  capture session is a new interview). Starting capture also clears it.
- At most 240 segments are retained. Long-term context is interview memory
  (Milestone 4), not this buffer.

Typed test turns ("Test without audio") become synthetic final segments in
the current session and take exactly the same path.

## When a suggestion is requested

| Event | Effect |
|---|---|
| Interviewer final that completes the latest interviewer turn | New suggestion (if auto-answer is on) |
| …but the whole turn is only a backchannel (`Mm.`, `Okej, precis.`, `Yeah`) | No request. Deterministic list of non-content tokens only; `Varför?` still triggers |
| Interviewer partial that continues the turn being answered | Cancel it: the question was not finished. The next final asks again with the full turn |
| Interviewer partial in a new turn (after the candidate spoke) | Keep the current suggestion (the candidate is using it); the UI dims it |
| Candidate speech | Never cancels |
| Newer suggestion starts | Older one is cancelled |
| **Force answer** | Suggestion for the latest interviewer turn, even if incomplete |

Whether the interviewer actually expects an answer is decided by Hermes in the
same request (`SKIP`), not by a separate classification request. On `SKIP` the
UI restores the previous useful suggestion.

## Context package

Built per request by `context::build_context` in README order:

```text
SYSTEM RULES              (system message)
JOB CONTEXT               company, role, topics, description (≤ 6,000 chars)
CANDIDATE PROFILE         full raw profile.yaml (≤ 256 KiB)
RECENT CONVERSATION       last 12 turns before the current one, ≤ 4,000 chars, newest kept
CURRENT INTERVIEWER TURN
TASK                      interview language (sv / en)
```

`INTERVIEW MEMORY` (Milestone 4) and `SCREEN CONTEXT` (Milestone 5) are not
implemented yet. Follow-ups such as "Hur testade du det?" are resolved from
recent conversation only, so a reference to something older than the
recent-turn window is lost until interview memory exists.

## Not yet implemented

- Persistence (SQLite) of job context, transcript and suggestions; the job
  context lives in memory and must be re-applied after an app restart.
- Answer modes (short / STAR / technical / simple Swedish) and "alternative
  example".
- Stories-used tracking and interview memory (Milestone 4).
- Keyboard shortcuts.
