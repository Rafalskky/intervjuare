import assert from 'node:assert/strict'
import test from 'node:test'
import { DESKTOP_APP_REQUIRED_MESSAGE, hasTauriBackend } from './runtimeSupport.ts'

test('reports browser-only runtimes as unsupported', () => {
  assert.equal(hasTauriBackend({}), false)
  assert.match(DESKTOP_APP_REQUIRED_MESSAGE, /macOS desktop app/)
  assert.match(DESKTOP_APP_REQUIRED_MESSAGE, /Vite browser preview/)
})

test('retains the native path when the Tauri invoke bridge is present', () => {
  assert.equal(hasTauriBackend({ __TAURI_INTERNALS__: { invoke() {} } }), true)
})