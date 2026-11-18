import { useState } from 'react'
import type { ReportLanguage } from './copilotTypes'
import { modelChoices } from './suggestionView'
import type { CopilotApi } from './useCopilot'

const LANGUAGES: Array<{ code: ReportLanguage; label: string }> = [
  { code: 'pl', label: 'Polski' },
  { code: 'sv', label: 'Svenska' },
  { code: 'en', label: 'English' },
]

/** Post-interview report (README "Post-Interview Mode"). Suggested profile
 * changes are text only; profile.yaml is never edited by the app. */
export function AnalysisPanel({ copilot, hasConversation }: { copilot: CopilotApi; hasConversation: boolean }) {
  const settings = copilot.status?.settings
  const [language, setLanguage] = useState<ReportLanguage>(settings?.reportLanguage ?? 'pl')
  const [model, setModel] = useState(settings?.analysisModel ?? '')
  const [copied, setCopied] = useState(false)
  const analysis = copilot.analysis
  const running = analysis?.status === 'running'
  const models = modelChoices(copilot.connection?.models ?? [], [model])

  async function start() {
    if (settings && (language !== settings.reportLanguage || model !== settings.analysisModel)) {
      await copilot.setReportSettings(language, model)
    }
    await copilot.analyzeInterview(language)
  }

  async function copy() {
    if (!analysis?.text) return
    await navigator.clipboard.writeText(analysis.text)
    setCopied(true)
  }

  return (
    <section className="rounded-2xl border border-white/10 bg-[#11151b] p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-medium">After the interview</h2>
          <p className="mt-1 text-xs text-stone-500">Summary, strong and weak answers, follow-ups to expect and suggested profile additions. Nothing is changed automatically.</p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <select aria-label="Report language" value={language} onChange={(event) => setLanguage(event.target.value as ReportLanguage)} className="rounded-lg border border-white/10 bg-black/30 px-2 py-1.5 text-sm">
            {LANGUAGES.map((item) => <option key={item.code} value={item.code}>{item.label}</option>)}
          </select>
          <select aria-label="Report model" value={model} onChange={(event) => setModel(event.target.value)} className="rounded-lg border border-white/10 bg-black/30 px-2 py-1.5 text-sm">
            <option value="">Answer model</option>
            {models.map((slug) => <option key={slug} value={slug}>{slug}</option>)}
          </select>
          <button type="button" disabled={!hasConversation || running} onClick={() => void start()} className="rounded-lg bg-emerald-300 px-3 py-1.5 text-sm font-semibold text-emerald-950 disabled:opacity-40">
            {running ? 'Analysing…' : 'Analyse interview'}
          </button>
          {running && (
            <button type="button" onClick={() => void copilot.cancelAnalysis()} className="rounded-lg border border-white/15 px-3 py-1.5 text-sm text-stone-300">
              Cancel
            </button>
          )}
        </div>
      </div>

      {analysis?.error && <p role="alert" className="mt-4 rounded-lg border border-red-300/25 bg-red-400/10 p-3 text-sm text-red-100">{analysis.error}</p>}
      {analysis?.status === 'cancelled' && <p className="mt-4 text-sm text-stone-400">Analysis cancelled.</p>}
      {analysis?.text && (
        <>
          <pre className="mt-4 max-h-[560px] overflow-auto whitespace-pre-wrap rounded-xl bg-black/25 p-4 font-sans text-sm leading-6 text-stone-200">{analysis.text}</pre>
          {analysis.status === 'done' && (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <button type="button" onClick={() => void copy()} className="rounded-lg border border-white/10 px-3 py-1.5 text-sm text-stone-300">{copied ? 'Copied ✓' : 'Copy'}</button>
              <button type="button" onClick={() => void copilot.saveReport(analysis.text)} className="rounded-lg border border-white/10 px-3 py-1.5 text-sm text-stone-300">Save as Markdown</button>
              {copilot.savedReport && <span className="text-xs text-stone-500">Saved: {copilot.savedReport}</span>}
              <span className="text-xs text-stone-600">· {analysis.model}</span>
            </div>
          )}
        </>
      )}
    </section>
  )
}
