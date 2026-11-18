# Hermes integration (Milestone 2)

Intervjuare talks to the local **Hermes Agent API server**: an OpenAI-compatible
endpoint that `hermes gateway` starts when it is enabled. Hermes routes the
request to whatever provider/model it is configured with (for example a ChatGPT
or Claude subscription), so no direct paid model API is used by the app.

## One-time setup

Add to `~/.hermes/.env` (or the `.env` of a dedicated Hermes profile):

```bash
API_SERVER_ENABLED=true
API_SERVER_KEY=<a long random local secret>
# optional, default 8642
# API_SERVER_PORT=8642
```

Restart the gateway (`hermes gateway restart`, or `hermes gateway run` in a
terminal), then press **Check Hermes connection** in the app.

The app reads `API_SERVER_KEY`, `API_SERVER_PORT` and `API_SERVER_HOST` straight
from the Hermes `.env` (`$HERMES_HOME/.env`, default `~/.hermes/.env`), so the
key is not duplicated. Overrides, mainly for development:

| Variable | Default | Meaning |
|---|---|---|
| `INTERVJUARE_HERMES_URL` | `http://127.0.0.1:<API_SERVER_PORT>/v1` | Base URL |
| `INTERVJUARE_HERMES_API_KEY` | `API_SERVER_KEY` | Bearer key |
| `INTERVJUARE_HERMES_MODEL` | `hermes-agent` | `model` field |
| `INTERVJUARE_HERMES_REASONING` | `none` | `model_options.reasoning_effort` |

A Finder-launched `.app` does not see shell environment variables; the `.env`
path is what works there.

## Request

One request per interviewer turn, no separate classification request
(AGENTS.md §19):

- `POST {base}/chat/completions`, `stream: true`
- `messages[0]` (system): the fixed rules in `src-tauri/src/context.rs`
  (`SYSTEM_RULES`): grounding, reference resolution, language/style and the
  output format. Hermes layers it on top of its own core prompt.
- `messages[1]` (user): `JOB CONTEXT`, `CANDIDATE PROFILE` (full raw
  `profile.yaml`), `RECENT CONVERSATION` (last 12 turns / 4,000 chars),
  `CURRENT INTERVIEWER TURN`, `TASK` (interview language).
- `model_options.reasoning_effort`: `none` by default (latency first).

Transport is `/usr/bin/curl` (the same approach as the STT path, no HTTP
dependency). The key is passed on curl's stdin as a config line, never in
argv. The JSON body sits in an owner-only (`0700`) temporary directory that is
deleted when the request ends. `Expect:` is cleared so curl does not wait up to
1 s for `100 Continue` on bodies over 1 KiB.

## Response contract

Plain text, parsed progressively by `src-tauri/src/suggestion.rs`:

```text
ANCHORS
- 3–5 anchors of 2–6 words
ANSWER
spoken answer, ~20–40 s
SOURCES: profile ids used, or none
```

plus the optional line `NO_VERIFIED_EXAMPLE`, or only `SKIP` when the turn
needs no answer. Anchors are shown as soon as each line is complete; the answer
streams character by character and never shows a half-written marker. The
parser tolerates markdown decoration and a missing header.

`SOURCES` ids are checked against the ids in `profile.yaml` (keys under
`projects`, `stories`, `experience`, `employment`, `education`). Ids that do not
exist are shown as a red grounding warning instead of being trusted.

Hermes-specific stream frames (`event: hermes.tool.progress`,
`event: hermes.status`), `: keepalive` comments and `reasoning_content` deltas
are ignored. `finish_reason: "error"`/`"cancelled"` and HTTP errors become a
visible failed suggestion; transcription keeps running.

## Cancellation

Each suggestion has a cancel token. Cancelling kills curl; Hermes interrupts
the agent turn when the client disconnects. A newer suggestion always
supersedes an older one, and the UI ignores events from superseded ids.

## Measured latency (2026-11-18, owner's default Hermes model)

`hermes -z` one-shot (CLI process start + full non-streamed answer) with the
real context package (example profile, Swedish backend question):

| reasoning | total | format/grounding |
|---|---|---|
| `low` | 10.2 s | correct anchors, answer, `SOURCES: sagobo, password_reset_race_condition` |
| `none` | 7.4 s | same quality |

These are whole-answer times, not time-to-first-anchor, and include CLI start.
Streaming through the API server shows anchors before the answer finishes, but
the README target (< 1 s anchors, < 2 s answer) has **not** been measured or
met yet. The app shows per-suggestion timings (request → first anchor, request
→ first answer text, and end of interviewer speech → first anchor) to measure
it. If it is too slow, the biggest lever is the model Hermes routes to: a
dedicated Hermes profile (e.g. `hermes profile create intervjuare`) with a fast
model, minimal toolsets and its own API server port keeps the interview path
separate from the everyday assistant.

## Privacy notes

- The context package (profile, job, recent transcript) goes to Hermes and from
  there to the configured model provider. That is inherent to the design.
- Hermes stores API-server conversations in its own local session history. Use
  a dedicated Hermes profile if interview transcripts should not mix with the
  everyday assistant's history and memory.
- Intervjuare itself still persists nothing: no transcript, suggestion or
  profile copy is written to disk beyond the transient request file.
