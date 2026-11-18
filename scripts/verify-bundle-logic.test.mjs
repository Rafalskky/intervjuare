import assert from 'node:assert/strict'
import test from 'node:test'
import {
  checkSingleTarget,
  evaluateSingleBundle,
  evaluateStability,
  evaluateTwoBuild,
  isAdhocSignature,
  isClassifiedTwoBuildFailure,
  isMissingTeam,
  leafAuthority,
  parseDesignatedRequirement,
  parseDisplayOutput,
} from './verify-logic.mjs'

function target({
  verifyExit = 0,
  identifier = 'com.rafal.intervjuare',
  team = 'TEAMID123',
  cdhash = 'aaa',
  signature = 'size=12345',
  designated = 'identifier "com.rafal.intervjuare" and anchor apple generic',
  authorities = ['Apple Development: Owner Name (TEAMDEV123)'],
} = {}) {
  return { verifyExit, identifier, team, cdhash, signature, designated, authorities }
}

function helperTarget(overrides = {}) {
  return target({ identifier: 'com.rafal.intervjuare.macos-bridge', ...overrides })
}

test('Apple Development single bundle passes without requiring CDHash', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'TEAMDEV123', cdhash: 'aaa' }),
    helper: helperTarget({ team: 'TEAMDEV123', cdhash: 'bbb' }),
  })
  assert.deepEqual(failures, [])
})

test('Developer ID single bundle passes', () => {
  const { failures, mode } = evaluateSingleBundle({
    app: target({ team: 'TEAMDIST456' }),
    helper: helperTarget({ team: 'TEAMDIST456' }),
  })
  assert.deepEqual(failures, [])
  assert.match(mode, /apple-certificate/)
})

test('self-signed with no team passes single-bundle non-ad-hoc check', () => {
  const { failures, mode } = evaluateSingleBundle({
    app: target({ team: 'not set', signature: 'size=9999', authorities: ['Intervjuare Local'] }),
    helper: helperTarget({ team: 'not set', signature: 'size=8888', authorities: ['Intervjuare Local'] }),
  })
  assert.deepEqual(failures, [])
  assert.match(mode, /self-signed/)
})

test('ad-hoc app signature is rejected', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'not set', signature: 'adhoc' }),
    helper: helperTarget({ team: 'not set', signature: 'size=8888' }),
  })
  assert.ok(failures.some((f) => f.startsWith('app:') && f.includes('ad-hoc')))
})

test('ad-hoc helper signature is rejected even when app is valid', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'TEAMDEV123', signature: 'size=12345' }),
    helper: helperTarget({ team: 'not set', signature: 'adhoc' }),
  })
  assert.ok(failures.some((f) => f.startsWith('helper:') && f.includes('ad-hoc')))
})

test('missing signature is rejected as ad-hoc', () => {
  assert.equal(isAdhocSignature(''), true)
  assert.equal(isAdhocSignature('(missing)'), true)
  assert.equal(isAdhocSignature('adhoc'), true)
  assert.equal(isAdhocSignature('ADHOC'), true)
  assert.equal(isAdhocSignature('size=123'), false)
})

test('two-build Apple continuity passes with changed CDHash', () => {
  const { failures, cdhashChanged } = evaluateStability({
    prevApp: target({ team: 'TEAMDEV123', cdhash: 'old-app', designated: 'REQ-APP' }),
    currApp: target({ team: 'TEAMDEV123', cdhash: 'new-app', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'TEAMDEV123', cdhash: 'old-helper', designated: 'REQ-HELP' }),
    currHelper: helperTarget({ team: 'TEAMDEV123', cdhash: 'new-helper', designated: 'REQ-HELP' }),
  })
  assert.deepEqual(failures, [])
  assert.equal(cdhashChanged, true)
})

test('two-build self-signed continuity passes without team via stable DR', () => {
  const { failures } = evaluateStability({
    prevApp: target({ team: 'not set', cdhash: 'a1', designated: 'REQ-APP-SELF', signature: 'size=1' }),
    currApp: target({ team: 'not set', cdhash: 'a2', designated: 'REQ-APP-SELF', signature: 'size=1' }),
    prevHelper: helperTarget({ team: 'not set', cdhash: 'h1', designated: 'REQ-HELP-SELF' }),
    currHelper: helperTarget({ team: 'not set', cdhash: 'h2', designated: 'REQ-HELP-SELF' }),
  })
  assert.deepEqual(failures, [])
})

