# AGENTS.md — Intervjuare

This file defines how coding agents should work on **Intervjuare**.

Read `README.md` before making architectural or product decisions. `README.md` is the source of truth for product behavior and scope. This file defines implementation discipline.

---

## 1. Project Intent

Intervjuare is a **private, personal, macOS-only AI interview copilot**.

The application listens to online interviews, transcribes candidate and interviewer audio separately, maintains interview context, sends relevant context to the model, and displays fast speaking anchors followed by a streaming full answer.

The project is not a SaaS product and should not be engineered as one.

Do not add product requirements that are not present in `README.md`.

---

## 2. Hard Constraints

These constraints are intentional and must be preserved unless the owner explicitly changes them.

### Platform

- macOS only.
- Do not add Windows support.
- Do not add Linux support.
- Do not add mobile support.
- Do not add a browser-only application.

### AI runtime

- Runtime AI is the owner's **ChatGPT subscription**, called directly through the Codex backend (`chatgpt.com/backend-api/codex`) with the ChatGPT sign-in that already exists on the Mac. *(Owner decision 2026-11-18; this replaced Hermes.)*
- Do not require Hermes or another agent runtime at run time.
- Do not switch to a paid API-key provider (OpenAI API, Anthropic API, Gemini API, …) unless explicitly requested.
- Use only the account the owner chose. Never fall back to another account, never refresh, rewrite or copy another tool's OAuth tokens, and never log, display or pass tokens in process arguments.
- Do not build or require a large local LLM.

### Candidate data

- `profile.yaml` is the canonical candidate profile.
- Personal claims generated for the candidate must be grounded in `profile.yaml`.
- Never invent employment, projects, technologies, achievements, responsibilities, or results.
- The same real experience may be framed differently depending on the job.

### Scope

Do not add:

- accounts;
- login systems;
- payments;
- subscriptions;
- cloud sync;
- multi-user architecture;
- teams/organizations;
- quota dashboards;
- usage analytics;
- telemetry;
- plugin marketplaces;
- unrelated onboarding flows.

This is a personal tool. Prefer simple local architecture.

---

## 3. Primary Technology Direction

Unless there is a strong technical reason to change it, use:

```text
UI              React + TypeScript + Tailwind CSS
Desktop shell   Tauri 2
Core            Rust
macOS bridge    Swift
STT             whisper.cpp
Storage         SQLite
Profile         YAML
Runtime AI      ChatGPT subscription (Codex backend)
```

A proposed change to one of these core technologies must include a concrete reason and trade-off analysis before implementation.

Do not casually rewrite the stack.

---

## 4. Architectural Responsibilities

Keep responsibilities separated.

### React / TypeScript

Responsible for:

- interview UI;
- setup UI;
- profile UI when needed;
- history UI;
- state presentation;
- rendering streaming anchors and answers;
- user interactions.

Do not put audio capture, STT engines, SQLite internals, or native macOS logic in React.

### Rust / Tauri core

Responsible for:

- application orchestration;
- event routing;
- interview state;
- transcript buffers;
- context assembly;
- model communication;
- persistence coordination;
- commands exposed to the UI;
- cancellation and lifecycle management.

### Swift / native macOS

Responsible only for macOS-specific functionality that is better handled natively, such as:

- system audio capture;
- microphone integration when necessary;
- screen capture;
- permissions;
- native window behavior when required.

Keep the Swift layer narrow.

### whisper.cpp

Responsible for local speech-to-text.

Do not mix AI reasoning with STT.

### Model (ChatGPT)

Responsible for semantic reasoning:

- understanding what the interviewer wants;
- understanding follow-up context;
- selecting relevant candidate experience;
- choosing the right angle for the role;
- generating speaking anchors;
- generating the full answer;
- understanding screenshots;
- using web information when needed;
- maintaining or updating compact interview memory;
- preparing likely follow-ups.

Do not recreate these abilities with brittle local heuristics unless the task is explicitly deterministic.

---

## 5. Performance Is a Product Requirement

The target user experience is:

```text
< 1 second
useful speaking anchors begin appearing

< 2 seconds
useful answer text has started streaming
```

Do not introduce blocking work into the critical answer path without measuring it.

When implementing a feature that touches the live interview pipeline:

1. identify whether it runs on the critical path;
2. avoid unnecessary serialization;
3. use streaming where possible;
4. support cancellation;
5. avoid waiting for work that can happen in parallel;
6. measure end-to-end latency.

Prefer:

```text
FAST + GOOD
```

over:

```text
PERFECT + LATE
```

---

