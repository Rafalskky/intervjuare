import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useCallback, useEffect, useMemo, useState } from 'react'
import { createCaptureSessionGate, type PendingCaptureEvent } from './captureSession'
import { DESKTOP_APP_REQUIRED_MESSAGE, hasTauriBackend } from './runtimeSupport'
import { isLikelyMeetingApp } from './appUtils'
import {
  canStartCapture,
  isScreenPermissionActionEnabled,
  screenPermissionFollowupMessage,
  shouldShowScreenPermissionAction,
} from './screenPermission'
import { AnalysisPanel } from './AnalysisPanel'
import { CopilotSetup } from './CopilotSetup'
import { SuggestionPanel } from './SuggestionPanel'
import { isSuggestionStale } from './suggestionView'
import { useCopilot } from './useCopilot'
import type { Speaker as CopilotSpeaker } from './copilotTypes'

type CaptureSource = { id: string; name: string; bundleId?: string }
type SourceDiscoveryError = { code: string; message: string }
type Sources = { microphones: CaptureSource[]; applications: CaptureSource[]; applicationDiscoveryError?: SourceDiscoveryError }
type Speaker = 'candidate' | 'interviewer'
type TranscriptEvent = {
  sessionId: string
  segmentId: string
  revision: number
  speaker: Speaker
  text: string
  finalSegment: boolean
  startedAtMs: number
  endedAtMs: number
  capturedAtMs: number
  emittedAtMs: number
  latencyMs: number
}
type CaptureState = { sessionId: string; state: 'starting' | 'capturing' | 'stopped'; message: string }
type CaptureWarning = { sessionId: string; code: string; message: string; source?: Speaker }

/** Warning codes whose message is a terminal session cause. A terminal
 * warning is sticky: later transient warnings never overwrite it, and it
 * stays visible after Stop so final loss is seen at the correct time with
 * its cumulative numeric summary (M6). Must match TERMINAL_WARNING_CODES
 * in captureSession.ts; the gate lets these through after stopped. */
const TERMINAL_WARNING_CODES = ['stt_final_lost']

