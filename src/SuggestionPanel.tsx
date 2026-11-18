import type { SuggestionEvent } from './copilotTypes'
import { formatSeconds, latencySummary } from './suggestionView'

type Props = {
  shown: SuggestionEvent | null
  notice: string | null
  stale: boolean
  canForce: boolean
  onForce: () => void
  onCancel: () => void
}

const STATUS_LABEL: Record<SuggestionEvent['status'], string> = {
  preparing: 'PREPARING',
  streaming: 'ANSWERING',
  done: 'READY',
  skipped: 'LISTENING',
  cancelled: 'CANCELLED',
  failed: 'ERROR',
}

export function SuggestionPanel({ shown, notice, stale, canForce, onForce, onCancel }: Props) {
  const status = shown ? STATUS_LABEL[shown.status] : 'LISTENING'
  const latency = shown ? latencySummary(shown.timing) : null
  const active = shown?.status === 'preparing' || shown?.status === 'streaming'

  return (
    <section className={`rounded-2xl border border-white/10 bg-[#11151b] p-6 transition-opacity ${stale ? 'opacity-50' : ''}`} aria-live="polite">
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <span className={`rounded-full px-3 py-1 text-xs font-semibold tracking-wider ${statusClass(shown?.status)}`}>{stale ? 'INTERVIEWER SPEAKING' : status}</span>
          {latency && (
            <span className="text-xs text-stone-500" title="From request; in brackets from the captured end of the interviewer's speech">
              anchors {formatSeconds(latency.anchorsMs)}
              {latency.anchorsFromSpeechEndMs !== null && ` (${formatSeconds(latency.anchorsFromSpeechEndMs)})`} · answer {formatSeconds(latency.answerMs)}
            </span>
          )}
        </div>
        <div className="flex gap-2">
          <button type="button" disabled={!canForce} onClick={onForce} className="rounded-lg border border-emerald-300/30 bg-emerald-300/10 px-3 py-1.5 text-sm font-semibold text-emerald-100 disabled:opacity-40">
            Force answer
          </button>
          <button type="button" disabled={!active} onClick={onCancel} className="rounded-lg border border-white/15 px-3 py-1.5 text-sm text-stone-300 disabled:opacity-40">
            Cancel
          </button>
        </div>
      </div>

      {notice && <p className="mt-3 text-sm text-stone-400">{notice}</p>}

      {!shown && (
        <p className="mt-10 mb-6 text-center text-stone-500">Suggestions appear here when the interviewer finishes a question.</p>
      )}

      {shown && (
        <div className="mt-5 space-y-6">
          <div>
            <p className="text-xs font-semibold uppercase tracking-[0.2em] text-sky-300">Question</p>
            <p className="mt-2 text-xl leading-8 text-stone-100">{shown.question}</p>
          </div>

          <div>
            <p className="text-xs font-semibold uppercase tracking-[0.2em] text-emerald-300">Start here</p>
            {shown.anchors.length === 0 ? (
              <p className="mt-2 text-lg text-stone-500">{active ? 'Preparing anchors…' : '—'}</p>
            ) : (
              <ul className="mt-2 space-y-1.5">
                {shown.anchors.map((anchor, index) => (
                  <li key={`${index}-${anchor}`} className="text-2xl font-medium leading-9 text-stone-50">• {anchor}</li>
                ))}
              </ul>
            )}
          </div>

          {(shown.answer || active) && (
            <div>
              <p className="text-xs font-semibold uppercase tracking-[0.2em] text-stone-400">Full answer</p>
              <p className="mt-2 max-w-[68ch] whitespace-pre-line text-lg leading-8 text-stone-200">
                {shown.answer}
                {active && <span className="ml-0.5 inline-block h-5 w-2 translate-y-0.5 animate-pulse bg-stone-400" aria-hidden />}
              </p>
            </div>
          )}

          {shown.noVerifiedExample && (
            <p className="rounded-lg border border-amber-300/25 bg-amber-300/10 p-3 text-sm text-amber-100">
              No verified personal example in profile.yaml — this answer avoids personal claims.
            </p>
          )}
          {shown.unverifiedSources.length > 0 && (
            <p role="alert" className="rounded-lg border border-red-300/25 bg-red-400/10 p-3 text-sm text-red-100">
              Cited ids not found in profile.yaml: {shown.unverifiedSources.join(', ')}. Do not rely on those details.
            </p>
          )}
          {shown.error && (
            <p role="alert" className="rounded-lg border border-red-300/25 bg-red-400/10 p-3 text-sm text-red-100">{shown.error}</p>
          )}
          {shown.sources.length > 0 && <p className="text-xs text-stone-500">Grounded in: {shown.sources.join(', ')}</p>}
        </div>
      )}
    </section>
  )
}

function statusClass(status: SuggestionEvent['status'] | undefined): string {
  switch (status) {
    case 'preparing':
    case 'streaming':
      return 'bg-sky-400/15 text-sky-200'
    case 'done':
      return 'bg-emerald-400/15 text-emerald-200'
    case 'failed':
      return 'bg-red-400/15 text-red-200'
    default:
      return 'bg-white/5 text-stone-400'
  }
}
