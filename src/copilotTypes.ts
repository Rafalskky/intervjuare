// Mirrors the camelCase Serialize types in src-tauri/src/{copilot,profile,context,hermes}.rs.

export type Speaker = 'candidate' | 'interviewer'
export type InterviewLanguage = 'sv' | 'en'

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

export type HermesConfigSummary = {
  baseUrl: string
  keyConfigured: boolean
  keySource: string
  model: string
  apiServerEnabled: boolean | null
}

export type HermesStatus = {
  reachable: boolean
  authorized: boolean
  message: string
  latencyMs: number
  config: HermesConfigSummary
}

export type CopilotStatus = {
  profile: ProfileSummary | null
  profileError: string | null
  profilePath: string
  job: JobContext
  autoAnswer: boolean
  hermes: HermesConfigSummary
}
