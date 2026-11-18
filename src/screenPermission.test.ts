import assert from 'node:assert/strict'
import test from 'node:test'
import {
  canStartCapture,
  isScreenAudioPermissionDenied,
  isScreenPermissionActionEnabled,
  screenPermissionFollowupMessage,
  shouldShowScreenPermissionAction,
} from './screenPermission.ts'

test('explicit screen action is shown only for typed screen_audio_permission_denied', () => {
  assert.equal(
    shouldShowScreenPermissionAction({ code: 'screen_audio_permission_denied', message: 'x' }),
    true,
  )
  assert.equal(shouldShowScreenPermissionAction(undefined), false)
  assert.equal(shouldShowScreenPermissionAction(null), false)
  assert.equal(shouldShowScreenPermissionAction({ code: 'source_discovery_failed' }), false)
  assert.equal(shouldShowScreenPermissionAction({ code: '' }), false)
})

test('explicit screen action is enabled only for the typed denial and while stopped', () => {
  const denied = { code: 'screen_audio_permission_denied', message: 'x' }
  assert.equal(isScreenPermissionActionEnabled(denied, false), true)
  assert.equal(isScreenPermissionActionEnabled(denied, true), false)
  assert.equal(isScreenPermissionActionEnabled(undefined, false), false)
  assert.equal(isScreenPermissionActionEnabled({ code: 'other' }, false), false)
})

test('start still requires both microphone and application', () => {
  assert.equal(canStartCapture('mic-1', '1234'), true)
  assert.equal(canStartCapture('', '1234'), false)
  assert.equal(canStartCapture('mic-1', ''), false)
  assert.equal(canStartCapture('', ''), false)
})

test('denied code matcher is exact', () => {
  assert.equal(isScreenAudioPermissionDenied('screen_audio_permission_denied'), true)
  assert.equal(isScreenAudioPermissionDenied('SCREEN_AUDIO_PERMISSION_DENIED'), false)
  assert.equal(isScreenAudioPermissionDenied(undefined), false)
  assert.equal(isScreenAudioPermissionDenied(null), false)
})

test('follow-up message stays truthful about restart requirement', () => {
  const granted = screenPermissionFollowupMessage(true, 'Granted.')
  assert.match(granted, /restart/i)
  assert.match(granted, /Refresh sources/)
  const denied = screenPermissionFollowupMessage(false, 'Denied.')
  assert.match(denied, /System Settings/)
  assert.match(denied, /restart/i)
})