test('DR drift is rejected for app and helper', () => {
  const appDrift = evaluateStability({
    prevApp: target({ designated: 'REQ-OLD' }),
    currApp: target({ designated: 'REQ-NEW' }),
    prevHelper: helperTarget({ designated: 'REQ-HELP' }),
    currHelper: helperTarget({ designated: 'REQ-HELP' }),
  })
  assert.ok(appDrift.failures.some((f) => f.includes('app designated')))

  const helperDrift = evaluateStability({
    prevApp: target({ designated: 'REQ-APP' }),
    currApp: target({ designated: 'REQ-APP' }),
    prevHelper: helperTarget({ designated: 'REQ-H-OLD' }),
    currHelper: helperTarget({ designated: 'REQ-H-NEW' }),
  })
  assert.ok(helperDrift.failures.some((f) => f.includes('helper designated')))
})

test('TeamIdentifier presence parity and equality are enforced', () => {
  const parity = evaluateStability({
    prevApp: target({ team: 'TEAMDEV123' }),
    currApp: target({ team: 'not set' }),
    prevHelper: helperTarget({ team: 'TEAMDEV123' }),
    currHelper: helperTarget({ team: 'TEAMDEV123' }),
  })
  assert.ok(parity.failures.some((f) => f.includes('app TeamIdentifier presence')))

  const changed = evaluateStability({
    prevApp: target({ team: 'TEAMAAA' }),
    currApp: target({ team: 'TEAMBBB' }),
    prevHelper: helperTarget({ team: 'TEAMAAA' }),
    currHelper: helperTarget({ team: 'TEAMAAA' }),
  })
  assert.ok(changed.failures.some((f) => f.includes('app TeamIdentifier changed')))
})

test('missing-team helper is detected', () => {
  assert.equal(isMissingTeam('not set'), true)
  assert.equal(isMissingTeam(''), true)
  assert.equal(isMissingTeam('(missing)'), true)
  assert.equal(isMissingTeam('TEAMID123'), false)
})

// ---- RED fixtures for the Hermes-reported verifier defects ----

const ADHOC_APP_DISPLAY = `Executable=/tmp/Intervjuare.app/Contents/MacOS/intervjuare
Identifier=com.rafal.intervjuare
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=10+7 location=embedded
Signature=adhoc
Info.plist entries=22
TeamIdentifier=not set
Sealed Resources version=2 rules=13 files=5
Internal requirements count=1 size=220
`

const APPLE_DEV_APP_DISPLAY = `Executable=/tmp/Intervjuare.app/Contents/MacOS/intervjuare
Identifier=com.rafal.intervjuare
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=45678 flags=0x10000(runtime) hashes=200+7 location=embedded
Signature size=9001
Authority=Apple Development: Owner Name (TEAMDEV123)
Authority=Apple Worldwide Developer Relations Certification Authority
Authority=Apple Root CA
Info.plist entries=22
TeamIdentifier=TEAMDEV123
Sealed Resources version=2 rules=13 files=5
Internal requirements count=1 size=220
`

const DEVELOPER_ID_HELPER_DISPLAY = `Executable=/tmp/Intervjuare.app/Contents/MacOS/intervjuare-macos-bridge
Identifier=com.rafal.intervjuare.macos-bridge
Format=Mach-O thin (arm64)
CodeDirectory v=20500 size=12340 flags=0x10000(runtime) hashes=60+7 location=embedded
Signature size=9010
Authority=Developer ID Application: Owner Name (TEAMDIST456)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
TeamIdentifier=TEAMDIST456
`

const SELF_SIGNED_APP_DISPLAY = `Executable=/tmp/Intervjuare.app/Contents/MacOS/intervjuare
Identifier=com.rafal.intervjuare
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20200 size=8900 flags=0x0 hashes=50+5 location=embedded
Signature size=4755
Authority=Intervjuare Local
Authority=Intervjuare Local
Info.plist entries=22
TeamIdentifier=not set
Sealed Resources version=2 rules=13 files=5
Internal requirements count=1 size=220
`

