import { useCallback, useEffect, useState } from 'react'
import type { InterviewDetail, InterviewSummary, ReportLanguage } from './copilotTypes'
import { ANSWER_MODE_LABELS, durationLabel, reportForInterview } from './suggestionView'
import type { CopilotApi } from './useCopilot'

const LANGUAGES: Array<{ code: ReportLanguage; label: string }> = [
  { code: 'pl', label: 'Polski' },
  { code: 'sv', label: 'Svenska' },
  { code: 'en', label: 'English' },
]

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString(undefined, { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

/** Local interview history: list, open, analyse again, delete. */
export function HistoryView({ copilot }: { copilot: CopilotApi }) {
  const [interviews, setInterviews] = useState<InterviewSummary[] | null>(null)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [detail, setDetail] = useState<InterviewDetail | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [language, setLanguage] = useState<ReportLanguage>(copilot.status?.settings.reportLanguage ?? 'pl')
  const { listInterviews, getInterview } = copilot

  const refresh = useCallback(async () => {
    const list = await listInterviews()
    if (list) setInterviews(list)
  }, [listInterviews])

  const open = useCallback(
    async (id: number) => {
      setSelectedId(id)
      setConfirmDelete(false)
      setDetail((await getInterview(id)) ?? null)
    },
    [getInterview],
  )

  useEffect(() => {
    queueMicrotask(() => void refresh())
  }, [refresh])

  // A finished re-analysis is stored with the interview; reload once.
  const finishedAnalysisId =
    copilot.analysis?.status === 'done' && copilot.analysis.savedInterviewId !== null ? copilot.analysis.analysisId : null
  useEffect(() => {
    if (finishedAnalysisId === null || selectedId === null) return
    const timer = setTimeout(() => {
      void open(selectedId)
      void refresh()
    }, 300)
    return () => clearTimeout(timer)
  }, [finishedAnalysisId, selectedId, open, refresh])

  async function remove() {
    if (selectedId === null) return
    await copilot.deleteInterview(selectedId)
    setSelectedId(null)
    setDetail(null)
    setConfirmDelete(false)
    await refresh()
  }

  const { text: report, live } = reportForInterview(detail, copilot.analysis)
  const running = live?.status === 'running'
  const historyOff = copilot.status?.settings.historyEnabled === false

  return (
    <section className="grid gap-5 lg:grid-cols-[340px_1fr]">
      <aside className="rounded-2xl border border-white/10 bg-[#11151b] p-5">
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-medium">Interviews</h2>
          <button type="button" onClick={() => void refresh()} className="rounded-lg border border-white/10 px-2.5 py-1 text-xs text-stone-300">Refresh</button>
        </div>
        <p className="mt-2 text-xs leading-5 text-stone-500">
          Stored only on this Mac{copilot.status ? ` (${copilot.status.historyPath})` : ''}. No audio or screenshots.
          {historyOff && ' Saving is currently off.'}
        </p>
        {copilot.status?.historyError && <p className="mt-2 rounded-lg bg-amber-300/10 p-2 text-xs text-amber-100">{copilot.status.historyError}</p>}
        {interviews === null && <p className="mt-6 text-sm text-stone-500">Loading…</p>}
        {interviews?.length === 0 && <p className="mt-6 text-sm text-stone-500">No saved interviews yet.</p>}
        <ul className="mt-4 space-y-2">
          {interviews?.map((interview) => (
            <li key={interview.id}>
              <button
                type="button"
                onClick={() => void open(interview.id)}
                className={`w-full rounded-xl border p-3 text-left ${interview.id === selectedId ? 'border-emerald-300/40 bg-emerald-300/10' : 'border-white/10 bg-black/20 hover:bg-white/5'}`}
              >
                <p className="text-sm text-stone-100">{interview.company || 'Unnamed company'}{interview.role ? ` · ${interview.role}` : ''}</p>
                <p className="mt-1 text-xs text-stone-500">
                  {formatDate(interview.startedAtMs)} · {durationLabel(interview.startedAtMs, interview.lastActivityMs)} · {interview.language.toUpperCase()}
                </p>
                <p className="mt-1 text-xs text-stone-500">
                  {interview.segmentCount} segments · {interview.suggestionCount} suggestions
                  {interview.hasReport && <span className="ml-2 text-emerald-300">report</span>}
                  {interview.simulatedOnly && <span className="ml-2 text-sky-300">simulated</span>}
                </p>
              </button>
            </li>
          ))}
        </ul>
      </aside>

      <div className="space-y-5">
        {!detail && <section className="rounded-2xl border border-white/10 bg-[#11151b] p-6 text-center text-stone-500">Choose an interview.</section>}
        {detail && (
          <>
            <section className="rounded-2xl border border-white/10 bg-[#11151b] p-6">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <h2 className="text-xl font-medium">{detail.company || 'Unnamed company'}{detail.role ? ` · ${detail.role}` : ''}</h2>
                  <p className="mt-1 text-xs text-stone-500">{formatDate(detail.startedAtMs)} · {durationLabel(detail.startedAtMs, detail.lastActivityMs)} · {detail.language.toUpperCase()}</p>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <select aria-label="Report language" value={language} onChange={(event) => setLanguage(event.target.value as ReportLanguage)} className="rounded-lg border border-white/10 bg-black/30 px-2 py-1.5 text-sm">
                    {LANGUAGES.map((item) => <option key={item.code} value={item.code}>{item.label}</option>)}
                  </select>
                  <button type="button" disabled={running || detail.segmentCount === 0} onClick={() => void copilot.analyzeSavedInterview(detail.id, language)} className="rounded-lg bg-emerald-300 px-3 py-1.5 text-sm font-semibold text-emerald-950 disabled:opacity-40">
                    {running ? 'Analysing…' : detail.report ? 'Analyse again' : 'Analyse interview'}
                  </button>
                  {running && <button type="button" onClick={() => void copilot.cancelAnalysis()} className="rounded-lg border border-white/15 px-3 py-1.5 text-sm text-stone-300">Cancel</button>}
                  {confirmDelete ? (
                    <>
                      <button type="button" onClick={() => void remove()} className="rounded-lg bg-red-400/80 px-3 py-1.5 text-sm font-semibold text-red-950">Delete permanently</button>
                      <button type="button" onClick={() => setConfirmDelete(false)} className="rounded-lg border border-white/15 px-3 py-1.5 text-sm text-stone-300">Keep</button>
                    </>
                  ) : (
                    <button type="button" onClick={() => setConfirmDelete(true)} className="rounded-lg border border-red-300/30 px-3 py-1.5 text-sm text-red-200">Delete</button>
                  )}
                </div>
              </div>
              {live?.error && <p role="alert" className="mt-4 rounded-lg border border-red-300/25 bg-red-400/10 p-3 text-sm text-red-100">{live.error}</p>}
              <h3 className="mt-5 text-xs font-semibold uppercase tracking-[0.2em] text-emerald-300">Report</h3>
              {report ? (
                <pre className="mt-2 max-h-[520px] overflow-auto whitespace-pre-wrap rounded-xl bg-black/25 p-4 font-sans text-sm leading-6 text-stone-200">{report}</pre>
              ) : (
                <p className="mt-2 text-sm text-stone-500">No report yet. Analyse the interview to create one; it uses your current profile.yaml.</p>
              )}
              {!live && detail.reportAtMs && <p className="mt-2 text-xs text-stone-600">{formatDate(detail.reportAtMs)} · {detail.reportModel} · {detail.reportLanguage}</p>}
            </section>

            <section className="rounded-2xl border border-white/10 bg-[#11151b] p-6">
              <h3 className="text-xs font-semibold uppercase tracking-[0.2em] text-sky-300">Transcript</h3>
              <div className="mt-3 space-y-2">
                {detail.segments.map((segment) => (
                  <p key={segment.segmentId} className="text-sm leading-6">
                    <span className={segment.speaker === 'interviewer' ? 'text-sky-300' : 'text-emerald-300'}>{segment.speaker === 'interviewer' ? 'Interviewer' : 'Me'}:</span>{' '}
                    <span className="text-stone-200">{segment.text}</span>
                    {segment.simulated && <span className="ml-2 text-xs text-stone-600">simulated</span>}
                  </p>
                ))}
              </div>
            </section>

            {detail.suggestions.length > 0 && (
              <section className="rounded-2xl border border-white/10 bg-[#11151b] p-6">
                <h3 className="text-xs font-semibold uppercase tracking-[0.2em] text-stone-400">Suggestions shown</h3>
                <div className="mt-3 space-y-3">
                  {detail.suggestions.map((suggestion) => (
                    <details key={suggestion.suggestionId} className="rounded-xl bg-black/20 p-3">
                      <summary className="cursor-pointer text-sm text-stone-200">
                        {suggestion.question}
                        <span className="ml-2 text-xs text-stone-500">{ANSWER_MODE_LABELS[suggestion.mode]}{suggestion.screen ? ' · screen' : ''}</span>
                      </summary>
                      <ul className="mt-2 text-sm text-stone-300">{suggestion.anchors.map((anchor) => <li key={anchor}>• {anchor}</li>)}</ul>
                      <p className="mt-2 whitespace-pre-line text-sm leading-6 text-stone-400">{suggestion.answer}</p>
                      {suggestion.sources.length > 0 && <p className="mt-1 text-xs text-stone-600">Grounded in: {suggestion.sources.join(', ')}</p>}
                    </details>
                  ))}
                </div>
              </section>
            )}

            {detail.memory && (
              <details className="rounded-2xl border border-white/10 bg-[#11151b] p-6">
                <summary className="cursor-pointer text-xs font-semibold uppercase tracking-[0.2em] text-stone-400">Interview memory</summary>
                <pre className="mt-3 whitespace-pre-wrap text-xs leading-5 text-stone-400">{detail.memory}</pre>
              </details>
            )}
          </>
        )}
      </div>
    </section>
  )
}
