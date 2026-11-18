export type CaptureSessionEvent = {
  sessionId: string
  finalSegment?: boolean
  code?: string
}

/** Terminal warning codes that must stay visible after Stop. A shed
 * accepted final terminally fails the session; the warning carries the
 * cumulative lost-final count and must not disappear with the terminal
 * stopped state (M6). */
export const TERMINAL_WARNING_CODES = ['stt_final_lost']

export type PendingCaptureEvent =
  | { kind: 'transcript'; payload: CaptureSessionEvent }
  | { kind: 'state'; payload: CaptureSessionEvent }
  | { kind: 'warning'; payload: CaptureSessionEvent }

export type CaptureSessionGate = {
  beginStart: () => void
  accept: (event: PendingCaptureEvent) => void
  resolveStart: (sessionId: string) => void
  rejectStart: () => void
  stop: (sessionId: string) => void
}

export function createCaptureSessionGate(
  dispatch: (event: PendingCaptureEvent) => void,
): CaptureSessionGate {
  let activeSessionId = ''
  let terminalSessionId = ''
  let startPending = false
  let pendingEvents: PendingCaptureEvent[] = []

  return {
    beginStart() {
      activeSessionId = ''
      terminalSessionId = ''
      startPending = true
      pendingEvents = []
    },
    accept(event) {
      if (activeSessionId) {
        if (event.payload.sessionId === activeSessionId) dispatch(event)
        return
      }
      if (
        event.payload.sessionId === terminalSessionId
        && ((event.kind === 'transcript' && event.payload.finalSegment === true)
          || (event.kind === 'warning'
            && TERMINAL_WARNING_CODES.includes(event.payload.code ?? '')))
      ) {
        dispatch(event)
        return
      }
      if (startPending) pendingEvents.push(event)
    },
    resolveStart(sessionId) {
      activeSessionId = sessionId
      startPending = false
      const accepted = pendingEvents.filter((event) => event.payload.sessionId === sessionId)
      pendingEvents = []
      accepted.forEach(dispatch)
    },
    rejectStart() {
      activeSessionId = ''
      terminalSessionId = ''
      startPending = false
      pendingEvents = []
    },
    stop(sessionId) {
      if (activeSessionId === sessionId) {
        activeSessionId = ''
        terminalSessionId = sessionId
      }
      pendingEvents = []
    },
  }
}