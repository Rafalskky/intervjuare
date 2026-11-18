# Intervjuare

**Intervjuare** is a private, personal macOS AI interview copilot.

It listens to online job interviews, continuously transcribes both sides of the conversation, understands the ongoing interview context, detects when the interviewer expects an answer, and helps the candidate formulate clear and truthful responses in Swedish or English.

The application is designed primarily for candidates who know what they want to say but may need help with wording, structure, recalling relevant experience, choosing the best example, speaking in a non-native language, answering technical questions, understanding something shown on screen, and keeping answers relevant to the position.

Intervjuare is not primarily a teleprompter. Its purpose is to act as:

```text
memory
+
language assistant
+
interview context assistant
+
technical assistant
+
real-time response copilot
```

The application is being developed exclusively as a private tool for its owner.

---

## Core Goal

During an interview:

```text
Interviewer speaks
        ↓
Audio capture
        ↓
Live transcription
        ↓
Conversation context
        ↓
The model understands what is happening
        ↓
Relevant candidate experience is selected
        ↓
Quick speaking anchors appear
        ↓
Full answer streams underneath
```

The candidate should be able to begin answering almost immediately using the speaking anchors while the full answer continues to generate.

Primary performance goal:

```text
< 1 second
useful speaking anchors begin appearing

< 2 seconds
a useful full answer has started streaming
```

Two seconds should be treated as the approximate maximum acceptable delay after the interviewer finishes asking a question.

---

## Core Principles

### 1. The Candidate Remains the Source of Truth

Intervjuare may organize experience, reformulate sentences, make Swedish or English sound more natural, choose the best example, explain technical concepts, connect existing experience to the current role, and highlight different aspects of the same project.

Intervjuare must not invent employment, projects, responsibilities, technologies used, achievements, education, personal experience, or results that never happened.

Personal claims must be grounded in the candidate profile.

If the candidate does not have a relevant personal example, Intervjuare should say so internally and provide the safest useful alternative, such as explaining the concept, suggesting how the candidate could approach the situation, or helping formulate an honest answer. It must not fabricate experience.

### 2. One Candidate Profile

There is only one canonical candidate profile:

```text
profile.yaml
```

The candidate's history does not change between interviews. What changes is the **angle** used to present that experience.

Example:

```text
PROJECT: Sagobo

Frontend role
→ React
→ components
→ state
→ performance
→ frontend architecture

Backend role
→ APIs
→ authentication
→ validation
→ race conditions
→ persistence

Designer role
→ onboarding
→ user flows
→ usability
→ information hierarchy
→ visual decisions

Product role
→ user problem
→ prioritization
→ monetization
→ product decisions
→ analytics
```

The same real experience can therefore support different types of positions without changing the underlying facts.

### 3. Job-Aware Answers

Before an interview, the user can provide information about the position.

Example:

```yaml
company: Example AB
role: Backend Developer
job_description: |
  Full job advertisement...
important_topics:
  - backend
  - APIs
  - Java
  - testing
  - architecture
language: sv
```

This information does not modify `profile.yaml`. It changes how the model interprets and prioritizes the candidate's existing experience.

Example:

```text
Question:
"Kan du berätta om ett problem du själv upptäckte?"

Backend position
→ password reset / race condition may be a strong example

Product-oriented position
→ analytics / adblocker example may be stronger

Design position
→ UX or Sagobo design decision may be stronger
```

### 4. Full Conversation Context Matters

Intervjuare must not send only the interviewer's latest question to the AI.

The AI should understand who is speaking, what has already been discussed, what examples have already been used, what the interviewer appears interested in, what role the candidate is interviewing for, what the candidate has already claimed, what the latest question refers to, and what language is currently being used.

A typical context package should conceptually contain:

```text
SYSTEM RULES
CURRENT JOB
CANDIDATE PROFILE
INTERVIEW MEMORY
RECENT CONVERSATION
CURRENT INTERVIEWER UTTERANCE
CURRENT SCREEN CONTEXT (if relevant)
TASK
```

The entire raw transcript does not need to be sent every time.

---

## Candidate Profile

The canonical candidate profile lives in:

```text
profile.yaml
```

Example structure:

