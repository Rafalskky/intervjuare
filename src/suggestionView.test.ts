import assert from 'node:assert/strict'
import test from 'node:test'
import type { SuggestionEvent, SuggestionStatus } from './copilotTypes.ts'
import {
  applySuggestionEvent,
  EMPTY_SUGGESTION_VIEW,
  formatSeconds,
  isSuggestionStale,
  latencySummary,
  parseTopics,
} from './suggestionView.ts'

function suggestion(id: number, status: SuggestionStatus, anchors: string[] = [], answer = ''): SuggestionEvent {
  return {
    suggestionId: id,
    status,
    question: `question ${id}`,
    anchors,
    answer,
    sources: [],
    unverifiedSources: [],
    noVerifiedExample: false,
    error: null,
    forced: false,
    timing: { turnEndedAtMs: 1_000, requestedAtMs: 1_200, firstAnchorAtMs: null, firstAnswerAtMs: null, finishedAtMs: null },
  }
}

test('streaming updates replace the shown suggestion for the same id', () => {
  let view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, suggestion(1, 'preparing'))
  view = applySuggestionEvent(view, suggestion(1, 'streaming', ['a']))
  view = applySuggestionEvent(view, suggestion(1, 'done', ['a', 'b'], 'answer'))
  assert.deepEqual(view.shown?.anchors, ['a', 'b'])
  assert.equal(view.shown?.status, 'done')
  assert.equal(view.notice, null)
})

test('a superseded generation cannot repaint over a newer question', () => {
  let view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, suggestion(2, 'preparing'))
  view = applySuggestionEvent(view, suggestion(1, 'done', ['old'], 'old answer'))
  assert.equal(view.shown?.suggestionId, 2)
})

test('a skipped turn restores the previous useful answer', () => {
  let view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, suggestion(1, 'done', ['a'], 'useful'))
  view = applySuggestionEvent(view, suggestion(2, 'preparing'))
  assert.equal(view.shown?.suggestionId, 2, 'the new question appears immediately')
  view = applySuggestionEvent(view, suggestion(2, 'skipped'))
  assert.equal(view.shown?.suggestionId, 1)
  assert.match(view.notice ?? '', /did not need an answer/)
  view = applySuggestionEvent(view, suggestion(3, 'preparing'))
  assert.equal(view.shown?.suggestionId, 3)
  assert.equal(view.fallback?.suggestionId, 1)
})

test('an empty cancelled generation falls back, a partial one stays visible', () => {
  let view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, suggestion(1, 'done', ['a'], 'x'))
  view = applySuggestionEvent(view, suggestion(2, 'preparing'))
  view = applySuggestionEvent(view, suggestion(2, 'cancelled'))
  assert.equal(view.shown?.suggestionId, 1)

  view = applySuggestionEvent(view, suggestion(3, 'streaming', ['partial']))
  view = applySuggestionEvent(view, suggestion(3, 'cancelled', ['partial']))
  assert.equal(view.shown?.suggestionId, 3)
  assert.equal(view.shown?.status, 'cancelled')
})

test('failures stay visible with their question', () => {
  const failed = { ...suggestion(1, 'failed'), error: 'Hermes is not reachable' }
  const view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, failed)
  assert.equal(view.shown?.error, 'Hermes is not reachable')
})

test('suggestion becomes stale only when the interviewer speaks again after its turn', () => {
  const shown = suggestion(1, 'done', ['a'])
  assert.equal(isSuggestionStale(shown, [{ speaker: 'candidate', finalSegment: false, startedAtMs: 5_000 }]), false)
  assert.equal(isSuggestionStale(shown, [{ speaker: 'interviewer', finalSegment: true, startedAtMs: 5_000 }]), false)
  assert.equal(isSuggestionStale(shown, [{ speaker: 'interviewer', finalSegment: false, startedAtMs: 500 }]), false)
  assert.equal(isSuggestionStale(shown, [{ speaker: 'interviewer', finalSegment: false, startedAtMs: 5_000 }]), true)
  assert.equal(isSuggestionStale(null, [{ speaker: 'interviewer', finalSegment: false, startedAtMs: 5_000 }]), false)
})

test('latency is measured from request and from end of speech', () => {
  const summary = latencySummary({ turnEndedAtMs: 1_000, requestedAtMs: 1_200, firstAnchorAtMs: 1_900, firstAnswerAtMs: 2_400, finishedAtMs: 5_000 })
  assert.deepEqual(summary, { anchorsMs: 700, answerMs: 1_200, anchorsFromSpeechEndMs: 900 })
  assert.equal(latencySummary({ turnEndedAtMs: 0, requestedAtMs: 1, firstAnchorAtMs: null, firstAnswerAtMs: null, finishedAtMs: null }).anchorsMs, null)
  assert.equal(formatSeconds(1_234), '1.2 s')
  assert.equal(formatSeconds(null), '—')
})

test('topics accept commas and new lines', () => {
  assert.deepEqual(parseTopics('backend, APIs\n Java ,, testing\n'), ['backend', 'APIs', 'Java', 'testing'])
})