function App() {
  const nativeBackendAvailable = hasTauriBackend(window)
  const [sources, setSources] = useState<Sources>({ microphones: [], applications: [] })
  const [microphoneId, setMicrophoneId] = useState('')
  const [applicationId, setApplicationId] = useState('')
  const [language, setLanguage] = useState<'sv' | 'en'>('sv')
  const [captureState, setCaptureState] = useState<CaptureState>({ sessionId: '', state: 'stopped', message: 'Choose both sources before starting.' })
  const [activeSessionId, setActiveSessionId] = useState('')
  const [segments, setSegments] = useState<Record<string, TranscriptEvent>>({})
  const [warning, setWarning] = useState<CaptureWarning | null>(null)
  const [terminalWarning, setTerminalWarning] = useState<CaptureWarning | null>(null)
  const [loading, setLoading] = useState(false)
  const [permissionLoading, setPermissionLoading] = useState(false)
  const [searchTerm, setSearchTerm] = useState('')
  const [sessionGate] = useState(() => {
    let dispatch = (_event: PendingCaptureEvent) => {}
    return {
      gate: createCaptureSessionGate((event) => dispatch(event)),
      setDispatch(next: (event: PendingCaptureEvent) => void) {
        dispatch = next
      },
    }
  })
  const applySessionEvent = useCallback((event: PendingCaptureEvent) => {
    if (event.kind === 'transcript') {
      const payload = event.payload as TranscriptEvent
      setSegments((current) => {
        const previous = current[payload.segmentId]
        if (previous?.finalSegment || (previous && previous.revision >= payload.revision)) return current
        return { ...current, [payload.segmentId]: payload }
      })
    } else if (event.kind === 'state') {
      const payload = event.payload as CaptureState
      setCaptureState(payload)
      if (payload.state === 'stopped') {
        sessionGate.gate.stop(payload.sessionId)
        setActiveSessionId('')
      }
    } else {
      const incoming = event.payload as CaptureWarning
      if (TERMINAL_WARNING_CODES.includes(incoming.code)) {
        // Terminal cause: sticky, never overwritten by transient warnings,
        // and also mirrored to the visible warning slot so it is seen at
        // the correct time (before the terminal stopped state, and after).
        setTerminalWarning(incoming)
        setWarning(incoming)
      } else if (!terminalWarning || terminalWarning.sessionId !== incoming.sessionId) {
        setWarning(incoming)
      }
    }
  }, [sessionGate, terminalWarning])
  sessionGate.setDispatch(applySessionEvent)

  const transcript = useMemo(
    () => Object.values(segments).sort((a, b) => a.startedAtMs - b.startedAtMs),
    [segments],
  )

  const copilot = useCopilot<TranscriptEvent>(nativeBackendAvailable, language)
  const suggestionStale = isSuggestionStale(copilot.view.shown, transcript)
  const hasInterviewerTurn = transcript.some((segment) => segment.speaker === 'interviewer')

  // Simulated turns bypass the capture session gate: Rust returns the
  // synthetic final segment, and it is shown like a transcribed one.
  async function addTestTurn(speaker: CopilotSpeaker, text: string) {
    const event = await copilot.submitTestTurn(speaker, text)
    if (event) setSegments((current) => ({ ...current, [event.segmentId]: event }))
  }

  const refreshSources = useCallback(async () => {
    if (!nativeBackendAvailable) return
    setLoading(true)
    setWarning(null)
    try {
      const next = await invoke<Sources>('list_audio_sources')
      setSources(next)
      setMicrophoneId((current) => next.microphones.some((source) => source.id === current) ? current : (next.microphones[0]?.id ?? ''))
      setApplicationId((current) => next.applications.some((source) => source.id === current) ? current : '')
      if (next.applicationDiscoveryError) {
        setWarning({ sessionId: '', ...next.applicationDiscoveryError })
      }
    } catch (cause) {
      setWarning({ sessionId: '', code: 'source_discovery_failed', message: String(cause) })
    } finally {
      setLoading(false)
    }
  }, [nativeBackendAvailable])

  async function start() {
    if (!nativeBackendAvailable) return
    setWarning(null)
    setTerminalWarning(null)
    setSegments({})
    // A new capture session is a new interview: drop simulated/old turns.
    await copilot.clearConversation()
    sessionGate.gate.beginStart()
    setCaptureState({ sessionId: '', state: 'starting', message: 'Starting capture…' })
    try {
      const sessionId = await invoke<string>('start_audio_capture', { config: { microphoneId, applicationId, language } })
      setActiveSessionId(sessionId)
      sessionGate.gate.resolveStart(sessionId)
    } catch (cause) {
      sessionGate.gate.rejectStart()
      setActiveSessionId('')
      setCaptureState({ sessionId: '', state: 'stopped', message: 'Capture did not start.' })
      setWarning({ sessionId: '', code: 'capture_start_failed', message: String(cause) })
    }
  }

  async function stop() {
    if (!nativeBackendAvailable) return
    try {
      await invoke('stop_audio_capture')
    } catch (cause) {
      setWarning({ sessionId: activeSessionId, code: 'capture_stop_failed', message: String(cause) })
    }
  }

  // Explicit owner-initiated Screen & System Audio Recording request. This is
  // the only UI path that invokes `request_screen_capture_permission` (Swift
  // `request-screen-access` -> prompting CGRequestScreenCaptureAccess). Refresh
  // Sources stays passive; this button is shown/enabled only for the typed
  // `screen_audio_permission_denied` denial. It never starts recording.
  const requestScreenPermission = useCallback(async () => {
    if (!nativeBackendAvailable) return
    if (!shouldShowScreenPermissionAction(sources.applicationDiscoveryError)) return
    if (captureState.state !== 'stopped') return
    setPermissionLoading(true)
    try {
      const result = await invoke<{ granted: boolean; message: string }>('request_screen_capture_permission')
      setWarning({
        sessionId: '',
        code: result.granted ? 'screen_audio_permission_granted' : 'screen_audio_permission_denied',
        message: screenPermissionFollowupMessage(result.granted, result.message),
      })
    } catch (cause) {
      setWarning({ sessionId: '', code: 'screen_audio_permission_denied', message: String(cause) })
    } finally {
      setPermissionLoading(false)
      await refreshSources()
    }
  }, [nativeBackendAvailable, sources.applicationDiscoveryError, captureState.state, refreshSources])

  useEffect(() => {
    if (!nativeBackendAvailable) return
    let active = true
    const cleanup: Array<() => void> = []
    Promise.all([
      listen<TranscriptEvent>('transcript-event', ({ payload }) => {
        if (active) sessionGate.gate.accept({ kind: 'transcript', payload })
      }),
      listen<CaptureState>('capture-state', ({ payload }) => {
        if (active) sessionGate.gate.accept({ kind: 'state', payload })
      }),
      listen<CaptureWarning>('capture-warning', ({ payload }) => {
        if (active) sessionGate.gate.accept({ kind: 'warning', payload })
      }),
    ]).then((unlisteners) => cleanup.push(...unlisteners))
      .catch((cause) => active && setWarning({ sessionId: '', code: 'event_setup_failed', message: String(cause) }))
    queueMicrotask(() => void refreshSources())
    return () => {
      active = false
      cleanup.forEach((unlisten) => unlisten())
    }
  }, [nativeBackendAvailable, refreshSources, sessionGate])

  const running = captureState.state !== 'stopped'
  const showScreenPermissionAction = shouldShowScreenPermissionAction(sources.applicationDiscoveryError)
  const screenPermissionEnabled =
    isScreenPermissionActionEnabled(sources.applicationDiscoveryError, running) &&
    nativeBackendAvailable &&
    !loading &&
    !permissionLoading
  const canStart = canStartCapture(microphoneId, applicationId)

  // Filter applications by search term (case-insensitive on name and bundleId)
  const filteredApplications = useMemo(() => {
    if (!sources.applications) return []
    if (!searchTerm) return sources.applications
    const lowerSearch = searchTerm.toLowerCase()
    return sources.applications.filter(app => 
      app.name.toLowerCase().includes(lowerSearch) || 
      (app.bundleId && app.bundleId.toLowerCase().includes(lowerSearch))
    )
  }, [sources.applications, searchTerm])

  // Group and sort applications: likely meeting apps first, then others
  const groupedApplications = useMemo(() => {
    if (!filteredApplications) return []
    const likely = filteredApplications.filter(isLikelyMeetingApp)
    const others = filteredApplications.filter(app => !isLikelyMeetingApp(app))

    // Define priority order for known meeting apps
    const knownAppOrder = ['Safari', 'Chrome', 'Chromium', 'Edge', 'Firefox', 'Arc', 'Teams']
    const sortedLikely = likely.sort((a, b) => {
      const getIndex = (app: CaptureSource) => {
        const idx = knownAppOrder.indexOf(app.name)
        return idx === -1 ? knownAppOrder.length : idx
      }
      const idxA = getIndex(a)
      const idxB = getIndex(b)
      if (idxA !== idxB) return idxA - idxB
      return a.name.localeCompare(b.name)
    })

    // Sort others by name alphabetically
    const sortedOthers = others.sort((a, b) => a.name.localeCompare(b.name))

    return [...sortedLikely, ...sortedOthers]
  }, [filteredApplications])

  return (
    <main className="min-h-screen bg-[#090b0e] px-6 py-6 text-stone-100">
      <div className="mx-auto max-w-6xl">
        <header className="mb-6 flex items-center justify-between border-b border-white/10 pb-5">
          <div>
            <p className="text-xs font-semibold uppercase tracking-[0.26em] text-emerald-300">Milestones 1–6</p>
            <h1 className="mt-1 text-3xl font-semibold">Interview copilot</h1>
          </div>
          <span className={`rounded-full px-4 py-2 text-sm ${running ? 'bg-red-400/15 text-red-200' : 'bg-white/5 text-stone-400'}`}>
            {captureState.state === 'capturing' ? '● LIVE' : captureState.state.toUpperCase()}
          </span>
        </header>

        <section className="grid gap-5 lg:grid-cols-[340px_1fr]">
          <div>
          <aside className="rounded-2xl border border-white/10 bg-[#11151b] p-5">
            <h2 className="text-lg font-medium">Audio setup</h2>
            <p className="mt-2 text-sm leading-6 text-stone-400">
              The microphone is always candidate audio. For the interviewer, select the app carrying the call: Chrome, Safari, Edge, Firefox, Arc for browser-based calls (like Google Meet or Teams web), or Teams for the desktop app. Google Meet is not a separate macOS app — choose the browser hosting the call.
            </p>

            <label className="mt-5 block text-xs uppercase tracking-wider text-stone-500" htmlFor="microphone">Candidate microphone</label>
            <select id="microphone" disabled={running || !nativeBackendAvailable} value={microphoneId} onChange={(event) => setMicrophoneId(event.target.value)} className="mt-2 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2.5">
              <option value="">Select microphone</option>
              {sources.microphones.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
            </select>

            <label className="mt-5 block text-xs uppercase tracking-wider text-stone-500" htmlFor="application">Interviewer application</label>
            <div className="mt-2">
              <label className="block text-xs uppercase tracking-wider text-stone-500" htmlFor="application-search">
                Filter applications
              </label>
              <input
                id="application-search"
                type="text"
                placeholder="Search by name or bundle ID"
                value={searchTerm}
                onChange={(e) => setSearchTerm(e.target.value)}
                disabled={running || !nativeBackendAvailable}
                className="mt-1 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2 text-sm text-stone-300 disabled:opacity-60"
              />
            </div>
            <select id="application" disabled={running || !nativeBackendAvailable} value={applicationId} onChange={(event) => setApplicationId(event.target.value)} className="mt-2 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2.5">
              <option value="">Select running meeting app</option>
              {groupedApplications.map((source) => (
                <option key={source.id} value={source.id}>
                  {source.name + (source.bundleId ? ` (${source.bundleId})` : '')}
                </option>
              ))}
            </select>

            <label className="mt-5 block text-xs uppercase tracking-wider text-stone-500" htmlFor="language">Interview language</label>
            <select id="language" disabled={running || !nativeBackendAvailable} value={language} onChange={(event) => setLanguage(event.target.value as 'sv' | 'en')} className="mt-2 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2.5">
              <option value="sv">Swedish</option>
              <option value="en">English</option>
            </select>

            <div className="mt-5 flex gap-3">
              <button type="button" disabled={!nativeBackendAvailable || running || !canStart} onClick={() => void start()} className="flex-1 rounded-lg bg-emerald-300 px-4 py-2.5 font-semibold text-emerald-950 disabled:opacity-40">Start</button>
              <button type="button" disabled={!running} onClick={() => void stop()} className="rounded-lg border border-white/15 px-4 py-2.5 disabled:opacity-40">Stop</button>
            </div>
            <button type="button" disabled={!nativeBackendAvailable || running || loading} onClick={() => void refreshSources()} className="mt-3 w-full rounded-lg border border-white/10 px-4 py-2 text-sm text-stone-300 disabled:opacity-40">{loading ? 'Refreshing…' : 'Refresh sources'}</button>
            {showScreenPermissionAction && (
              <button
                type="button"
                disabled={!screenPermissionEnabled}
                onClick={() => void requestScreenPermission()}
                className="mt-3 w-full rounded-lg border border-emerald-300/30 bg-emerald-300/10 px-4 py-2 text-sm font-semibold text-emerald-100 disabled:opacity-40"
              >
                {permissionLoading ? 'Requesting…' : 'Allow Screen & System Audio Recording'}
              </button>
            )}

            <p className={`mt-5 rounded-lg p-3 text-xs leading-5 ${nativeBackendAvailable ? 'bg-black/25 text-stone-400' : 'border border-amber-300/20 bg-amber-300/10 text-amber-100'}`}>{nativeBackendAvailable ? captureState.message : DESKTOP_APP_REQUIRED_MESSAGE}</p>
            <p className="mt-3 text-xs leading-5 text-stone-400">Application audio is captured at app level, not from one isolated browser tab. Avoid unrelated audio in the selected browser while the interview is running.</p>
            <p className="mt-3 text-xs leading-5 text-amber-200/80">Use headphones when possible. Source separation cannot prevent acoustic echo or meeting-app sidetone.</p>
          </aside>
          {nativeBackendAvailable && <CopilotSetup copilot={copilot} language={language} onTestTurn={addTestTurn} />}
          </div>

          <div className="space-y-5">
          {nativeBackendAvailable && (
            <SuggestionPanel
              shown={copilot.view.shown}
              notice={copilot.view.notice}
              stale={suggestionStale}
              canForce={hasInterviewerTurn}
              onForce={(mode, alternative) => void copilot.forceAnswer(mode, alternative)}
              onCaptureScreen={() => void copilot.captureScreen(applicationId)}
              onCancel={() => void copilot.cancelSuggestion()}
            />
          )}
          <section className="min-h-[320px] rounded-2xl border border-white/10 bg-[#11151b] p-6">
            {terminalWarning && <div role="alert" className="mb-5 rounded-xl border border-red-300/25 bg-red-400/10 p-4 text-sm leading-6 text-red-100"><strong>{terminalWarning.code}</strong><br />{terminalWarning.message}</div>}
            {!terminalWarning && warning && <div role="alert" className="mb-5 rounded-xl border border-amber-300/20 bg-amber-300/10 p-4 text-sm leading-6 text-amber-100"><strong>{warning.code}</strong><br />{warning.message}</div>}
            <div className="flex items-center justify-between">
              <h2 className="text-lg font-medium">Conversation</h2>
              <p className="text-xs text-stone-500">Local whisper.cpp · raw audio transient</p>
            </div>
            <div className="mt-5 space-y-4" aria-live="polite">
              {transcript.length === 0 && <p className="mt-20 text-center text-stone-500">Transcript segments will appear here after speech is detected.</p>}
              {transcript.map((segment) => (
                <article key={segment.segmentId} className={`rounded-xl border p-4 ${segment.speaker === 'interviewer' ? 'border-sky-400/15 bg-sky-400/5' : 'border-emerald-400/15 bg-emerald-400/5'}`}>
                  <div className="flex items-center justify-between text-xs uppercase tracking-wider">
                    <span className={segment.speaker === 'interviewer' ? 'text-sky-300' : 'text-emerald-300'}>{segment.speaker === 'interviewer' ? 'Interviewer' : 'Me'}</span>
                    <span className="text-stone-500">{copilot.commandSegments.includes(segment.segmentId) ? 'command · screen' : segment.segmentId.startsWith('simulated-') ? 'simulated' : `${segment.finalSegment ? 'final' : `partial · r${segment.revision}`} · ${segment.latencyMs} ms`}</span>
                  </div>
                  <p className="mt-2 text-lg leading-7 text-stone-100">{segment.text}</p>
                </article>
              ))}
            </div>
          </section>
          {nativeBackendAvailable && <AnalysisPanel copilot={copilot} hasConversation={transcript.length > 0} />}
          </div>
        </section>
      </div>
    </main>
  )
}

export default App