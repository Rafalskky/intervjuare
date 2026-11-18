// Pure, deterministic bundle-verification logic for Intervjuare stable permissions.
//
// Stable semantics (no codesign/TCC access here; pure fixtures only):
// - Apple Development / Developer ID: present stable TeamIdentifier + stable
//   exact designated requirement (DR), non-ad-hoc signature.
// - Self-signed Code Signing identity: NO TeamIdentifier, but non-ad-hoc
//   signature plus the same fixed identifiers and stable exact DR across two
//   builds (continuity is proven only by the two-build comparison).
// - Ad-hoc (`Signature=adhoc` or missing signature): NEVER stable, always FAIL.
// - CDHash equality is NEVER required: changed contents normally change CDHash.
// - App/helper designated requirements are INDEPENDENT: each target's DR must
//   be present and stable across builds, but app DR is never required to equal
//   helper DR (their identifiers differ).
//
// Real `codesign -d --verbose=4` format notes:
// - Ad-hoc bundles report a literal `Signature=adhoc` line (no Authority).
// - Certificate/CMS signatures report `Signature size=<positive>` plus one or
//   more `Authority=<identity>` lines (leaf first). They do NOT report a
//   `Signature=<value>` line, so parsers must not treat a missing `Signature=`
//   line as ad-hoc on its own.
// - Unsigned objects print `code object is not signed at all` with neither
//   marker: classified as missing/invalid and never accepted.
//
// Apple leaf-Authority limitation: when both app and helper carry the same
// TeamIdentifier, team equality is the enforced gate. Leaf-Authority equality
// is additionally enforced whenever both leaves are nonempty; if Authority
// lines were stripped from the inspection output, team equality alone passes
// and the mode notes the reduced assurance.

import { APP_IDENTIFIER, HELPER_IDENTIFIER } from './codesign-config.mjs'

export const DEVELOPER_ONLY_ADHOC_INFO_FLAG = '--developer-only-adhoc-info'

export function isDeveloperOnlyAdhocInfoMode(argv = process.argv) {
  return argv.includes(DEVELOPER_ONLY_ADHOC_INFO_FLAG)
}

export function normalizeTeam(team) {
  return (team ?? '').trim()
}

export function isMissingTeam(team) {
  const value = normalizeTeam(team)
  return value === '' || value === 'not set' || value === '(missing)'
}

export function isAdhocSignature(signature) {
  const value = (signature ?? '').trim().toLowerCase()
  // codesign reports `Signature=adhoc` for ad-hoc; a missing/empty signature
  // line means unsigned output and is also not a stable non-ad-hoc signature.
  return value === '' || value === '(missing)' || value === 'adhoc'
}

// Parse raw `codesign -d --verbose=4` output (stderr or stdout) into stable
// fields. Pure: no process access, safe for fixture tests.
//
// Classification:
// - explicit `Signature=adhoc`            => signatureKind 'adhoc'
// - `Signature size=<positive>` and/or one or more Authority lines
//                                            => signatureKind 'certificate'
// - neither                                => signatureKind 'missing' (unsigned/invalid)
export function parseDisplayOutput(output) {
  const text = output ?? ''
  const lines = text.split('\n')
  const get = (key) => {
    const line = lines.find((l) => l.startsWith(key))
    return line ? line.slice(key.length).trim() : ''
  }
  const authorities = lines
    .filter((l) => l.startsWith('Authority='))
    .map((l) => l.slice('Authority='.length).trim())
    .filter((v) => v.length > 0)
  const explicitSignature = get('Signature=')
  let signatureSize = null
  const sizeLine = lines.find((l) => l.startsWith('Signature size='))
  if (sizeLine) {
    const parsed = Number.parseInt(sizeLine.slice('Signature size='.length).trim(), 10)
    if (Number.isFinite(parsed)) signatureSize = parsed
  }
  let signatureKind
  let signature
  if (explicitSignature.toLowerCase() === 'adhoc') {
    signatureKind = 'adhoc'
    signature = 'adhoc'
  } else if ((signatureSize !== null && signatureSize > 0) || authorities.length > 0) {
    signatureKind = 'certificate'
    signature = signatureSize !== null && signatureSize > 0
      ? `size=${signatureSize}`
      : `cms:${authorities[0]}`
  } else {
    signatureKind = 'missing'
    signature = ''
  }
  return {
    identifier: get('Identifier='),
    team: get('TeamIdentifier='),
    cdhash: get('CDHash='),
    signature,
    signatureSize,
    authorities,
    signatureKind,
  }
}

