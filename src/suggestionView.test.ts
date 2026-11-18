import assert from 'node:assert/strict'
import test from 'node:test'
import type { AnalysisEvent, InterviewDetail, SuggestionEvent, SuggestionStatus } from './copilotTypes.ts'
import {
  applySuggestionEvent,
  EMPTY_SUGGESTION_VIEW,
  formatSeconds,
  isSuggestionStale,
  latencySummary,
  durationLabel,
  modelChoices,
  parseTopics,
  reportForInterview,
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
    mode: 'normal',
    alternative: false,
    screen: false,
    model: 'gpt-test',
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
  const failed = { ...suggestion(1, 'failed'), error: 'Could not reach chatgpt.com' }
  const view = applySuggestionEvent(EMPTY_SUGGESTION_VIEW, failed)
  assert.equal(view.shown?.error, 'Could not reach chatgpt.com')
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

test('model choices keep saved selections that the account list lacks', () => {
  const models = [{ slug: 'gpt-6-luna', description: '' }, { slug: 'gpt-6.2-sol', description: '' }]
  assert.deepEqual(modelChoices(models, ['gpt-6-luna', '']), ['gpt-6-luna', 'gpt-6.2-sol'])
  assert.deepEqual(modelChoices([], ['gpt-6-luna', 'gpt-6.1-terra']), ['gpt-6.1-terra', 'gpt-6-luna'])
})

test('interview duration labels', () => {
  assert.equal(durationLabel(0, 20_000), '< 1 min')
  assert.equal(durationLabel(0, 42 * 60_000), '42 min')
  assert.equal(durationLabel(0, 75 * 60_000), '1 h 15 min')
  assert.equal(durationLabel(10, 0), '< 1 min')
})

test('a live analysis of the opened interview wins over the stored report', () => {
  const detail = { id: 4, report: 'stored' } as InterviewDetail
  const analysis = (id: number | null, status: AnalysisEvent['status'], text: string): AnalysisEvent =>
    ({ analysisId: 1, status, text, error: null, model: 'm', savedInterviewId: id })
  assert.equal(reportForInterview(detail, null).text, 'stored')
  assert.equal(reportForInterview(detail, analysis(4, 'running', 'new…')).text, 'new…')
  assert.equal(reportForInterview(detail, analysis(9, 'done', 'other interview')).text, 'stored')
  assert.equal(reportForInterview(detail, analysis(null, 'done', 'live interview')).text, 'stored')
  assert.equal(reportForInterview(detail, analysis(4, 'failed', '')).text, 'stored')
  assert.equal(reportForInterview(null, analysis(4, 'done', 'x')).text, null)
})
