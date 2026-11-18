# Model runtime: ChatGPT subscription

*Owner decision 2026-11-18: no Hermes. The app calls the model directly.*

Intervjuare sends one streaming request per interviewer turn to the backend
Codex uses (`https://chatgpt.com/backend-api/codex/responses`, OpenAI Responses
API format). Usage counts against the owner's ChatGPT subscription; there is no
API key and no per-token billing.

Code: `src-tauri/src/codex_auth.rs` (sign-in), `src-tauri/src/llm.rs` (client).

## Sign-in

The app does not run its own login. It reads, **read-only**, the ChatGPT
sign-ins already on this Mac:

| Source | File | Priority |
|---|---|---|
| Codex app / `codex login` | `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) | preferred while valid |
| Hermes copy (if Hermes is installed) | `$HERMES_HOME/auth.json` (default `~/.hermes/auth.json`) | fallback only |

Hermes does not have to run; only its stored copy of the same ChatGPT login
may be read as a fallback.

- The owner picks the **account e-mail** in the app ("ChatGPT → Account"). The
  choice is saved. If no sign-in exists for that e-mail, the app says so and
  lists the accounts it found. It never falls back to another account.
- Tokens are re-read for every request, so when Codex (or Hermes) refreshes a
  login, the app uses the new token immediately.
- The app never refreshes or rewrites these files. OpenAI rotates refresh
  tokens, so refreshing someone else's token would sign that tool out. When a
  token has expired, open the Codex app (or run `codex`) once and it refreshes.
- Tokens never appear in logs, process arguments, the UI or the repository.
  They go to curl on stdin as a config line.

The app identifies itself as `originator: intervjuare`,
`User-Agent: Intervjuare/<version>`. Personal logins without a workspace id are
accepted; the `ChatGPT-Account-ID` header is sent only when the token has one.

## Settings

Saved in `~/Library/Application Support/com.rafal.intervjuare/settings.json`
(owner-only, atomic writes): account e-mail, answer model, reasoning, model for
"Låt oss se", profile path, job context, auto-answer and default answer style.

`INTERVJUARE_SETTINGS` and `INTERVJUARE_CODEX_BASE_URL` exist for tests only.

## Request

- `instructions`: fixed rules (`context::SYSTEM_RULES`): grounding, references,
  story reuse, screen use, language/style and output format.
- `input`: one user message with `JOB CONTEXT`, `CANDIDATE PROFILE`,
  `INTERVIEW MEMORY`, `STORIES ALREADY USED`, `RECENT CONVERSATION`,
  `CURRENT INTERVIEWER TURN`, optional `SCREEN CONTEXT` + the screenshot as
  `input_image`, then `TASK` (language, answer mode, alternative example).
- `prompt_cache_key`: hash of rules + job + profile, so the static prefix is
  cached across turns.
- `reasoning.effort`: `none` by default; `store: false`; `stream: true`.

Body files live in an owner-only temporary directory that is deleted when the
request ends. `Expect:` is cleared so curl does not wait up to 1 s for
`100 Continue`.

## Response contract

Plain text, parsed progressively (`src-tauri/src/suggestion.rs`):

```text
ANCHORS
- 3–5 anchors of 2–6 words
ANSWER
spoken answer
NO_VERIFIED_EXAMPLE   (only when personal experience was asked for and is missing)
SOURCES: profile ids used, or none
```

or only `SKIP`. Anchors appear as soon as each line is complete; the answer
streams; markers never flash into the visible text. `SOURCES` ids are checked
against `profile.yaml` and unknown ids are shown as a red warning.

Errors become a visible failed suggestion, and transcription keeps running:
expired or invalid sign-in (HTTP 401/403), usage limit (429), no network, a
stream that ends without `response.completed`.

## Measured latency (2026-11-18, Plus account, Swedish backend questions)

Direct streaming, real context package (example profile):

| model | first anchor | answer starts | total |
|---|---|---|---|
| `gpt-6-luna`, reasoning none (default) | 1.0–1.8 s | 1.6–2.3 s | 2.6–4.4 s |
| `gpt-5.6-luna`, none | 1.3 s | 2.0 s | 4.6 s |
| `gpt-reserve`, none | 1.5 s | 2.0 s | 4.5 s |
| `gpt-6.2-sol`, low (does not accept none) | 1.9 s | 3.1 s | 6.8 s |

For comparison, the earlier Hermes route took 7.4–10.2 s for a whole answer.
Measured from request start; add the STT finalization time (≈ 0.85 s silence +
inference) for end of speech to first anchor. The app shows both per
suggestion.

Observed quality with `gpt-6-luna`: follow-up "Hur testade du det?" resolved
to the password-reset story; "Har du jobbat med Java?" answered honestly
without inventing experience (the model did not always add the
`NO_VERIFIED_EXAMPLE` line); filler ("Okej, jag ska bara anteckna…") returned
`SKIP` in about 1 s; "other example + STAR" switched to a different real story.

## Privacy

- The context package (profile, job, recent transcript, memory, and a
  screenshot only for "Låt oss se") is sent to OpenAI under the owner's ChatGPT
  account. That is inherent to using a cloud model.
- `store: false` is set on every request.
- Intervjuare persists only `settings.json`. Transcripts, suggestions, memory
  and screenshots stay in memory and are gone when the app quits.
