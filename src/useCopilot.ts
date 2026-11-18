import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useCallback, useEffect, useRef, useState } from 'react'
import type {
  AnswerMode,
  ConnectionStatus,
  CopilotStatus,
  InterviewLanguage,
  JobContext,
  MemoryEvent,
  Speaker,
  SuggestionEvent,
} from './copilotTypes'
import { applySuggestionEvent, EMPTY_SUGGESTION_VIEW, type SuggestionView } from './suggestionView'

export type CopilotApi = ReturnType<typeof useCopilot>

export type ModelSettingsInput = {
  accountEmail: string | null
  model: string
  reasoningEffort: string
  screenModel: string
}

/** Copilot state shared by the setup panel and the suggestion panel. All
 * reasoning and state live in Rust; this hook only mirrors and invokes. */
export function useCopilot<TTranscript>(nativeBackendAvailable: boolean, language: InterviewLanguage) {
  const [status, setStatus] = useState<CopilotStatus | null>(null)
  const [view, setView] = useState<SuggestionView>(EMPTY_SUGGESTION_VIEW)
  const [connection, setConnection] = useState<ConnectionStatus | null>(null)
  const [checking, setChecking] = useState(false)
  const [memory, setMemory] = useState<MemoryEvent | null>(null)
  const [commandSegments, setCommandSegments] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)
  const jobRef = useRef<JobContext | null>(null)

  const refreshStatus = useCallback(async () => {
    const next = await invoke<CopilotStatus>('copilot_status')
    jobRef.current = next.settings.job
    setStatus(next)
    return next
  }, [])

  const run = useCallback(async <T,>(action: () => Promise<T>): Promise<T | undefined> => {
    setError(null)
    try {
      return await action()
    } catch (cause) {
      setError(String(cause))
      return undefined
    }
  }, [])

  const checkConnection = useCallback(async () => {
    setChecking(true)
    await run(async () => setConnection(await invoke<ConnectionStatus>('check_connection')))
    setChecking(false)
  }, [run])

  useEffect(() => {
    if (!nativeBackendAvailable) return
    let active = true
    const stops: Array<() => void> = []
    const keep = (stop: () => void) => (active ? stops.push(stop) : stop())
    listen<SuggestionEvent>('suggestion-event', ({ payload }) => {
      if (active) setView((current) => applySuggestionEvent(current, payload))
    }).then(keep).catch((cause) => active && setError(String(cause)))
    listen<MemoryEvent>('memory-event', ({ payload }) => {
      if (active) setMemory(payload)
    }).then(keep).catch((cause) => active && setError(String(cause)))
    listen<{ segmentId: string }>('voice-command', ({ payload }) => {
      if (active) setCommandSegments((current) => [...current, payload.segmentId])
    }).then(keep).catch((cause) => active && setError(String(cause)))
    queueMicrotask(() => {
      if (!active) return
      void run(refreshStatus).then(() => void checkConnection())
    })
    return () => {
      active = false
      stops.forEach((stop) => stop())
    }
  }, [nativeBackendAvailable, refreshStatus, run, checkConnection])

  const applyJob = useCallback(
    async (job: JobContext) => {
      await run(async () => {
        await invoke('set_job_context', { job })
        await refreshStatus()
      })
    },
    [run, refreshStatus],
  )

  // The interview language is chosen once (audio setup) and shared with the model.
  useEffect(() => {
    const job = jobRef.current
    if (!nativeBackendAvailable || !job || job.language === language) return
    void applyJob({ ...job, language })
  }, [nativeBackendAvailable, language, status, applyJob])

  return {
    status,
    view,
    connection,
    checking,
    memory,
    commandSegments,
    error,
    checkConnection,
    applyJob,
    reloadProfile: (path: string) =>
      run(async () => {
        try {
          await invoke('load_candidate_profile', { path: path.trim() || null })
        } finally {
          await refreshStatus()
        }
      }),
    saveModelSettings: (input: ModelSettingsInput) =>
      run(async () => {
        await invoke('set_model_settings', input)
        await refreshStatus()
        await checkConnection()
      }),
    setAutoAnswer: (enabled: boolean) =>
      run(async () => {
        await invoke('set_auto_answer', { enabled })
        await refreshStatus()
      }),
    setAnswerMode: (mode: AnswerMode) =>
      run(async () => {
        await invoke('set_answer_mode', { mode })
        await refreshStatus()
      }),
    forceAnswer: (mode?: AnswerMode, alternative = false) =>
      run(() => invoke<number>('force_answer', { mode: mode ?? null, alternative })),
    captureScreen: (applicationId: string) =>
      run(() => invoke<number>('capture_screen_answer', { applicationId: applicationId || null })),
    cancelSuggestion: () => run(() => invoke('cancel_suggestion')),
    clearConversation: () =>
      run(async () => {
        await invoke('clear_conversation')
        setView(EMPTY_SUGGESTION_VIEW)
        setMemory(null)
        setCommandSegments([])
      }),
    submitTestTurn: (speaker: Speaker, text: string) =>
      run(() => invoke<TTranscript>('submit_test_turn', { speaker, text })),
  }
}