// Leaf (signing) Authority/certificate identity: first Authority line.
// Accepts parsed display objects ({ authorities: [...] }) and legacy
// { authority: '...' } shapes; missing info yields ''.
export function leafAuthority(target) {
  if (!target) return ''
  if (Array.isArray(target.authorities)) return (target.authorities[0] ?? '').trim()
  return (target.authority ?? '').trim()
}

// Parse raw `codesign -d -r -` split-stream capture into the actual
// designated requirement (DR). Pure: no process access, safe for fixtures.
//
// Real behavior: exit status 0, stdout carries the actual requirement line
// (`# designated => cdhash H"..."`), while stderr carries only the
// `Executable=/absolute/path/...` diagnostic. The diagnostic must never
// satisfy a present-DR check and absolute executable paths are never
// compared.
//
// Fail-closed rules:
// - nonzero `status`                       => '' (invalid, even if stderr nonempty)
// - no `designated =>` line on stdout      => '' (path-only stderr never passes)
// - only stdout lines containing the actual `designated =>` marker are kept;
//   `Executable=` diagnostics are ignored even if they appear on stdout.
// - output is trimmed (normalized) but otherwise kept exact so DR drift is
//   still detected by exact string comparison downstream.
export function parseDesignatedRequirement(capture = {}) {
  // Accept either the capture object { status, stdout, stderr } or a raw
  // stdout string (treated as status 0 for convenience in fixtures).
  if (typeof capture === 'string') {
    capture = { status: 0, stdout: capture, stderr: '' }
  }
  const status = capture?.status
  if (typeof status === 'number' && status !== 0) return ''
  const stdout = capture?.stdout ?? ''
  const lines = String(stdout).split(/\r?\n/)
  const designated = lines
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .filter((line) => !line.startsWith('Executable='))
    .filter((line) => line.includes('designated =>'))
  if (designated.length === 0) return ''
  return designated.join('\n').trim()
}

export function checkSingleTarget({ verifyExit, identifier, expectedIdentifier, designated, signature, designatedExit }) {
  const failures = []
  if (verifyExit !== 0) failures.push('deep strict verification failed')
  if (identifier !== expectedIdentifier) {
    failures.push(`identifier is ${identifier || 'missing'}, expected ${expectedIdentifier}`)
  }
  // Fail closed on DR inspection errors: a nonzero `codesign -d -r -` exit
  // means extraction failed even if a stale/nonempty designated string was
  // carried alongside. `designatedExit` is optional so older fixtures without
  // it keep their existing semantics (undefined => no extra failure).
  if (typeof designatedExit === 'number' && designatedExit !== 0) {
    failures.push(`designated requirement inspection failed (exit ${designatedExit})`)
  }
  if (!designated || designated.length === 0) failures.push('designated requirement is missing')
  if (isAdhocSignature(signature)) failures.push('signature is ad-hoc (not a stable certificate signature)')
  return failures
}