```yaml
candidate:
  name: Rafael

languages:
  interview:
    - sv
    - en

skills:
  frontend:
    - React
    - TypeScript
    - Next.js
  testing:
    - Jest
    - React Testing Library

projects:
  sagobo:
    type: personal_project
    description: >
      Personalized children's story application.
    areas:
      frontend:
        - React
        - Next.js
      backend:
        - authentication
        - APIs
        - password reset
        - validation
      design:
        - user flows
        - onboarding
        - profile UX
      product:
        - monetization
        - positioning
        - conversion

stories:
  analytics_adblock:
    project: work
    tags:
      - initiative
      - analytics
      - problem_solving
      - privacy
      - business
    situation: >
      Google Analytics was being implemented on the website.
    problem: >
      Ad blockers could block analytics even for users
      who had consented to data collection.
    action: >
      A solution was designed to improve analytics
      reliability while continuing to respect user consent.
    result: >
      More reliable data for business decisions without
      collecting analytics from users who had declined tracking.

  password_reset_race_condition:
    project: sagobo
    tags:
      - backend
      - security
      - debugging
      - testing
      - initiative
    situation: >
      Password reset functionality was implemented.
    problem: >
      Multiple reset requests could be triggered quickly,
      including from multiple browser tabs.
    action: >
      Server-side protection was added so that repeated
      requests could not bypass the intended cooldown logic.
    testing: >
      Jest tests simulated repeated user actions
      and multiple requests.
    lessons:
      - server-side protection matters
      - client-side state is not sufficient
      - race conditions need to be considered
```

The exact schema may evolve. The important requirement is that candidate facts remain structured, human-readable, human-editable, and traceable.

---

## Application Architecture

Intervjuare targets:

```text
macOS only
```

There is no requirement for Windows, Linux, mobile, or a web application.

Recommended high-level architecture:

```text
┌─────────────────────────────────────────┐
│ UI                                      │
│ React + TypeScript + Tailwind           │
│                                         │
│ Interview overlay                       │
│ Setup                                   │
│ Profile                                 │
│ History                                 │
└───────────────────┬─────────────────────┘
                    │
                  Tauri
                    │
┌───────────────────▼─────────────────────┐
│ Application Core                        │
│                                         │
│ interview state                         │
│ transcript buffering                    │
│ context management                      │
│ storage                                 │
│ model communication                     │
│ event handling                          │
└─────────────┬───────────────┬───────────┘
              │               │
            Swift          whisper.cpp
              │
      macOS integration
```

### Recommended Technology

```text
Frontend
React
TypeScript
Tailwind CSS

Desktop
Tauri 2

Application core
Rust

macOS integration
Swift

Speech-to-text
whisper.cpp

Storage
SQLite

Candidate profile
YAML

Runtime AI
ChatGPT subscription (existing Codex sign-in)
```

---

## AI Runtime: ChatGPT Subscription

Intervjuare calls the language model directly. No separate agent runtime (such as Hermes) is required. *(Owner decision, 2026-11-18: Hermes was the original plan and has been removed.)*

```text
Intervjuare
     ↓
chatgpt.com/backend-api/codex   (Responses API, streaming)
     ↓
the owner's existing ChatGPT subscription
```

Authentication reuses the ChatGPT sign-in that already exists on the Mac: the Codex app / `codex login` (`~/.codex/auth.json`), with a Hermes-stored copy (`~/.hermes/auth.json`) read only as a fallback. The owner chooses which account e-mail is used. The app never falls back to another account, never refreshes or rewrites those tokens (that would sign the owning tool out) and re-reads them for every request. The default model is `gpt-6-luna` with reasoning `none`, the fastest measured option. See `docs/model-runtime.md`.

The model is responsible for the AI-heavy parts of the system, including understanding the current interview situation, understanding what the interviewer is asking, choosing the best candidate experience, deciding how to frame that experience, creating speaking anchors, writing the full answer, understanding screenshots, using web information when needed, understanding technical questions, maintaining interview memory, and predicting likely follow-up questions.

Intervjuare itself should remain primarily an orchestration layer.

---

## Local Responsibilities

The application should not attempt to run a large local language model.

Local processing should focus on tasks that do not require advanced reasoning:

```text
audio capture
speech-to-text
voice activity detection
speaker source identification
transcript buffering
detecting known explicit voice commands
loading profile.yaml
local storage
screen capture
window management
interview state
sending context to the model
```

The actual semantic understanding belongs to the model.

---

## Audio Sources

Because interviews are expected to happen mainly online, the two audio sources should remain separate.

```text
MICROPHONE
     ↓
candidate

SYSTEM / meeting application audio
     ↓
interviewer
```

This avoids unnecessary speaker diarization. The application already knows who is speaking based on the audio source.

Example transcript event:

```json
{
  "speaker": "interviewer",
  "text": "Kan du berätta om ett tekniskt problem du löste?",
  "timestamp": 312.4
}
```

---

## Speech-to-Text

Initial transcription engine:

```text
whisper.cpp
```

Primary requirements:

```text
local processing
streaming transcription
Swedish
English
Apple Silicon optimization
partial transcripts
final transcript segments
voice activity detection
low latency
```

The transcription engine should operate continuously and should not wait for an entire sentence before producing text.

Example:

```text
INTERVIEWER PARTIAL
"Kan du berätta om ett tillfälle där..."
```

Later:

```text
INTERVIEWER FINAL
"Kan du berätta om ett tillfälle där du själv upptäckte ett problem?"
```

---

## Language Support

Interview languages:

```text
Swedish
English
```

The candidate profile does not need to be duplicated in both languages. The model should answer in the current interview language.

```text
Swedish interviewer
→ Swedish answer

English interviewer
→ English answer
```

### Swedish Response Style

Swedish answers should prioritize natural spoken language, professional tone, simple sentence construction, easy pronunciation, and clear structure.

Prefer:

```text
"Jag märkte att..."
```

over:

```text
"Jag identifierade sedermera..."
```

The goal is not to make the candidate sound artificially native. The goal is to make the candidate easier to understand and more confident while preserving their natural way of speaking.

---

## Conversation Manager

Intervjuare should maintain structured conversation state.

Example:

```yaml
interview_state:
  language: sv
  current_topic: testing
  topics_discussed:
    - React
    - Next.js
    - analytics
  stories_used:
    - analytics_adblock
  interviewer_focus:
    - testing
    - backend
  candidate_claims_made:
    - used Jest
    - built password reset protection
```

This allows the model to avoid repeating the same example, contradicting an earlier answer, forgetting what was already discussed, or missing references such as `och hur gjorde du då?`.

---

## Recent Conversation

Recent turns should remain available in structured form.

```yaml
recent_turns:
  - speaker: interviewer
    text: >
      Vi pratade lite om React tidigare...
  - speaker: candidate
    text: >
      Ja, jag har arbetat...
  - speaker: interviewer
    text: >
      Hur skulle du säga att du arbetar med tester?
```

The system should preserve speaker identity.

---

## Interview Memory

The entire raw transcript should not need to be resent indefinitely.

Instead, the model can maintain a compact interview memory.

```yaml
interview_memory:
  discussed:
    - React experience
    - Sagobo
    - Jest
  stories_used:
    - analytics_adblock
  interviewer_focus:
    - backend
    - testing
  important_candidate_statements:
    - has used Jest
    - implemented password reset protection
  unanswered_topics:
    - Java experience
```

This memory should represent the important state of the complete interview.

A practical target may be approximately:

```text
500–1500 tokens
```

depending on the interview.

---

## Context Sent to the Model

Conceptually:

```text
┌─────────────────────────────┐
│ SYSTEM RULES                │
├─────────────────────────────┤
│ JOB CONTEXT                 │
├─────────────────────────────┤
│ CANDIDATE PROFILE           │
├─────────────────────────────┤
│ INTERVIEW MEMORY            │
├─────────────────────────────┤
│ RECENT CONVERSATION         │
├─────────────────────────────┤
│ CURRENT INTERVIEWER TURN    │
├─────────────────────────────┤
│ OPTIONAL SCREEN CONTEXT     │
├─────────────────────────────┤
│ TASK                        │
└─────────────────────────────┘
              ↓
            MODEL
```

For the initial implementation, passing the complete `profile.yaml` to the model is acceptable.

