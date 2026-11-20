// Mirrors the camelCase Serialize types in src-tauri/src/{copilot,profile,context,settings,codex_auth,llm}.rs.

export type Speaker = 'candidate' | 'interviewer'
export type InterviewLanguage = 'sv' | 'en'
export type AnswerMode = 'normal' | 'short' | 'star' | 'technical' | 'simple' | 'explain'

export type SuggestionStatus = 'preparing' | 'streaming' | 'done' | 'skipped' | 'cancelled' | 'failed'

export type SuggestionTiming = {
  turnEndedAtMs: number
  requestedAtMs: number
  firstAnchorAtMs: number | null
  firstAnswerAtMs: number | null
  finishedAtMs: number | null
}

export type SuggestionEvent = {
  suggestionId: number
  status: SuggestionStatus
  question: string
  anchors: string[]
  answer: string
  sources: string[]
  unverifiedSources: string[]
  noVerifiedExample: boolean
  error: string | null
  forced: boolean
  mode: AnswerMode
  alternative: boolean
  screen: boolean
  model: string
  timing: SuggestionTiming
}

export type ProfileSummary = {
  path: string
  candidateName: string | null
  projects: string[]
  stories: string[]
  bytes: number
  warnings: string[]
}

export type JobContext = {
  company: string
  role: string
  jobDescription: string
  importantTopics: string[]
  language: InterviewLanguage
}

export type Settings = {
  accountEmail: string | null
  model: string
  reasoningEffort: string
  screenModel: string
  profilePath: string | null
  job: JobContext
  autoAnswer: boolean
  answerMode: AnswerMode
  reportLanguage: ReportLanguage
  analysisModel: string
  historyEnabled: boolean
}

export type ReportLanguage = 'pl' | 'sv' | 'en'

export type AnalysisEvent = {
  analysisId: number
  status: 'running' | 'done' | 'cancelled' | 'failed'
  text: string
  error: string | null
  model: string
  savedInterviewId: number | null
}

export type InterviewSummary = {
  id: number
  startedAtMs: number
  lastActivityMs: number
  company: string
  role: string
  language: InterviewLanguage
  segmentCount: number
  suggestionCount: number
  hasReport: boolean
  simulatedOnly: boolean
}

export type StoredSegment = {
  segmentId: string
  speaker: Speaker
  text: string
  startedAtMs: number
  endedAtMs: number
  simulated: boolean
}

export type StoredSuggestion = {
  suggestionId: number
  createdAtMs: number
  question: string
  anchors: string[]
  answer: string
  sources: string[]
  mode: AnswerMode
  screen: boolean
  model: string
}

export type InterviewDetail = InterviewSummary & {
  job: JobContext
  memory: string
  report: string | null
  reportLanguage: string | null
  reportModel: string | null
  reportAtMs: number | null
  segments: StoredSegment[]
  suggestions: StoredSuggestion[]
}

export type AccountSummary = {
  email: string
  plan: string | null
  source: string
  expiresAtMs: number
  expired: boolean
}

export type ModelInfo = { slug: string; description: string }

export type ConnectionStatus = {
  ok: boolean
  message: string
  account: AccountSummary | null
  models: ModelInfo[]
  latencyMs: number
}

export type MemoryEvent = { text: string; absorbedTurns: number; error: string | null }

export type CopilotStatus = {
  profile: ProfileSummary | null
  profileError: string | null
  profilePath: string
  settings: Settings
  settingsWarning: string | null
  accounts: AccountSummary[]
  memory: string
  memoryTurns: number
  storiesUsed: string[]
  historyPath: string
  historyError: string | null
}