// Single-bundle stable check: non-ad-hoc + identifiers + DR for app and
// helper, plus one-identity coherence: helper and app are signed with the
// same identity. TeamIdentifier presence is still reported via mode, but
// coherence is now enforced:
// - presence differs (one team, one missing) => mixed-team FAIL;
// - both present but different teams      => FAIL;
// - both missing (self-signed)            => same nonempty leaf Authority
//   required on app and helper, else FAIL;
// - both present and equal (Apple)        => same leaf Authority required
//   whenever both leaves are nonempty (best-effort; see limitation note).
// Two-build comparison is still needed to prove continuity.
export function evaluateSingleBundle({ app, helper }) {
  const failures = []
  for (const issue of checkSingleTarget({
    verifyExit: app.verifyExit,
    identifier: app.identifier,
    expectedIdentifier: APP_IDENTIFIER,
    designated: app.designated,
    signature: app.signature,
    designatedExit: app.designatedExit,
  })) {
    failures.push(`app: ${issue}`)
  }
  for (const issue of checkSingleTarget({
    verifyExit: helper.verifyExit,
    identifier: helper.identifier,
    expectedIdentifier: HELPER_IDENTIFIER,
    designated: helper.designated,
    signature: helper.signature,
    designatedExit: helper.designatedExit,
  })) {
    failures.push(`helper: ${issue}`)
  }
  const appSelfSigned = isMissingTeam(app.team)
  const helperSelfSigned = isMissingTeam(helper.team)
  if (appSelfSigned !== helperSelfSigned) {
    failures.push('app/helper TeamIdentifier presence differs (mixed-team bundle)')
  } else if (!appSelfSigned && normalizeTeam(app.team) !== normalizeTeam(helper.team)) {
    failures.push(
      `app/helper TeamIdentifier differs (app ${normalizeTeam(app.team)} vs helper ${normalizeTeam(helper.team)})`,
    )
  } else if (appSelfSigned && helperSelfSigned) {
    const appLeaf = leafAuthority(app)
    const helperLeaf = leafAuthority(helper)
    if (!appLeaf || !helperLeaf) {
      failures.push('self-signed certificate authority is missing on app/helper (no TeamIdentifier and no nonempty leaf Authority)')
    } else if (appLeaf !== helperLeaf) {
      failures.push(
        `self-signed certificate authority differs between app and helper (app "${appLeaf}" vs helper "${helperLeaf}")`,
      )
    }
  } else {
    const appLeaf = leafAuthority(app)
    const helperLeaf = leafAuthority(helper)
    if (appLeaf && helperLeaf && appLeaf !== helperLeaf) {
      failures.push(
        `app/helper leaf Authority differs (app "${appLeaf}" vs helper "${helperLeaf}")`,
      )
    }
  }
  const mode = appSelfSigned && helperSelfSigned
    ? 'self-signed (no TeamIdentifier; same leaf Authority required; continuity requires two-build DR comparison)'
    : appSelfSigned !== helperSelfSigned
      ? 'mixed-team (app/helper TeamIdentifier presence differs)'
      : `apple-certificate (TeamIdentifier ${normalizeTeam(app.team)})`
  return { failures, mode }
}