There is no requirement to build a vector database, embedding infrastructure, local semantic search, or a complex retrieval pipeline unless the profile eventually becomes too large.

---

## Detecting When to Answer

Intervjuare should automatically detect when the interviewer expects a response.

This must not rely only on grammatical question marks.

Examples:

```text
"Kan du berätta om ett projekt du är stolt över?"
```

```text
"Berätta lite om hur du brukar arbeta med tester."
```

```text
"Jag skulle gärna vilja höra lite mer om din roll i projektet."
```

The model should understand the conversational intent.

---

## Question Understanding

There should not be a separate expensive AI request just to classify the question.

Avoid:

```text
Model request #1
→ classify question

wait

Model request #2
→ answer question
```

Prefer one model generation with instructions to understand the interview context, determine what the interviewer wants, choose the most relevant experience, immediately provide speaking anchors, and continue with the full answer.

---

## Progressive Answer Generation

This is one of the most important features.

The candidate should not have to wait for a complete polished response.

The model should produce output in this order:

```text
1. SPEAKING ANCHORS
2. FULL ANSWER
```

Example:

```text
QUESTION
Kan du berätta om ett problem som du själv upptäckte?
```

First:

```text
SPEAKING ANCHORS

• Google Analytics
• adblockers
• unreliable statistics
• took initiative
• GDPR / consent
```

Then:

```text
FULL ANSWER

När jag implementerade Google Analytics märkte jag att
adblockers kunde blockera analytics även för användare
som faktiskt hade samtyckt till datainsamling.

Det gjorde att statistiken riskerade att bli mindre
tillförlitlig, så jag tog initiativ till att hitta en
lösning som fortfarande respekterade samtycke och GDPR.
```

The full answer should stream progressively. Do not wait for the entire response before displaying it.

---

## Target Latency

Ideal behavior:

```text
Interviewer finishes
        │
        ├── detect end of turn
        ├── build context package
        ├── send to the model
        ├── first anchors arrive
        └── full answer begins streaming
```

Target:

```text
< 1 second
useful anchor content

< 2 seconds
useful answer text
```

The priority is:

```text
FAST + GOOD
```

rather than:

```text
PERFECT + LATE
```

---

## Optional Speculative Preparation

Future versions may begin preparing while the interviewer is still speaking.

Example:

```text
"Kan du berätta om ett tillfälle där du själv..."
```

The model could already infer that the question may involve ownership, initiative, or a behavioral example.

When the interviewer continues:

```text
"...upptäckte ett säkerhetsproblem?"
```

the system can refine its preparation.

However, speculative generation is not required for the first version. The first implementation should first test whether normal end-of-turn generation already meets the latency target.

---

## Interview UI

The interview interface should be minimal.

```text
┌─────────────────────────────────────────────┐
│ INTERVJUARE                          ● LIVE │
├─────────────────────────────────────────────┤
│ QUESTION                                    │
│                                             │
│ Kan du berätta om ett tekniskt problem     │
│ som du själv identifierade?                │
├─────────────────────────────────────────────┤
│ START HERE                                  │
│                                             │
│ • password reset                           │
│ • multiple tabs                            │
│ • server-side protection                   │
│ • Jest tests                               │
│ • race condition lesson                    │
├─────────────────────────────────────────────┤
│ FULL ANSWER                                 │
│                                             │
│ När jag arbetade med...                    │
│ █                                           │
└─────────────────────────────────────────────┘
```

### Reading Ergonomics

The UI exists to be glanced at during a live conversation.

Therefore:

```text
large readable typography
short lines
minimal visual noise
few controls
important concepts easy to scan
minimal animation
little or no scrolling
keyboard-driven controls where useful
```

The interviewer question should remain visible while the suggested response is being shown.

---

## Answer Modes

### Anchors

Only short speaking points.

```text
• Sagobo
• password reset
• two tabs
• server validation
• Jest
```

### Normal

Approximately 20–40 seconds of natural spoken answer.

### Short

One or two sentences.

### STAR

Structured as:

```text
Situation
Task
Action
Result
```

without necessarily displaying those labels in the final spoken text.

### Technical

More technical detail.

### Simple Swedish

Use simpler Swedish sentence construction while preserving professional meaning.

