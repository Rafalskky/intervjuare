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
}
