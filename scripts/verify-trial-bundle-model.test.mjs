import assert from 'node:assert/strict'
import test from 'node:test'
import { EXPECTED_MODELS, REQUIRED_RUNTIME_FILES, evaluateBundleModel } from './verify-trial-bundle-model.mjs'

// Fixture-level contract for the trial bundle model verifier. Uses synthetic
// directory entries only: no 3 GB hashing, no signing, no capture.

function entries(names, sizes = {}) {
  return names.map((name) => ({ name, bytes: sizes[name] ?? 1234 }))
}

function fullLayout(extra = [], sizes = {}) {
  return entries(
    [EXPECTED_MODELS.full.file, ...REQUIRED_RUNTIME_FILES, ...extra],
    { [EXPECTED_MODELS.full.file]: EXPECTED_MODELS.full.bytes, ...sizes },
  )
}

function q5Layout(extra = [], sizes = {}) {
  return entries(
    [EXPECTED_MODELS.q5.file, ...REQUIRED_RUNTIME_FILES, ...extra],
    { [EXPECTED_MODELS.q5.file]: EXPECTED_MODELS.q5.bytes, ...sizes },
  )
}

test('full trial layout passes only for --expect full', () => {
  const verdict = evaluateBundleModel(fullLayout(), 'full')
  assert.equal(verdict.failures.join('\n'), '', 'exact full layout must pass')
  assert.ok(verdict.ok)
})

test('q5 layout passes only for --expect q5', () => {
  const verdict = evaluateBundleModel(q5Layout(), 'q5')
  assert.ok(verdict.ok, verdict.failures.join('\n'))
})

test('q5 expectation on a full bundle fails (no accidental pass)', () => {
  const verdict = evaluateBundleModel(fullLayout(), 'q5')
  assert.ok(!verdict.ok, 'default q5 check must fail on a full trial bundle')
  assert.ok(verdict.failures.some((failure) => failure.includes(EXPECTED_MODELS.q5.file)))
})

test('full expectation on a q5 bundle fails (no accidental pass)', () => {
  const verdict = evaluateBundleModel(q5Layout(), 'full')
  assert.ok(!verdict.ok, 'full check must fail on a default q5 bundle')
})

test('a second bundled model fails even with the right size', () => {
  const verdict = evaluateBundleModel(fullLayout([EXPECTED_MODELS.q5.file], { [EXPECTED_MODELS.q5.file]: EXPECTED_MODELS.q5.bytes }), 'full')
  assert.ok(!verdict.ok, 'two bundled models must fail')
  assert.ok(verdict.failures.some((failure) => failure.includes('exactly one')))
})

test('wrong byte size fails closed', () => {
  const verdict = evaluateBundleModel(
    fullLayout([], { [EXPECTED_MODELS.full.file]: EXPECTED_MODELS.full.bytes - 1 }),
    'full',
  )
  assert.ok(!verdict.ok, 'truncated model must fail')
  assert.ok(verdict.failures.some((failure) => failure.includes('wrong byte size')))
})

test('retired model names are rejected', () => {
  const verdict = evaluateBundleModel(fullLayout(['ggml-small-q5_1.bin']), 'full')
  assert.ok(!verdict.ok, 'retired model alongside the trial model must fail')
})

test('missing whisper-server fails', () => {
  const names = [EXPECTED_MODELS.full.file, ...REQUIRED_RUNTIME_FILES.filter((name) => name !== 'whisper-server')]
  const verdict = evaluateBundleModel(
    names.map((name) => ({ name, bytes: name === EXPECTED_MODELS.full.file ? EXPECTED_MODELS.full.bytes : 1234 })),
    'full',
  )
  assert.ok(!verdict.ok, 'missing whisper-server must fail')
})

test('unknown expectation fails closed', () => {
  const verdict = evaluateBundleModel(fullLayout(), 'turbo')
  assert.ok(!verdict.ok, 'the verifier requires an explicit q5|full expectation')
})
