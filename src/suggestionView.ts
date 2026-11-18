import type { Speaker, SuggestionEvent, SuggestionTiming } from './copilotTypes.ts'

/** What the suggestion panel shows. `fallback` is the last suggestion with
 * visible content; it comes back when a newer turn turns out to need no
 * answer, so a backchannel never wipes a useful answer off the screen. */
export type SuggestionView = {
  shown: SuggestionEvent | null
  fallback: SuggestionEvent | null
  notice: string | null
}

export const EMPTY_SUGGESTION_VIEW: SuggestionView = { shown: null, fallback: null, notice: null }

export function hasContent(event: SuggestionEvent): boolean {
  return event.anchors.length > 0 || event.answer.length > 0
}

export function applySuggestionEvent(view: SuggestionView, event: SuggestionEvent): SuggestionView {
  const { shown } = view
  // A superseded generation must never repaint over a newer question.
  if (shown && event.suggestionId < shown.suggestionId) return view
  const isNew = !shown || event.suggestionId > shown.suggestionId
  const fallback = isNew && shown && hasContent(shown) ? shown : view.fallback

  if (event.status === 'skipped') {
    return { shown: fallback, fallback, notice: 'Hermes: the last interviewer turn did not need an answer.' }
  }
  if (event.status === 'cancelled' && !hasContent(event)) {
    return { shown: fallback, fallback, notice: 'The interviewer kept talking — waiting for the full question.' }
  }
  return { shown: event, fallback, notice: null }
}

type TranscriptSegmentLike = { speaker: Speaker; finalSegment: boolean; startedAtMs: number }

/** True while the interviewer has started speaking again after the turn the
 * shown suggestion answers: the old answer is now lower priority. */
export function isSuggestionStale(shown: SuggestionEvent | null, segments: TranscriptSegmentLike[]): boolean {
  if (!shown) return false
  return segments.some(
    (segment) => segment.speaker === 'interviewer' && !segment.finalSegment && segment.startedAtMs > shown.timing.turnEndedAtMs,
  )
}

export type LatencySummary = {
  anchorsMs: number | null
  answerMs: number | null
  /** First anchor measured from the captured end of the interviewer's speech. */
  anchorsFromSpeechEndMs: number | null
}

export function latencySummary(timing: SuggestionTiming): LatencySummary {
  const since = (start: number, end: number | null) => (end === null ? null : Math.max(0, end - start))
  return {
    anchorsMs: since(timing.requestedAtMs, timing.firstAnchorAtMs),
    answerMs: since(timing.requestedAtMs, timing.firstAnswerAtMs),
    anchorsFromSpeechEndMs: timing.turnEndedAtMs > 0 ? since(timing.turnEndedAtMs, timing.firstAnchorAtMs) : null,
  }
}

export function formatSeconds(ms: number | null): string {
  return ms === null ? '—' : `${(ms / 1000).toFixed(1)} s`
}

export function parseTopics(input: string): string[] {
  return input
    .split(/[,\n]/)
    .map((topic) => topic.trim())
    .filter((topic) => topic.length > 0)
}