const UNSIGNED_DISPLAY = `/tmp/Unsigned.app: code object is not signed at all
`

test('real codesign format: explicit Signature=adhoc classifies as adhoc', () => {
  const parsed = parseDisplayOutput(ADHOC_APP_DISPLAY)
  assert.equal(parsed.identifier, 'com.rafal.intervjuare')
  assert.equal(parsed.signatureKind, 'adhoc')
  assert.equal(isAdhocSignature(parsed.signature), true)
})

test('real codesign format: Apple Development Signature size + Authority classifies as certificate', () => {
  const parsed = parseDisplayOutput(APPLE_DEV_APP_DISPLAY)
  assert.equal(parsed.identifier, 'com.rafal.intervjuare')
  assert.equal(parsed.team, 'TEAMDEV123')
  assert.equal(parsed.signatureKind, 'certificate')
  assert.equal(isAdhocSignature(parsed.signature), false)
  assert.deepEqual(parsed.authorities, [
    'Apple Development: Owner Name (TEAMDEV123)',
    'Apple Worldwide Developer Relations Certification Authority',
    'Apple Root CA',
  ])
  assert.equal(leafAuthority(parsed), 'Apple Development: Owner Name (TEAMDEV123)')
})

test('real codesign format: Developer ID helper classifies as certificate', () => {
  const parsed = parseDisplayOutput(DEVELOPER_ID_HELPER_DISPLAY)
  assert.equal(parsed.identifier, 'com.rafal.intervjuare.macos-bridge')
  assert.equal(parsed.team, 'TEAMDIST456')
  assert.equal(parsed.signatureKind, 'certificate')
  assert.equal(isAdhocSignature(parsed.signature), false)
  assert.equal(leafAuthority(parsed), 'Developer ID Application: Owner Name (TEAMDIST456)')
})

test('real codesign format: self-signed classifies as certificate with missing team', () => {
  const parsed = parseDisplayOutput(SELF_SIGNED_APP_DISPLAY)
  assert.equal(parsed.signatureKind, 'certificate')
  assert.equal(isMissingTeam(parsed.team), true)
  assert.equal(isAdhocSignature(parsed.signature), false)
  assert.equal(leafAuthority(parsed), 'Intervjuare Local')
})

test('real codesign format: unsigned output never classifies as certificate', () => {
  const parsed = parseDisplayOutput(UNSIGNED_DISPLAY)
  assert.equal(parsed.signatureKind, 'missing')
  assert.equal(isAdhocSignature(parsed.signature), true)
  const { failures } = evaluateSingleBundle({
    app: { ...target({ team: 'not set', authorities: [] }), ...parsed, verifyExit: 1, designated: '' },
    helper: helperTarget({ team: 'TEAMDEV123' }),
  })
  assert.ok(failures.length > 0)
})

test('single bundle rejects app/helper with different nonempty TeamIdentifiers', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'TEAMAAA', authorities: ['Apple Development: A (TEAMAAA)'] }),
    helper: helperTarget({ team: 'TEAMBBB', authorities: ['Apple Development: B (TEAMBBB)'] }),
  })
  assert.ok(failures.some((f) => f.includes('TeamIdentifier')))
})

test('single bundle rejects mismatched Apple leaf authority even with same team', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'TEAMDEV123', authorities: ['Apple Development: Owner Name (TEAMDEV123)'] }),
    helper: helperTarget({ team: 'TEAMDEV123', authorities: ['Apple Development: Other Name (TEAMDEV123)'] }),
  })
  assert.ok(failures.some((f) => f.toLowerCase().includes('authority')))
})

test('single bundle rejects mismatched self-signed leaf authority', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'not set', signature: 'size=111', authorities: ['Intervjuare Local'] }),
    helper: helperTarget({ team: 'not set', signature: 'size=222', authorities: ['Other Local'] }),
  })
  assert.ok(failures.some((f) => f.toLowerCase().includes('authority')))
})

test('single bundle rejects self-signed with missing leaf authority', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'not set', signature: 'size=111', authorities: [] }),
    helper: helperTarget({ team: 'not set', signature: 'size=222', authorities: [] }),
  })
  assert.ok(failures.some((f) => f.toLowerCase().includes('authority')))
})