This is especially important for candidates interviewing in a non-native language.

---

## Technical Questions

Not all interview questions concern personal experience.

The model should understand the difference between:

```text
PERSONAL
TECHNICAL
BEHAVIORAL
COMPANY
VISUAL
META
```

Example:

```text
"What is SSR?"
```

does not require a personal story. The model may answer from its technical knowledge.

Example:

```text
"Tell me about a backend system you built."
```

does require grounding in `profile.yaml`.

---

## Grounding Rules

### Technical knowledge

The model may use general model knowledge for questions such as:

```text
What is SSR?
How does React reconciliation work?
What is a race condition?
```

### Personal facts

Candidate profile grounding is required for questions such as:

```text
What did you build?
What technology did you use?
What happened in that project?
What did you learn?
```

If no verified personal example exists:

```text
NO VERIFIED PERSONAL EXAMPLE
```

should be the internal interpretation.

The application may still help formulate an honest answer.

---

## Internet Access

The model may use the internet when useful.

However, web search should not be part of every response.

Normal path:

```text
candidate profile
+
interview context
+
model knowledge
        ↓
answer
```

Internet may be useful for current library versions, current company information, recent technology changes, current product information, questions requiring verification, or other time-sensitive facts.

Because web access can increase latency, it should not unnecessarily block the initial speaking anchors.

Conceptually:

```text
question
   │
   ├── answer possible immediately
   │       ↓
   │   show initial guidance
   │
   └── current information needed
           ↓
       model uses web
           ↓
       response updated
```

---

## Screenshot Assistance

Intervjuare can use screen context when something visual becomes relevant.

Examples:

```text
coding task
architecture diagram
whiteboard
error message
code review
technical diagram
question displayed on screen
```

### Explicit Voice Command

The primary screenshot voice command is:

```text
Låt oss se
```

English equivalent:

```text
Let's see
```

Flow:

```text
candidate says:
"Låt oss se."
        ↓
Intervjuare recognizes explicit command
        ↓
captures relevant screen context
        ↓
sends screen context + interview context to the model
        ↓
the model helps interpret what is shown
```

This command should be detected locally. The command itself should not become normal interview context.

### Automatic Screen Understanding

Intervjuare may also infer that screen context would be useful.

Example interviewer statements:

```text
"Take a look at this function."
"Vad tror du är fel här?"
"If you look at the diagram..."
"Can you see my screen?"
```

In such cases the AI may request screen context.

Automatic screen use should complement, not replace, the explicit `Låt oss se` command.

### Screenshot Lifetime

Screenshots should be treated as temporary context by default:

```text
capture
↓
analyse
↓
discard
```

Screenshots should not automatically become permanent interview history.

---

## Follow-Up Awareness

Intervjuare should understand that interview questions often form chains.

Example:

```text
1. "Kan du berätta om ett tekniskt problem du löste?"
2. "Hur visste du att det faktiskt var ett problem?"
3. "Hur testade du lösningen?"
4. "Vad lärde du dig?"
```

The later questions should not be interpreted as independent questions with no context.

The model should understand what references such as `det`, `den lösningen`, `då`, `projektet`, and `problemet` refer to.

---

## Follow-Up Prediction

While the candidate is answering, the model may prepare likely follow-up areas.

Example:

```text
Current answer involves:
password reset
+
race condition
+
Jest
```

Potential follow-ups:

```text
How did you test it?
Why was client-side protection insufficient?
What happened when two requests arrived simultaneously?
What would you do differently today?
```

These do not need to be displayed immediately. They can be used to prepare context and reduce future latency.

---

## Interruption Handling

Interviewers may interrupt.

```text
candidate answering
        ↓
interviewer begins speaking
        ↓
old answer becomes lower priority
        ↓
listen to interviewer
        ↓
new context
```

The system should not continue aggressively showing an outdated response.

---

## Transcript Corrections

Streaming speech recognition may revise earlier words.

Transcript events should support revisions.

```json
{
  "segment_id": 281,
  "revision": 3,
  "speaker": "interviewer",
  "text": "Kan du berätta om...",
  "final": false
}
```

Later:

```json
{
  "segment_id": 281,
  "revision": 4,
  "speaker": "interviewer",
  "text": "Kan du berätta om ett problem...",
  "final": true
}
```

The interview state should tolerate these corrections.

---

## Manual Controls

Useful manual actions may include:

```text
Force answer
Alternative example
Shorter
Normal answer
STAR answer
More technical
Explain the question
Capture screen
Force web search
Hide/show interface
Clear current suggestion
```

Suggested shortcuts can be decided later. The exact shortcuts are not part of the core architecture.

---

## Privacy and Visibility

Intervjuare is a private personal tool intended for use exclusively by its owner.

A core requirement is that the application remains private during an interview and does not expose its interface, assistance, internal state, or captured context to the interviewer.

Intervjuare should be designed with strong protection against accidental or unintended exposure through screen sharing, meeting software, monitoring environments, or other third-party systems involved in the interview.

From the interviewer's perspective, Intervjuare should not interfere with the interview experience or reveal information about the candidate's private assistance workflow.

This is a product requirement.

This document intentionally does not prescribe a specific technical implementation for achieving it.

---

## Data Privacy

Privacy should be a first-class requirement.

Default expectations:

```text
candidate profile remains local
application settings remain local
transcripts remain local unless explicitly configured otherwise
audio is processed transiently
raw audio is not permanently stored by default
screenshots are temporary by default
no analytics
no telemetry
no unrelated data collection
```

The candidate owns their interview history and profile.

---

## Storage

Recommended local storage:

```text
SQLite
```

Potential stored data:

```text
interviews
transcripts
questions
candidate answers
AI suggestions
used stories
interview memories
job contexts
settings
```

`profile.yaml` remains a separate human-readable source of truth.

---

## Interview Setup

Before starting:

```text
Select / load candidate profile
Enter job information
Choose interview language or automatic detection
Select microphone
Select interview audio source
Test transcription
Confirm ChatGPT connection
Start interview
```

Because this is a personal tool, setup should remain simple.

---

## Interview States

Possible UI states:

```text
LISTENING
QUESTION DETECTED
PREPARING
ANSWERING
SCREEN CONTEXT
WEB
```

---

## Post-Interview Mode

After an interview, Intervjuare should be able to generate:

```text
Interview summary
Questions asked
Candidate answers
Strong answers
Weak answers
Questions that were difficult
Stories that worked well
Stories that were overused
Technical areas worth reviewing
Potential follow-up questions
Potential improvements to profile.yaml
```

The application must not automatically rewrite `profile.yaml`. It may suggest additions or changes. The user decides whether to apply them.

---

## Interview History

Conceptually:

```text
Interview
├── company
├── role
├── date
├── transcript
├── interview memory
├── questions
├── candidate answers
├── AI suggestions
├── stories used
└── post-interview feedback
```

---

## Repository Structure

Proposed:

```text
intervjuare/

README.md
AGENTS.md
profile.yaml

apps/
  desktop/
    src/
      app/
      interview/
      overlay/
      setup/
      profile/
      history/
      settings/

src-tauri/
  src/
    audio/
    transcription/
    interview/
    context/
    llm/
    storage/
    commands/
    events/

native/
  macos/
    Sources/
      AudioCapture/
      ScreenCapture/
      Permissions/

packages/
  shared/
    src/
      types/
      schemas/
      events/

docs/
  architecture.md
  audio-pipeline.md
  model-runtime.md
  context-management.md
  profile-format.md
  latency.md
  privacy.md
  screenshot.md

tests/
```

---

## Building From Source

Large binaries are not stored in the repository. To build locally on Apple Silicon macOS:

```bash
# 1. JavaScript dependencies
npm install

# 2. whisper.cpp (vendored, v1.8.2) with Metal
cmake -S vendor/whisper.cpp -B vendor/whisper.cpp/build \
  -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
  -DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON -DGGML_ACCELERATE=ON
cmake --build vendor/whisper.cpp/build -j --target whisper-server

# 3. Multilingual Whisper model (~574 MB) into assets/models/
sh vendor/whisper.cpp/models/download-ggml-model.sh large-v3-turbo-q5_0 assets/models

# 4. Candidate profile (ignored by Git) — then edit it with your real experience
cp profile.example.yaml profile.yaml

# 5. Build a signed debug bundle (a persistent signing identity keeps macOS permissions stable)
INTERVJUARE_CODESIGN_IDENTITY="<identity from: security find-identity -v -p codesigning>" npm run bundle:stable
```