## 6. Progressive Response Contract

The AI response must prioritize useful content in this order:

```text
1. speaking anchors
2. full answer
```

Do not design a flow where the application waits for the complete answer before showing anything.

The UI should be able to render anchors as soon as they arrive and stream the answer underneath.

A suitable logical response contract may contain fields such as:

```ts
interface InterviewSuggestion {
  question: string;
  anchors: string[];
  answer: string;
  profileSources?: string[];
  needsWeb?: boolean;
  needsScreen?: boolean;
}
```

The exact schema may evolve, but progressive delivery is mandatory.

---

## 7. Context Is More Important Than the Last Question

Never reduce the AI request to only the latest interviewer sentence.

The model should receive the relevant combination of:

```text
system rules
job context
candidate profile
compact interview memory
recent speaker-aware turns
current interviewer turn
screen context when relevant
```

Do not continuously resend an ever-growing raw transcript if a compact interview memory can represent older context.

Recent conversation and long-term interview memory are separate concepts.

---

## 8. Conversation State

Maintain structured state for at least:

```text
current language
current topic
topics discussed
stories used
interviewer focus
candidate claims made
recent turns
current interviewer turn
current candidate turn
```

Follow-up questions must retain context.

For example:

```text
"Hur testade du det?"
```

must resolve `det` against the prior discussion instead of being treated as a standalone unrelated question.

---

## 9. Audio Rules

Online interviews are the primary use case.

Keep audio sources separate:

```text
microphone     → candidate
system audio   → interviewer
```

Do not add speaker diarization if source separation already provides reliable speaker identity.

Audio and transcript processing must support:

- partial segments;
- final segments;
- revisions;
- Swedish;
- English;
- low latency;
- interruption handling.

Do not assume STT output is immutable before a segment is final.

---

## 10. Explicit Voice Commands

The first required voice commands are:

```text
Låt oss se
Let's see
```

These are commands, not normal interview content.

They should trigger the screenshot-assistance flow and should not be inserted into the candidate/interviewer conversation transcript sent as ordinary dialogue.

Do not send these commands to the model merely to decide whether they are commands when a deterministic local matcher is sufficient.

---

## 11. Screenshot Context

Screenshot assistance is part of the product.

Screen context may be needed for:

- coding tasks;
- code review;
- architecture diagrams;
- whiteboards;
- errors;
- technical diagrams;
- questions displayed on screen.

Screenshots should be treated as temporary context by default.

Do not automatically persist screenshots unless the owner explicitly asks for that behavior.

When screen context is supplied to the model, combine it with the relevant interview context rather than treating the image as an isolated request.

---

## 12. Internet Use

The model may use current web information when needed.

Do not put web search into every answer path.

Use it when the question genuinely depends on current information, for example:

- current library/framework versions;
- current company information;
- recent technical changes;
- current product information;
- time-sensitive facts.

Initial speaking anchors should not be unnecessarily delayed by optional web retrieval.

---

## 13. Privacy

Intervjuare is a private personal tool.

Default expectations:

- no analytics;
- no telemetry;
- candidate profile remains local;
- local settings remain local;
- raw audio is not permanently stored by default;
- screenshots are not permanently stored by default;
- transcripts remain local unless explicitly configured otherwise;
- do not add unrelated data collection.

The README contains the product-level visibility requirement. Preserve that requirement without silently weakening or reinterpreting it.

---

## 14. Storage

Use SQLite for local structured application data unless a simpler representation is clearly better for a specific feature.

`profile.yaml` remains outside the database as the human-editable candidate source of truth.

Potential SQLite data includes:

- interview metadata;
- transcript segments;
- interview memory;
- questions;
- candidate answers;
- AI suggestions;
- stories used;
- job context;
- local settings.

Do not copy `profile.yaml` into multiple competing sources of truth.

---

## 15. Coding Style

General rules:

- prefer small modules with clear responsibility;
- prefer typed interfaces over unstructured maps;
- avoid global mutable state;
- keep platform-specific code isolated;
- make long-running operations cancellable;
- do not swallow errors silently;
- use structured logging during development;
- never log sensitive profile or transcript data unnecessarily;
- keep comments focused on why, not obvious syntax;
- remove dead code instead of leaving speculative paths around;
- avoid abstraction until there is a real boundary worth abstracting.

This project values clarity over cleverness.

---

## 16. Testing Expectations

Add tests where they provide real value.

Prioritize tests for:

- transcript revision handling;
- speaker/source assignment;
- conversation-state transitions;
- context assembly;
- interview memory updates;
- profile loading and validation;
- grounding behavior;
- progressive response parsing;
- interruption/cancellation behavior;
- explicit voice-command detection.