test('app/helper designated requirements are independent; CDHash never required', () => {
  const { failures } = evaluateSingleBundle({
    app: target({ team: 'TEAMDEV123', cdhash: 'app-hash-1', designated: 'REQ-APP' }),
    helper: helperTarget({ team: 'TEAMDEV123', cdhash: 'helper-hash-2', designated: 'REQ-HELPER-DIFFERENT' }),
  })
  assert.deepEqual(failures, [])
})

test('two-build verification fails on corrupt/ad-hoc previous app', () => {
  const { failures } = evaluateTwoBuild({
    prevApp: target({ verifyExit: 1, team: 'not set', signature: 'adhoc', authorities: [], designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'TEAMDEV123', designated: 'REQ-HELP' }),
    currApp: target({ team: 'TEAMDEV123', designated: 'REQ-APP' }),
    currHelper: helperTarget({ team: 'TEAMDEV123', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.includes('previous') && f.toLowerCase().includes('ad-hoc') || f.includes('previous app')))
})

test('two-build verification fails on ad-hoc previous helper even when current is valid', () => {
  const { failures } = evaluateTwoBuild({
    prevApp: target({ team: 'TEAMDEV123', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ verifyExit: 0, team: 'not set', signature: 'adhoc', authorities: [], designated: 'REQ-HELP' }),
    currApp: target({ team: 'TEAMDEV123', designated: 'REQ-APP' }),
    currHelper: helperTarget({ team: 'TEAMDEV123', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.includes('previous helper')))
})

test('two-build self-signed authority must remain stable', () => {
  const { failures } = evaluateStability({
    prevApp: target({ team: 'not set', signature: 'size=1', authorities: ['Intervjuare Local'], cdhash: 'a1', designated: 'REQ-APP' }),
    currApp: target({ team: 'not set', signature: 'size=1', authorities: ['Other Local'], cdhash: 'a2', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'not set', authorities: ['Intervjuare Local'], cdhash: 'h1', designated: 'REQ-HELP' }),
    currHelper: helperTarget({ team: 'not set', authorities: ['Intervjuare Local'], cdhash: 'h2', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.toLowerCase().includes('authority')))
})

test('two-build previous app/helper TeamIdentifier mismatch cannot be hidden by per-target continuity', () => {
  // Previous bundle is incoherent (app TEAMAAA vs helper TEAMBBB), while the
  // current bundle preserves each target's identity/DR so per-target
  // stability alone would pass. The previous coherence defect must still fail.
  const { failures } = evaluateTwoBuild({
    prevApp: target({ team: 'TEAMAAA', authorities: ['Apple Development: A (TEAMAAA)'], cdhash: 'prev-a', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'TEAMBBB', authorities: ['Apple Development: B (TEAMBBB)'], cdhash: 'prev-h', designated: 'REQ-HELP' }),
    currApp: target({ team: 'TEAMAAA', authorities: ['Apple Development: A (TEAMAAA)'], cdhash: 'curr-a', designated: 'REQ-APP' }),
    currHelper: helperTarget({ team: 'TEAMBBB', authorities: ['Apple Development: B (TEAMBBB)'], cdhash: 'curr-h', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.startsWith('previous') && f.includes('TeamIdentifier')), `expected previous-scoped TeamIdentifier failure, got: ${JSON.stringify(failures)}`)
})

test('two-build previous mismatched self-signed leaf authorities cannot be hidden by per-target continuity', () => {
  const { failures } = evaluateTwoBuild({
    prevApp: target({ team: 'not set', signature: 'size=111', authorities: ['Intervjuare Local A'], cdhash: 'prev-a', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: ['Other Local B'], cdhash: 'prev-h', designated: 'REQ-HELP' }),
    currApp: target({ team: 'not set', signature: 'size=111', authorities: ['Intervjuare Local A'], cdhash: 'curr-a', designated: 'REQ-APP' }),
    currHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: ['Other Local B'], cdhash: 'curr-h', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.startsWith('previous') && f.toLowerCase().includes('authority')), `expected previous-scoped authority failure, got: ${JSON.stringify(failures)}`)
})

test('two-build previous missing self-signed authority cannot be hidden by per-target continuity', () => {
  const { failures } = evaluateTwoBuild({
    prevApp: target({ team: 'not set', signature: 'size=111', authorities: [], cdhash: 'prev-a', designated: 'REQ-APP' }),
    prevHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: [], cdhash: 'prev-h', designated: 'REQ-HELP' }),
    currApp: target({ team: 'not set', signature: 'size=111', authorities: [], cdhash: 'curr-a', designated: 'REQ-APP' }),
    currHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: [], cdhash: 'curr-h', designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.startsWith('previous') && f.toLowerCase().includes('authority')), `expected previous-scoped missing-authority failure, got: ${JSON.stringify(failures)}`)
})

test('evaluateTwoBuild failures contain no unclassified entry', () => {
  const cases = [
    evaluateTwoBuild({
      prevApp: target({ team: 'TEAMAAA', authorities: ['Apple Development: A (TEAMAAA)'], designated: 'REQ-APP' }),
      prevHelper: helperTarget({ team: 'TEAMBBB', authorities: ['Apple Development: B (TEAMBBB)'], designated: 'REQ-HELP' }),
      currApp: target({ team: 'TEAMAAA', authorities: ['Apple Development: A (TEAMAAA)'], designated: 'REQ-APP' }),
      currHelper: helperTarget({ team: 'TEAMBBB', authorities: ['Apple Development: B (TEAMBBB)'], designated: 'REQ-HELP' }),
    }),
    evaluateTwoBuild({
      prevApp: target({ team: 'not set', signature: 'size=111', authorities: ['Intervjuare Local A'], designated: 'REQ-APP' }),
      prevHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: ['Other Local B'], designated: 'REQ-HELP' }),
      currApp: target({ team: 'not set', signature: 'size=111', authorities: ['Intervjuare Local A'], designated: 'REQ-APP' }),
      currHelper: helperTarget({ team: 'not set', signature: 'size=222', authorities: ['Other Local B'], designated: 'REQ-HELP' }),
    }),
    evaluateTwoBuild({
      prevApp: target({ verifyExit: 1, team: 'not set', signature: 'adhoc', authorities: [], designated: 'REQ-APP' }),
      prevHelper: helperTarget({ team: 'TEAMDEV123', designated: 'REQ-HELP' }),
      currApp: target({ team: 'TEAMDEV123', designated: 'REQ-APP' }),
      currHelper: helperTarget({ team: 'TEAMDEV123', designated: 'REQ-HELP' }),
    }),
  ]
  for (const result of cases) {
    assert.ok(result.failures.length > 0, 'expected failures for coverage')
    for (const failure of result.failures) {
      assert.equal(isClassifiedTwoBuildFailure(failure), true, `unclassified two-build failure: ${failure}`)
    }
  }
})

// ---- RED: `codesign -d -r -` split-stream DR extraction ----
// Real Node spawnSync shape on the current bundle: status 0,
// stdout `# designated => cdhash H"..."`, stderr `Executable=/...`.
const SPLIT_STDOUT_DR = '# designated => cdhash H"aaa111bbb222ccc333"'
const SPLIT_STDERR_EXEC = 'Executable=/tmp/Intervjuare.app/Contents/MacOS/intervjuare'

test('(1) stdout actual DR is extracted/normalized while stderr Executable is ignored', () => {
  const parsed = parseDesignatedRequirement({
    status: 0,
    stdout: `${SPLIT_STDOUT_DR}\n`,
    stderr: `${SPLIT_STDERR_EXEC}\n`,
  })
  assert.equal(parsed, SPLIT_STDOUT_DR)
  assert.ok(parsed.includes('cdhash'), 'parsed DR must be the actual cdhash expression')
  assert.ok(!parsed.includes('Executable='), 'stderr Executable diagnostic must be ignored')
  // Normalization: surrounding whitespace is trimmed but the exact DR is kept.
  const padded = parseDesignatedRequirement({
    status: 0,
    stdout: `  ${SPLIT_STDOUT_DR}  \n`,
    stderr: `${SPLIT_STDERR_EXEC}\n`,
  })
  assert.equal(padded, SPLIT_STDOUT_DR)
})

test('(2) nonzero status or missing actual designated line yields empty/invalid even when stderr is nonempty', () => {
  // Nonzero inspection exit fails closed even though stderr is nonempty.
  assert.equal(
    parseDesignatedRequirement({ status: 1, stdout: SPLIT_STDOUT_DR, stderr: SPLIT_STDERR_EXEC }),
    '',
  )
  assert.equal(
    parseDesignatedRequirement({ status: 2, stdout: `${SPLIT_STDOUT_DR}\n`, stderr: `${SPLIT_STDERR_EXEC}\n` }),
    '',
  )
  // Missing stdout designated line fails closed despite nonempty stderr.
  assert.equal(parseDesignatedRequirement({ status: 0, stdout: '', stderr: SPLIT_STDERR_EXEC }), '')
  assert.equal(parseDesignatedRequirement({ status: 0, stdout: '\n', stderr: `${SPLIT_STDERR_EXEC}\n` }), '')
  // A carried nonzero designated exit fails the single-target gate even with a stale DR string.
  const failures = checkSingleTarget({
    verifyExit: 0,
    identifier: 'com.rafal.intervjuare',
    expectedIdentifier: 'com.rafal.intervjuare',
    designated: SPLIT_STDOUT_DR,
    signature: 'size=123',
    designatedExit: 1,
  })
  assert.ok(failures.some((f) => f.includes('designated requirement inspection failed')))
})

test('(3) exact DR drift remains rejected on extracted stdout DRs', () => {
  const prevDr = parseDesignatedRequirement({
    status: 0,
    stdout: '# designated => cdhash H"aaa111"\n',
    stderr: 'Executable=/tmp/prev.app/Contents/MacOS/intervjuare\n',
  })
  const currDr = parseDesignatedRequirement({
    status: 0,
    stdout: '# designated => cdhash H"bbb222"\n',
    stderr: 'Executable=/tmp/curr.app/Contents/MacOS/intervjuare\n',
  })
  assert.ok(prevDr.length > 0 && currDr.length > 0, 'fixtures must extract real DRs')
  const { failures } = evaluateStability({
    prevApp: target({ designated: prevDr }),
    currApp: target({ designated: currDr }),
    prevHelper: helperTarget({ designated: 'REQ-HELP' }),
    currHelper: helperTarget({ designated: 'REQ-HELP' }),
  })
  assert.ok(failures.some((f) => f.includes('app designated')), `expected app DR drift, got: ${JSON.stringify(failures)}`)
  // Identical extracted DRs still pass continuity.
  const stable = evaluateStability({
    prevApp: target({ designated: prevDr }),
    currApp: target({ designated: prevDr }),
    prevHelper: helperTarget({ designated: 'REQ-HELP' }),
    currHelper: helperTarget({ designated: 'REQ-HELP' }),
  })
  assert.deepEqual(stable.failures, [])
})

test('(4) path-only stderr can never satisfy the present-DR check', () => {
  const parsed = parseDesignatedRequirement({
    status: 0,
    stdout: '',
    stderr: 'Executable=/absolute/path/Intervjuare.app/Contents/MacOS/intervjuare\n',
  })
  assert.equal(parsed, '', 'path-only stderr must extract to empty DR')
  // The old buggy `(stderr || stdout).trim()` shape would have accepted the path.
  const buggy = (SPLIT_STDERR_EXEC || '').trim()
  assert.ok(buggy.length > 0 && buggy.startsWith('Executable='), 'buggy shape accepts the path diagnostic')
  const failures = checkSingleTarget({
    verifyExit: 0,
    identifier: 'com.rafal.intervjuare',
    expectedIdentifier: 'com.rafal.intervjuare',
    designated: parsed,
    signature: 'size=123',
  })
  assert.ok(failures.some((f) => f.includes('designated requirement is missing')))
  // Absolute executable paths are never compared: two different Executable
  // diagnostics with empty stdout both yield empty and never equal a real DR.
  const other = parseDesignatedRequirement({
    status: 0,
    stdout: '',
    stderr: 'Executable=/other/absolute/path/intervjuare\n',
  })
  assert.equal(other, '')
  assert.notEqual(other, SPLIT_STDOUT_DR)
})
