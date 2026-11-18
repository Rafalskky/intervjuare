import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Passive-refresh contract: Refresh Sources (list_audio_sources -> bridge
// `sources`) must not request permission. Microphone discovery stays
// independent; ScreenCaptureKit enumeration (a prompting API) must be gated
// behind a non-requesting preflight check. The prompting screen-capture API
// (CGRequestScreenCaptureAccess) may exist ONLY on the explicit
// owner-initiated `request-screen-access` path behind the dedicated UI button
// shown solely for typed `screen_audio_permission_denied`. This static check
// is deterministic and touches no TCC.

const root = resolve(import.meta.dirname, '..')
const swift = readFileSync(
  resolve(root, 'native/macos/Sources/IntervjuareMacOSBridge/main.swift'),
  'utf8',
)
const rust = readFileSync(resolve(root, 'src-tauri/src/lib.rs'), 'utf8')
const frontend = readFileSync(resolve(root, 'src/App.tsx'), 'utf8')
const screenHelper = readFileSync(resolve(root, 'src/screenPermission.ts'), 'utf8')

test('bridge sources path performs a non-requesting screen preflight before ScreenCaptureKit', () => {
  assert.match(swift, /CGPreflightScreenCaptureAccess/, 'sources must preflight with the non-requesting API')
  assert.match(swift, /SCShareableContent/, 'ScreenCaptureKit enumeration must still exist for the granted path')
  // Scope the ordering check to the passive listSources() body: the explicit
  // Start path (CaptureDelegate.start) legitimately calls SCShareableContent
  // after an explicit user action, so a file-wide indexOf would compare the
  // wrong occurrence.
  const listStart = swift.indexOf('private func listSources()')
  assert.ok(listStart !== -1, 'listSources must exist')
  const listBody = swift.slice(listStart, swift.indexOf('private func sourceListHarnessPayload', listStart))
  const preflightPos = listBody.indexOf('CGPreflightScreenCaptureAccess')
  const shareablePos = listBody.indexOf('SCShareableContent.excludingDesktopWindows')
  // Preflight may be called via the screenCapturePreflightGranted() helper.
  const helperPos = listBody.indexOf('screenCapturePreflightGranted()')
  assert.ok(preflightPos !== -1 || helperPos !== -1, 'listSources must preflight (directly or via helper)')
  assert.ok(shareablePos !== -1, 'listSources must still enumerate when preflight passes')
  const gatePos = preflightPos !== -1 ? preflightPos : helperPos
  assert.ok(gatePos < shareablePos, 'preflight gate must precede ScreenCaptureKit enumeration in listSources')
  // The requesting variant must never appear in the passive discovery path:
  // scope to listSources() body, not the whole file (the explicit action path
  // below legitimately contains it).
  assert.doesNotMatch(listBody, /CGRequestScreenCaptureAccess/,
    'passive refresh (listSources) must not call the requesting screen-capture API')
  assert.doesNotMatch(listBody, /requestAccess\(for:\s*\.screenCapture|requestScreenCaptureAccessForExplicitAction/,
    'passive refresh (listSources) must not request screen capture access')
})

test('requesting screen API exists only on the explicit action path', () => {
  // The prompting API must exist exactly once in code (comments stripped).
  const codeOnly = swift.replace(/\/\/.*$/gm, '')
  const occurrences = (codeOnly.match(/CGRequestScreenCaptureAccess/g) ?? []).length
  assert.equal(occurrences, 1, 'prompting API must appear exactly once in Swift')
  const helperStart = swift.indexOf('private func requestScreenCaptureAccessForExplicitAction()')
  assert.ok(helperStart !== -1, 'explicit request helper must exist')
  const helperBody = swift.slice(helperStart, swift.indexOf('\n}\n', helperStart) + 3)
  assert.match(helperBody, /CGRequestScreenCaptureAccess/, 'prompting API must live in the explicit helper')
  // The explicit helper command must be dispatched only for request-screen-access.
  assert.match(swift, /request-screen-access/, 'explicit helper command must exist')
  assert.match(swift, /private func requestScreenAccess\(\)/, 'explicit request handler must exist')
  // listSources and the Start capture path must not call the explicit helper.
  const listStart = swift.indexOf('private func listSources()')
  const listBody = swift.slice(listStart, swift.indexOf('private func sourceListHarnessPayload', listStart))
  assert.ok(!listBody.includes('requestScreenCaptureAccessForExplicitAction'),
    'listSources must not call the explicit request helper')
  const startIdx = swift.indexOf('func start(request: StartRequest)')
  const startBody = swift.slice(startIdx, swift.indexOf('\n    }\n', startIdx) + 4)
  assert.ok(!startBody.includes('requestScreenCaptureAccessForExplicitAction'),
    'Start path must not call the screen request helper (mic + stream start only)')
})

test('microphone discovery stays independent of screen permission', () => {
  // microphoneDevices() must be enumerated unconditionally in listSources,
  // and the partial-source contract must survive screen denial.
  assert.match(swift, /func listSources/)
  assert.match(swift, /microphoneDevices\(\)/)
  assert.match(swift, /screen_audio_permission_denied/)
  assert.match(rust, /source_discovery_preserves_microphones_when_applications_are_unavailable/)
  assert.match(rust, /screen_audio_permission_denied/)
})

test('permission request stays on the explicit Start path, not Refresh', () => {
  // Microphone requestAccess is only valid behind the explicit capture start.
  assert.match(swift, /ensureMicrophoneAccessForExplicitStart/)
  assert.match(swift, /requestMicrophoneAccess|requestAccess\(for: \.audio\)/)
  // Frontend Refresh invokes only list_audio_sources; Start invokes start_audio_capture.
  assert.match(frontend, /invoke<Sources>\('list_audio_sources'\)/)
  assert.match(frontend, /invoke<string>\('start_audio_capture'/)
  // No open-settings / TCC-modifying affordance exists in owned UI code.
  assert.doesNotMatch(frontend, /open.*Settings|NSWorkspace.*open|tccutil/i,
    'permission Settings affordance must not be silently embedded in Refresh')
  // No automatic System Settings opening on the Swift side either.
  assert.doesNotMatch(swift, /NSWorkspace.*open|tccutil/i,
    'Swift must not open Settings or run tccutil')
})

test('explicit screen-permission action is narrow, typed, and refreshes safely', () => {
  // Rust exposes one narrow command for the explicit button; Refresh stays passive.
  assert.match(rust, /fn request_screen_capture_permission/, 'Rust explicit command must exist')
  assert.match(rust, /request-screen-access/, 'Rust explicit command must use the helper command')
  assert.match(rust, /request_screen_capture_permission,/, 'explicit command must be registered')
  // The explicit command returns a typed granted/message status, never starts recording.
  const cmdStart = rust.indexOf('fn request_screen_capture_permission')
  const cmdBody = rust.slice(cmdStart, rust.indexOf('\n}\n', cmdStart) + 3)
  assert.match(cmdBody, /granted/, 'explicit command must return granted status')
  assert.match(cmdBody, /screenPermission/, 'explicit command must parse the screenPermission event')
  assert.ok(!cmdBody.includes('start_audio_capture') && !cmdBody.includes('spawn_transcriber'),
    'explicit permission command must not start recording')
  // list_audio_sources must remain the passive `sources` invocation.
  const listStart = rust.indexOf('fn list_audio_sources')
  const listBody = rust.slice(listStart, rust.indexOf('\n}\n', listStart) + 3)
  assert.match(listBody, /"sources"/, 'Refresh must invoke the passive sources command')
  assert.ok(!listBody.includes('request-screen-access'),
    'Refresh must not invoke the requesting helper command')
  // Frontend: dedicated button invokes the explicit command, gated on the typed denial.
  assert.match(frontend, /request_screen_capture_permission/, 'UI must invoke the explicit command')
  assert.match(frontend, /shouldShowScreenPermissionAction/, 'UI gating must use the pure helper')
  assert.match(frontend, /Allow Screen & System Audio Recording/, 'explicit button label must exist')
  assert.match(frontend, /await refreshSources\(\)/, 'explicit action must refresh sources after completion')
  // Pure helper keeps the gating deterministic and Start still requires mic+app.
  assert.match(screenHelper, /screen_audio_permission_denied/)
  assert.match(screenHelper, /shouldShowScreenPermissionAction/)
  assert.match(screenHelper, /canStartCapture/)
  assert.match(frontend, /canStartCapture\(microphoneId, applicationId\)/, 'Start must stay gated on mic+application')
})