Do not create large amounts of meaningless snapshot testing simply to increase coverage.

For latency-sensitive work, add lightweight timing instrumentation or benchmarks where useful.

---

## 17. Error Handling

Live interviews require graceful degradation.

Examples:

### Model unavailable (offline, expired sign-in, usage limit)

The application should remain stable and continue transcription if possible.

### STT failure

Surface the failure clearly without crashing the entire app.

### Screen capture permission missing

Disable screenshot assistance and explain the missing permission.

### System audio unavailable

Do not silently label all audio as interviewer input.

### Streaming response cancelled

Stop rendering obsolete content cleanly.

Do not turn recoverable failures into application-wide crashes.

---

## 18. Do Not Over-Engineer Retrieval

For the first version, sending the complete `profile.yaml` to the model is acceptable.

Do not introduce:

- vector databases;
- embedding pipelines;
- semantic indexes;
- local reranking models;

unless profiling shows that the profile has become too large or latency/context limits require it.

Simple first.

---

## 19. Do Not Add a Separate AI Classification Request

Avoid workflows such as:

```text
request 1 → classify question
request 2 → choose story
request 3 → generate answer
```

unless measurements prove that separation is necessary.

Prefer a single model request that can understand the context, select the relevant experience, emit anchors first, and then stream the full answer.

---

## 20. Milestone Discipline

Implement the project in the roadmap order from `README.md`.

Do not jump ahead because a later feature seems more interesting.

### Milestone 1

Audio capture + reliable speaker-separated live transcription.

Do not implement live AI answering until this works well.

### Milestone 2

Model integration + speaking anchors.

### Milestone 3

Streaming full answer.

### Milestone 4

Interview memory and story reuse awareness.

### Milestone 5

`Låt oss se` / screenshot assistance.

### Milestone 6

Web use, follow-up preparation, post-interview analysis.

When starting a milestone:

1. read the relevant README section;
2. inspect existing architecture;
3. make the smallest coherent implementation;
4. test it;
5. report what changed;
6. report known limitations;
7. do not silently begin the next milestone.

---

## 21. Before Editing Code

Before a non-trivial change:

1. inspect the relevant files;
2. understand the existing event/data flow;
3. identify the minimal set of files that must change;
4. preserve existing working behavior;
5. check whether the change affects the latency-critical path.

Do not rewrite unrelated areas while implementing a feature.

---

## 22. After Editing Code

Before considering a task finished:

- run relevant tests;
- run type checking;
- run linting if configured;
- compile/build affected native components when practical;
- verify no obvious dead imports or warnings were introduced;
- verify the app still starts;
- state anything that could not be verified.

For live pipeline changes, also verify that streaming/cancellation still behaves correctly.

---

## 23. Dependency Rules

Before adding a dependency:

- check whether the platform or existing stack already provides the capability;
- prefer maintained dependencies;
- avoid heavy dependencies for trivial tasks;
- avoid adding multiple libraries that solve the same problem;
- explain unusual native dependencies.

Do not introduce a framework merely to save a few lines of code.

---

## 24. Documentation Rules

When behavior or architecture changes materially, update the relevant documentation.

Relevant docs may include:

```text
README.md
docs/architecture.md
docs/audio-pipeline.md
docs/model-runtime.md
docs/context-management.md
docs/profile-format.md
docs/latency.md
docs/privacy.md
docs/screenshot.md
```

Do not let implementation and README silently diverge.

---

## 25. Decision Priority

When requirements conflict, use this priority order:

```text
1. correctness / truthful candidate grounding
2. interview responsiveness and latency
3. reliability
4. privacy
5. simplicity
6. maintainability
7. visual polish
8. speculative future extensibility
```

Do not sacrifice the live interview experience for an architecture intended for hypothetical future users.

---

## 26. Definition of a Good Change

A good Intervjuare change should usually make at least one of these better without unnecessarily harming the others:

```text
faster response
more accurate transcript
better conversation context
better candidate grounding
more natural answers
less cognitive load
more reliable live behavior
simpler architecture
stronger privacy
```

If a change adds complexity without improving the actual interview experience, reconsider it.

---

## 27. Final Rule

Intervjuare should feel like a quiet extension of the candidate's own memory and communication ability.

The system should help the candidate say what they already know clearly, quickly, and truthfully.

When uncertain about implementation choices, prefer the option that is:

```text
simpler
faster
more local
more reliable
less distracting
```

while remaining consistent with `README.md`.