// Two-build stability: identifiers + exact DR must match per target (app DR
// is never required to equal helper DR); TeamIdentifier requires presence
// parity and equality (both missing = self-signed continuity via DR +
// authority; both present and equal = Apple continuity). The same leaf
// Authority/certificate identity must remain stable per target: strictly for
// self-signed continuity, best-effort (both leaves nonempty) for Apple.
// CDHash is compared only for reporting, never for pass/fail.
export function evaluateStability({ prevApp, currApp, prevHelper, currHelper }) {
  const failures = []
  if (prevApp.identifier !== currApp.identifier) {
    failures.push('app identifier drifted across the two builds')
  }
  if (prevHelper.identifier !== currHelper.identifier) {
    failures.push('helper identifier drifted across the two builds')
  }
  if (!currApp.designated || currApp.designated.length === 0 || prevApp.designated !== currApp.designated) {
    failures.push('app designated requirement is missing or drifted across the two builds')
  }
  if (!currHelper.designated || currHelper.designated.length === 0 || prevHelper.designated !== currHelper.designated) {
    failures.push('helper designated requirement is missing or drifted across the two builds')
  }
  const appPrevMissing = isMissingTeam(prevApp.team)
  const appCurrMissing = isMissingTeam(currApp.team)
  if (appPrevMissing !== appCurrMissing) {
    failures.push('app TeamIdentifier presence differs across the two builds')
  } else if (!appPrevMissing && normalizeTeam(prevApp.team) !== normalizeTeam(currApp.team)) {
    failures.push('app TeamIdentifier changed across the two builds')
  }
  const helperPrevMissing = isMissingTeam(prevHelper.team)
  const helperCurrMissing = isMissingTeam(currHelper.team)
  if (helperPrevMissing !== helperCurrMissing) {
    failures.push('helper TeamIdentifier presence differs across the two builds')
  } else if (!helperPrevMissing && normalizeTeam(prevHelper.team) !== normalizeTeam(currHelper.team)) {
    failures.push('helper TeamIdentifier changed across the two builds')
  }
  for (const [label, prev, curr] of [['app', prevApp, currApp], ['helper', prevHelper, currHelper]]) {
    const prevLeaf = leafAuthority(prev)
    const currLeaf = leafAuthority(curr)
    const selfSignedContinuity = isMissingTeam(prev.team) && isMissingTeam(curr.team)
    if (selfSignedContinuity) {
      if (!prevLeaf || !currLeaf) {
        failures.push(`${label} certificate authority is missing across the two builds (self-signed continuity requires a stable leaf Authority)`)
      } else if (prevLeaf !== currLeaf) {
        failures.push(`${label} certificate authority changed across the two builds`)
      }
    } else if (prevLeaf && currLeaf && prevLeaf !== currLeaf) {
      failures.push(`${label} leaf Authority changed across the two builds`)
    }
  }
  const cdhashChanged =
    prevApp.cdhash !== currApp.cdhash || prevHelper.cdhash !== currHelper.cdhash
  return { failures, cdhashChanged }
}

// Full two-build verification in pure form: BOTH bundles must independently
// pass the single-bundle gate (deep strict verification, expected
// identifiers, present DR, non-ad-hoc, one-identity coherence) before the
// continuity comparison runs. A corrupt or ad-hoc previous app/helper fails
// here even when the current bundle is valid.
//
// Every single-bundle failure is explicitly scoped: `app:`/`helper:` issues
// become `previous app:`/`previous helper:` (or `current ...`), while
// identity-coherence issues (which do not start with `app:`/`helper:`) become
// `previous bundle:`/`current bundle:`. No failure may remain unclassified,
// otherwise the CLI filter (previous/current/stability) would silently drop
// it and a mismatched-identity previous bundle could pass on per-target
// continuity alone.
function prefixScopedFailure(failure, scope) {
  if (failure.startsWith('app:')) return failure.replace(/^app:/, `${scope} app:`)
  if (failure.startsWith('helper:')) return failure.replace(/^helper:/, `${scope} helper:`)
  return `${scope} bundle: ${failure}`
}

export function isClassifiedTwoBuildFailure(failure) {
  return failure.startsWith('previous') || failure.startsWith('current') || failure.startsWith('stability')
}

export function evaluateTwoBuild({ prevApp, prevHelper, currApp, currHelper }) {
  const failures = []
  const prevSingle = evaluateSingleBundle({ app: prevApp, helper: prevHelper })
  for (const failure of prevSingle.failures) {
    failures.push(prefixScopedFailure(failure, 'previous'))
  }
  const currSingle = evaluateSingleBundle({ app: currApp, helper: currHelper })
  for (const failure of currSingle.failures) {
    failures.push(prefixScopedFailure(failure, 'current'))
  }
  const stability = evaluateStability({ prevApp, currApp, prevHelper, currHelper })
  for (const failure of stability.failures) {
    failures.push(`stability: ${failure}`)
  }
  return { failures, prevSingle, currSingle, stability }
}
