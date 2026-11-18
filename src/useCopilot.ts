import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useCallback, useEffect, useRef, useState } from 'react'
import type { CopilotStatus, HermesStatus, InterviewLanguage, JobContext, Speaker, SuggestionEvent } from './copilotTypes'
import { applySuggestionEvent, EMPTY_SUGGESTION_VIEW, type SuggestionView } from './suggestionView'

export type CopilotApi = ReturnType<typeof useCopilot>

/** Copilot state shared by the setup panel and the suggestion panel. All
 * reasoning and state live in Rust; this hook only mirrors and invokes. */
export function useCopilot<TTranscript>(nativeBackendAvailable: boolean, language: InterviewLanguage) {
  const [status, setStatus] = useState<CopilotStatus | null>(null)
  const [view, setView] = useState<SuggestionView>(EMPTY_SUGGESTION_VIEW)
  const [hermes, setHermes] = useState<HermesStatus | null>(null)
  const [checkingHermes, setCheckingHermes] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const jobRef = useRef<JobContext | null>(null)

  const refreshStatus = useCallback(async () => {
    const next = await invoke<CopilotStatus>('copilot_status')
    jobRef.current = next.job
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

  const checkHermes = useCallback(async () => {
    setCheckingHermes(true)
    await run(async () => setHermes(await invoke<HermesStatus>('check_hermes')))
    setCheckingHermes(false)
  }, [run])

  useEffect(() => {
    if (!nativeBackendAvailable) return
    let active = true
    let unlisten: (() => void) | undefined
    listen<SuggestionEvent>('suggestion-event', ({ payload }) => {
      if (active) setView((current) => applySuggestionEvent(current, payload))
    })
      .then((stop) => {
        if (active) unlisten = stop
        else stop()
      })
      .catch((cause) => active && setError(String(cause)))
    queueMicrotask(() => {
      if (!active) return
      void run(refreshStatus)
      void checkHermes()
    })
    return () => {
      active = false
      unlisten?.()
    }
  }, [nativeBackendAvailable, refreshStatus, run, checkHermes])

  const applyJob = useCallback(
    async (job: JobContext) => {
      await run(async () => {
        await invoke('set_job_context', { job })
        await refreshStatus()
      })
    },
    [run, refreshStatus],
  )

  // The interview language is chosen once (audio setup) and shared with Hermes.
  useEffect(() => {
    const job = jobRef.current
    if (!nativeBackendAvailable || !job || job.language === language) return
    void applyJob({ ...job, language })
  }, [nativeBackendAvailable, language, status, applyJob])

  return {
    status,
    view,
    hermes,
    checkingHermes,
    error,
    checkHermes,
    applyJob,
    reloadProfile: (path: string) =>
      run(async () => {
        await invoke('load_candidate_profile', { path: path.trim() || null }).catch(async (cause) => {
          await refreshStatus()
          throw cause
        })
        await refreshStatus()
      }),
    setAutoAnswer: (enabled: boolean) =>
      run(async () => {
        await invoke('set_auto_answer', { enabled })
        await refreshStatus()
      }),
    forceAnswer: () => run(() => invoke<number>('force_answer')),
    cancelSuggestion: () => run(() => invoke('cancel_suggestion')),
    clearConversation: () =>
      run(async () => {
        await invoke('clear_conversation')
        setView(EMPTY_SUGGESTION_VIEW)
      }),
    submitTestTurn: (speaker: Speaker, text: string) =>
      run(() => invoke<TTranscript>('submit_test_turn', { speaker, text })),
  }
}