See `docs/audio-pipeline.md` ("Stable permissions across rebuilds") for signing details, `docs/model-runtime.md` for choosing the ChatGPT account and model, `docs/profile-format.md` for the profile, and `docs/context-management.md` for how interview turns trigger suggestions.

Without audio, the copilot can be tried with **Test without audio**: typed interviewer/candidate turns take the same conversation and model path as transcribed speech.

---

## Development Philosophy

Intervjuare is a personal tool.

Do not over-engineer for hypothetical future users.

Avoid unnecessary systems such as:

```text
accounts
multi-user architecture
payments
subscriptions
cloud sync
organization support
team features
Windows support
mobile support
complex onboarding
```

Build the smallest reliable system that solves the actual problem.

---

## Hermes + Codex During Development

Hermes and Codex CLI can also be used as the development agent for the project.

Development tasks should be given in clearly scoped milestones.

Example:

```text
Implement milestone 1.

Do not implement AI answering yet.

Requirements:
- macOS only
- capture microphone
- capture system interview audio
- keep both streams separate
- transcribe both in real time
- support Swedish and English
- emit structured transcript events
- add tests where practical
```

Avoid asking the coding agent to build the entire application in one pass.

---

## Development Roadmap

### Phase 0 — Project Foundation

Create:

```text
Tauri application
React UI
TypeScript
Rust core
Swift macOS bridge
basic settings
event architecture
```

Verify that all components can communicate.

### Phase 1 — Audio + Transcript MVP

Goal:

```text
Online interview
        ↓
microphone + system audio
        ↓
separate speaker streams
        ↓
live transcription
```

Requirements:

```text
microphone → candidate
system audio → interviewer
Swedish transcription
English transcription
partial transcript
final transcript
low latency
```

Do not add AI answering yet.

### Phase 2 — Interview State

Build:

```text
recent conversation buffer
speaker-aware turns
interviewer turn detection
candidate turn detection
conversation state
job context
profile loading
```

Still no complex AI logic required locally.

### Phase 3 — Model Integration

Connect Intervjuare to the model (ChatGPT subscription).

First test:

```text
current job
+
profile.yaml
+
recent conversation
+
current interviewer turn
        ↓
The model
        ↓
structured response
```

Initial output:

```text
speaking anchors
+
full answer
```

### Phase 4 — Progressive Answer UI

Implement:

```text
anchors appear first
full answer streams below
question remains visible
old answer clears when new question begins
candidate can request alternative response
```

Target:

```text
anchors < 1 second when possible
full answer begins < 2 seconds
```

### Phase 5 — Interview Memory

Add compact long-term interview memory.

The model periodically updates:

```text
topics discussed
stories already used
interviewer interests
candidate claims
important conversation state
```

Recent transcript remains separate.

### Phase 6 — Job-Aware Answering

Make sure the model properly adapts the same experience to different roles.

Test:

```text
same profile + Frontend job
same profile + Backend job
same profile + Design job
```

The facts should remain identical. The emphasis should change.

### Phase 7 — Screenshot Assistance

Implement:

```text
"Låt oss se"
"Let's see"
screen capture
screen context → model
screen-aware answer
```

Then optionally add automatic recognition of visually dependent questions.

### Phase 8 — Internet Access

Allow the model to use current external information when necessary.

Do not make web access the default path for every answer. Test latency carefully.

### Phase 9 — Follow-Up Intelligence

Add:

```text
follow-up awareness
likely follow-up preparation
story reuse avoidance
reference resolution
interruption handling
```

### Phase 10 — Post-Interview Analysis

Generate:

```text
summary
strong answers
weak answers
missing stories
technical weaknesses
suggested profile additions
practice questions
```

---

## MVP Definition

The first genuinely useful version should support:

```text
1. macOS only
2. capture candidate microphone
3. capture interviewer/system audio
4. transcribe Swedish and English
5. know who is speaking
6. load profile.yaml
7. accept job context
8. maintain recent interview context
9. send complete relevant context to the model
10. automatically understand when interviewer expects an answer
11. show fast speaking anchors
12. stream a full answer underneath
13. avoid inventing candidate experience
14. use different angles depending on the role
15. support "Låt oss se"
16. analyse screenshots through the model
17. preserve interview memory
18. optionally use internet information when needed
19. remain a private personal macOS tool
```

### Not MVP

Do not initially build:

```text
Windows support
Linux support
mobile app
cloud accounts
multi-user profiles
payments
subscriptions
team features
cloud sync
complex vector database
large local AI model
quota dashboard
usage statistics UI
complex plugin ecosystem
```

---

## MVP Milestones

### Milestone 1

Before anything else:

```text
Teams / Meet / similar
        ↓
Intervjuare

INTERVIEWER:
"Kan du berätta lite om dig själv?"

CANDIDATE:
"Ja, absolut..."
```

The transcript must correctly show:

```text
INTERVIEWER:
Kan du berätta lite om dig själv?

ME:
Ja, absolut...
```

with acceptable latency and reliability.

Do not proceed until this works well.

### Milestone 2

Add the model.

Input:

```text
profile.yaml
job context
recent interview conversation
current interviewer turn
```

Output:

```text
5 speaking anchors
```

Goal: useful anchors appear extremely quickly.

### Milestone 3

Add streaming full answer.

The candidate should be able to:

```text
look at anchors
↓
begin speaking
↓
glance at full answer when needed
```

### Milestone 4

Add interview memory.

Test similar questions multiple times. Intervjuare should understand that an example has already been used and choose another one when appropriate.

### Milestone 5

Add:

```text
"Låt oss se"
↓
screenshot
↓
vision context
↓
answer
```

### Milestone 6

Add:

```text
internet access
follow-up prediction
post-interview analysis
```

---

## Example Complete Interaction

Interviewer:

```text
Kan du berätta om ett tekniskt problem som du själv upptäckte?
```

Intervjuare already knows:

```text
Role:
Backend Developer

Previously discussed:
React
Sagobo frontend

Stories already used:
analytics_adblock

Candidate profile contains:
password_reset_race_condition
```

The model receives:

```text
job context
+
profile.yaml
+
interview memory
+
recent conversation
+
current interviewer turn
```

UI quickly shows:

```text
• Sagobo password reset
• requests from multiple tabs
• client-side protection wasn't enough
• server-side protection
• Jest tests
```

Then:

```text
När jag arbetade med lösenordsåterställningen i Sagobo
märkte jag att användaren kunde skicka flera requests
väldigt snabbt, till exempel genom två olika flikar.

Jag insåg då att det inte räckte att bara skydda
knappen på frontend, eftersom servern fortfarande
kunde få flera requests.

Jag flyttade därför den viktiga kontrollen till
servern och skrev Jest-tester som simulerade sådana
situationer.

Det lärde mig framför allt att man måste tänka på
race conditions och inte lita på att frontend ensam
skyddar serverlogik.
```

If the interviewer later asks:

```text
Hur testade du det?
```

Intervjuare understands that `det` refers to the password reset solution and continues the same conversation instead of treating it as an unrelated question.

---

## Product Principle

The candidate should not have to stop the conversation and wait for AI.

Ideal experience:

```text
interviewer asks question
        ↓
candidate glances at Intervjuare
        ↓
sees 4–5 useful anchors
        ↓
starts speaking naturally
        ↓
full answer continues appearing
        ↓
candidate uses it only when needed
```

The application should help the candidate remember and express what they already know. It should not attempt to replace the candidate.

---

## Definition of Success

Intervjuare succeeds when:

```text
the candidate can focus on the interviewer
the candidate does not need to search their memory under pressure
good examples are surfaced at the right moment
answers remain truthful
Swedish and English answers sound natural
the same experience is framed appropriately for different roles
follow-up questions retain context
visual questions can be understood
current information can be checked when necessary
the application responds quickly enough that conversation still feels natural
the application remains private to its owner
```

The final experience should feel less like:

```text
AI writes my interview for me
```

and more like:

```text
I already know this.

Intervjuare helps me remember it,
structure it,
and say it clearly.
```
