import assert from 'node:assert/strict'
import test from 'node:test'
import { resolveBundleSigning, resolveStableIdentity } from './codesign-config.mjs'

test('stable identity resolution requires an explicit persistent identity', () => {
  assert.throws(() => resolveStableIdentity({}), /INTERVJUARE_CODESIGN_IDENTITY is not set/)
  assert.throws(() => resolveStableIdentity({ INTERVJUARE_CODESIGN_IDENTITY: '   ' }), /not set/)
  assert.throws(() => resolveStableIdentity({ INTERVJUARE_CODESIGN_IDENTITY: '-' }), /must not be "-"/)
  assert.equal(
    resolveStableIdentity({ INTERVJUARE_CODESIGN_IDENTITY: 'Apple Development: Owner (TEAMID)' }),
    'Apple Development: Owner (TEAMID)',
  )
})

test('bundle signing never silently falls back to ad-hoc', () => {
  // Missing identity without the explicit developer-only opt-in must throw.
  assert.throws(() => resolveBundleSigning({}, ['node', 'sign']), /INTERVJUARE_CODESIGN_IDENTITY/)
  // "-" as configured identity must throw even with the developer flag present.
  assert.throws(
    () => resolveBundleSigning({ INTERVJUARE_CODESIGN_IDENTITY: '-' }, ['node', 'sign']),
    /must not be "-"/,
  )
  // Explicit developer-only path is reachable only with both env and flag.
  const adhoc = resolveBundleSigning(
    { INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD: '1' },
    ['node', 'sign', '--allow-adhoc-developer-only'],
  )
  assert.deepEqual(adhoc, { identity: '-', developerOnlyAdhoc: true })
  // Env alone (no flag) or flag alone (no env) must still fail closed.
  assert.throws(
    () => resolveBundleSigning({ INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD: '1' }, ['node', 'sign']),
    /INTERVJUARE_CODESIGN_IDENTITY/,
  )
  assert.throws(
    () => resolveBundleSigning({}, ['node', 'sign', '--allow-adhoc-developer-only']),
    /INTERVJUARE_CODESIGN_IDENTITY/,
  )
  // Stable identity wins and is never marked developer-only.
  const stable = resolveBundleSigning(
    { INTERVJUARE_CODESIGN_IDENTITY: 'Apple Development: Owner (TEAMID)' },
    ['node', 'sign'],
  )
  assert.deepEqual(stable, { identity: 'Apple Development: Owner (TEAMID)', developerOnlyAdhoc: false })
})
