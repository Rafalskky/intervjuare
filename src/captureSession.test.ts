import assert from 'node:assert/strict'
import test from 'node:test'
import {
  createCaptureSessionGate,
  type CaptureSessionEvent,
  type PendingCaptureEvent,
} from './captureSession.ts'

type TestCapturePayload = CaptureSessionEvent & {
  state?: 'stopped'
  segmentId?: string
  speaker?: 'candidate' | 'interviewer'
  finalSegment?: boolean
  code?: string
}

function warning(sessionId: string, code: string): PendingCaptureEvent {
  const payload: TestCapturePayload = { sessionId, code }
  return { kind: 'warning', payload }
}

function state(sessionId: string): PendingCaptureEvent {
  return { kind: 'state', payload: { sessionId } }
}

function stoppedState(sessionId: string): PendingCaptureEvent {
  const payload: TestCapturePayload = { sessionId, state: 'stopped' }
  return { kind: 'state', payload }
}

function transcript(
  sessionId: string,
  speaker: 'candidate' | 'interviewer',
  finalSegment = true,
): PendingCaptureEvent {
  const payload: TestCapturePayload = {
    sessionId,
    segmentId: 'segment-1',
    speaker,
    finalSegment,
  }
  return { kind: 'transcript', payload }
}

test('replays matching events that arrive before the start command response', () => {
  const accepted: PendingCaptureEvent[] = []
  const gate = createCaptureSessionGate((event) => accepted.push(event))

  gate.beginStart()
  gate.accept(state('session-new'))
  gate.resolveStart('session-new')

  assert.deepEqual(accepted, [state('session-new')])
})

test('rejects stale early events and accepts events after the response', () => {
  const accepted: PendingCaptureEvent[] = []
  const gate = createCaptureSessionGate((event) => accepted.push(event))

  gate.beginStart()
  gate.accept(state('session-old'))
  gate.resolveStart('session-new')
  gate.accept(state('session-new'))
  gate.accept(state('session-old'))

  assert.deepEqual(accepted, [state('session-new')])
})

test('clears pending events on failed and repeated starts', () => {
  const accepted: PendingCaptureEvent[] = []
  const gate = createCaptureSessionGate((event) => accepted.push(event))

  gate.beginStart()
  gate.accept(state('session-failed'))
  gate.rejectStart()
  gate.beginStart()
  gate.accept(state('session-next'))
  gate.resolveStart('session-next')
  gate.stop('session-next')
  gate.accept(state('session-next'))

  assert.deepEqual(accepted, [state('session-next')])
})

test('preserves a final transcript when terminal state is dispatched first', () => {
  const accepted: PendingCaptureEvent[] = []
  let gate: ReturnType<typeof createCaptureSessionGate>
  gate = createCaptureSessionGate((event) => {
    accepted.push(event)
    const payload = event.payload as TestCapturePayload
    if (event.kind === 'state' && payload.state === 'stopped') {
      gate.stop(payload.sessionId)
    }
  })
  const stopped = stoppedState('session-active')
  const trailingFinal = transcript('session-active', 'candidate')

  gate.beginStart()
  gate.resolveStart('session-active')
  gate.accept(stopped)
  gate.accept(trailingFinal)

  assert.deepEqual(
    accepted,
    [stopped, trailingFinal],
    'terminal state must not make a same-session trailing final disappear',
  )
})

test('rejects stale session finals without relabeling the current source', () => {
  const accepted: PendingCaptureEvent[] = []
  const gate = createCaptureSessionGate((event) => accepted.push(event))
  const staleFinal = transcript('session-old', 'interviewer')
  const currentFinal = transcript('session-new', 'candidate')

  gate.beginStart()
  gate.resolveStart('session-old')
  gate.stop('session-old')
  gate.beginStart()
  gate.resolveStart('session-new')
  gate.accept(staleFinal)
  gate.accept(currentFinal)

  assert.deepEqual(accepted, [currentFinal])
  assert.equal((accepted[0].payload as TestCapturePayload).speaker, 'candidate')
})

test('terminal final-loss warning passes after stopped but transient warnings do not', () => {
  const accepted: PendingCaptureEvent[] = []
  let gate: ReturnType<typeof createCaptureSessionGate>
  gate = createCaptureSessionGate((event) => {
    accepted.push(event)
    const payload = event.payload as TestCapturePayload
    if (event.kind === 'state' && payload.state === 'stopped') {
      gate.stop(payload.sessionId)
    }
  })

  gate.beginStart()
  gate.resolveStart('session-active')
  // Rust emits the terminal warning synchronously before the terminal
  // stopped state; the gate must deliver it in order.
  gate.accept(warning('session-active', 'stt_final_lost'))
  gate.accept(stoppedState('session-active'))
  // Late terminal warning for the same stopped session still passes (user
  // sees the final-loss cause with its cumulative summary); a transient
  // warning after stop is filtered.
  gate.accept(warning('session-active', 'stt_final_lost'))
  gate.accept(warning('session-active', 'audio_backpressure'))

  assert.deepEqual(
    accepted,
    [
      warning('session-active', 'stt_final_lost'),
      stoppedState('session-active'),
      warning('session-active', 'stt_final_lost'),
    ],
    'terminal final-loss must survive Stop; transient post-stop warnings are filtered',
  )
})

function terminalWarning(sessionId: string, lostFinals: number, lostMs: number): PendingCaptureEvent {
  const payload: TestCapturePayload & { message?: string } = {
    sessionId,
    code: 'stt_final_lost',
    message: `Lost finals so far: ${lostFinals} (${lostMs} ms of speech).`,
  }
  return { kind: 'warning', payload }
}

test('multiple cumulative terminal warnings arrive ordered through the same gate', () => {
  const accepted: PendingCaptureEvent[] = []
  let gate: ReturnType<typeof createCaptureSessionGate>
  gate = createCaptureSessionGate((event) => {
    accepted.push(event)
    const payload = event.payload as TestCapturePayload
    if (event.kind === 'state' && payload.state === 'stopped') {
      gate.stop(payload.sessionId)
    }
  })

  gate.beginStart()
  gate.resolveStart('session-active')
  // Rust emits each undecoded final with the session-scoped cumulative
  // summary (Lost finals so far: N) synchronously before the terminal
  // stopped state; the gate must deliver all of them in order. The UI keeps
  // the latest sticky warning, which carries the honest total.
  const first = terminalWarning('session-active', 1, 1000)
  const second = terminalWarning('session-active', 2, 3000)
  gate.accept(first)
  gate.accept(second)
  gate.accept(stoppedState('session-active'))

  assert.deepEqual(
    accepted,
    [first, second, stoppedState('session-active')],
    'every cumulative terminal warning must arrive in order before stopped',
  )
  const messages = accepted
    .filter((event) => event.kind === 'warning')
    .map((event) => (event.payload as TestCapturePayload & { message?: string }).message ?? '')
  assert.ok(messages[0].includes('Lost finals so far: 1'))
  assert.ok(messages[1].includes('Lost finals so far: 2'))
})

test('old-session terminal warning does not update a new session', () => {
  const accepted: PendingCaptureEvent[] = []
  const gate = createCaptureSessionGate((event) => accepted.push(event))

  gate.beginStart()
  gate.resolveStart('session-old')
  gate.stop('session-old')
  gate.beginStart()
  gate.resolveStart('session-new')
  // Stale terminal warning from the dead session is filtered even though its
  // code is terminal; the new session starts clean.
  gate.accept(terminalWarning('session-old', 3, 4000))
  const current = terminalWarning('session-new', 1, 1000)
  gate.accept(current)

  assert.deepEqual(accepted, [current])
})