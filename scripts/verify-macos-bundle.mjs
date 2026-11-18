// Read-only macOS bundle verification for Intervjuare stable permissions.
//
// Uses only inspection commands; never signs, never modifies TCC, never
// requests permission, never creates/imports a certificate:
//
// - codesign --verify --deep --strict
// - codesign -d (identifier / TeamIdentifier / CDHash inspection)
// - codesign -d -r - (designated requirement inspection)
//
// Stable verification FAILS CLOSED: any ad-hoc app or helper signature exits
// nonzero. Accepted stable modes:
//
// - Apple certificate (Apple Development for local-only, Developer ID for
//   distribution): present stable TeamIdentifier + stable exact designated
//   requirement (DR), non-ad-hoc signature.
// - Self-signed Code Signing identity: NO TeamIdentifier, but a non-ad-hoc
//   signature plus the same fixed identifiers and stable exact DR across two
//   builds (continuity proven only by the two-build comparison).
//
// CDHash equality is NEVER required: changed contents normally change CDHash.
//
// Usage:
//   node scripts/verify-macos-bundle.mjs <bundle.app> [<previous-bundle.app>]
//
// With two bundles, verifies the previous bundle through the same full
// single-bundle gate (deep strict verification, identifiers, DR, non-ad-hoc,
// one-identity coherence) and then performs the stability comparison
// (identifiers, exact per-target DR, TeamIdentifier presence parity +
// equality, leaf-Authority stability; app/helper DRs are independent and
// CDHash equality is never required). With a single bundle, verifies
// non-ad-hoc + identifiers + DR + one-identity coherence and reports that a
// two-build comparison is still needed to prove continuity.
//
// Informational developer-only mode (never used by `npm run bundle:verify`):
//   node scripts/verify-macos-bundle.mjs <bundle.app> --developer-only-adhoc-info
// reports ad-hoc signature details as NOTE lines for local iteration only,
// but still exits nonzero: ad-hoc is never accepted as stable.
// It must not be confused with the stable check.

import { spawnSync } from 'node:child_process'
import { accessSync, constants } from 'node:fs'
import { resolve } from 'node:path'
import { APP_IDENTIFIER, HELPER_IDENTIFIER } from './codesign-config.mjs'
import {
  DEVELOPER_ONLY_ADHOC_INFO_FLAG,
  evaluateSingleBundle,
  evaluateTwoBuild,
  isDeveloperOnlyAdhocInfoMode,
  isMissingTeam,
  leafAuthority,
  parseDesignatedRequirement,
  parseDisplayOutput,
} from './verify-logic.mjs'

const [bundleArg, previousArg] = process.argv.slice(2).filter((arg) => arg.endsWith('.app'))
const developerOnlyAdhocInfo = isDeveloperOnlyAdhocInfoMode(process.argv)

if (process.platform !== 'darwin') {
  throw new Error('macOS bundle verification is supported only on macOS')
}
if (!bundleArg) {
  throw new Error(
    `usage: node scripts/verify-macos-bundle.mjs <bundle.app> [<previous-bundle.app>] [${DEVELOPER_ONLY_ADHOC_INFO_FLAG}]`,
  )
}

const bundlePath = resolve(bundleArg)
const helperPath = resolve(bundlePath, 'Contents/MacOS/intervjuare-macos-bridge')
const appExecutable = resolve(bundlePath, 'Contents/MacOS/intervjuare')
for (const path of [bundlePath, helperPath, appExecutable]) {
  accessSync(path, constants.R_OK)
}

function capture(command, args) {
  const result = spawnSync(command, args, { encoding: 'utf8' })
  return {
    status: result.status ?? 1,
    stdout: result.stdout ?? '',
    stderr: result.stderr ?? '',
  }
}

// Real `codesign -d --verbose=4` reports ad-hoc as `Signature=adhoc` but a
// certificate/CMS signature as `Signature size=<positive>` plus Authority
// lines (never `Signature=<value>`). Parsing delegates to the tested pure
// helper so valid Apple Development / Developer ID / self-signed bundles are
// classified as certificate signatures instead of missing/ad-hoc.
function parseDisplay(output) {
  return parseDisplayOutput(output)
}

function inspectTarget(label, path) {
  const verify = capture('codesign', ['--verify', '--deep', '--strict', '--verbose=4', path])
  // Designated requirement and display inspection are read-only.
  const designated = capture('codesign', ['-d', '-r', '-', path])
  const display = capture('codesign', ['-d', '--verbose=4', path])
  const info = parseDisplay(display.stderr || display.stdout)
  console.log(`\n== ${label}: ${path}`)
  console.log(`deep strict verify: exit ${verify.status}`)
  const tail = (verify.stderr || verify.stdout).trim().split('\n').slice(-4).join('\n')
  if (tail) console.log(tail)
  console.log(`identifier: ${info.identifier || '(missing)'}`)
  console.log(`team: ${info.team || '(missing)'}`)
  console.log(`cdhash: ${info.cdhash || '(missing)'}`)
  console.log(`signature: ${info.signature || '(missing)'} (${info.signatureKind})`)
  console.log(`leaf authority: ${leafAuthority(info) || '(missing)'}`)
  // `codesign -d -r -` splits streams: stderr carries only the
  // `Executable=/absolute/path/...` diagnostic while stdout carries the
  // actual `# designated => ...` requirement. Parse stdout only so a path
  // diagnostic can never satisfy the present-DR check; a nonzero inspection
  // exit yields an empty DR and fails closed downstream.
  const req = parseDesignatedRequirement(designated)
  console.log(`designated requirement: ${req || '(missing)'}`)
  if (!req) {
    console.log(`designated inspection: exit ${designated.status} (extraction failed; failing closed)`)
  }
  return { verifyExit: verify.status, designated: req, designatedExit: designated.status, ...info }
}

console.log('Intervjuare read-only bundle verification (no signing, no TCC changes).')
if (developerOnlyAdhocInfo) {
  console.log(
    `NOTE: ${DEVELOPER_ONLY_ADHOC_INFO_FLAG} is informational developer-only mode; ad-hoc findings are reported, not accepted as stable.`,
  )
}
const app = inspectTarget('app bundle', bundlePath)
const helper = inspectTarget('helper executable', helperPath)

let failed = false
const check = (ok, message) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}: ${message}`)
  if (!ok) failed = true
}

check(app.verifyExit === 0, 'app bundle passes codesign --verify --deep --strict')
check(helper.verifyExit === 0, 'helper passes deep strict verification as part of the bundle')
check(app.identifier === APP_IDENTIFIER, `app identifier is ${APP_IDENTIFIER} (got ${app.identifier || 'missing'})`)
check(helper.identifier === HELPER_IDENTIFIER, `helper identifier is ${HELPER_IDENTIFIER} (got ${helper.identifier || 'missing'})`)
check(Boolean(app.designated) && app.designated.length > 0, 'app designated requirement is present')
check(Boolean(helper.designated) && helper.designated.length > 0, 'helper designated requirement is present')

// Fail-closed ad-hoc gate (pure logic shared with fixture tests).
const single = evaluateSingleBundle({ app, helper })
const adhocFailures = single.failures.filter((message) => message.includes('ad-hoc'))
const otherFailures = single.failures.filter((message) => !message.includes('ad-hoc'))
for (const failure of otherFailures) {
  check(false, `stable check: ${failure}`)
}
if (adhocFailures.length > 0) {
  if (developerOnlyAdhocInfo) {
    for (const failure of adhocFailures) {
      console.log(`NOTE (developer-only ad-hoc info): ${failure}`)
    }
    // Honestly informational: details are shown as NOTEs, but ad-hoc is
    // never stable, so the run still fails closed and never reports a pass.
    check(false, 'developer-only ad-hoc info: ad-hoc signature is present and NOT stable (informational only, stable check not passed)')
  } else {
    for (const failure of adhocFailures) {
      check(false, `stable check: ${failure}`)
    }
  }
} else {
  check(true, 'app and helper signatures are non-ad-hoc (stable certificate path)')
}

// One-identity coherence (team + leaf Authority) is enforced inside
// evaluateSingleBundle and already reported above via `stable check` FAIL
// lines. This block only reports the mode and known limitations.
console.log(`identity mode: ${single.mode}`)
if (isMissingTeam(app.team) && isMissingTeam(helper.team)) {
  console.log('NOTE: no TeamIdentifier on app/helper: self-signed path. Same nonempty leaf Authority on app/helper is required; single-bundle non-ad-hoc + identifiers + DR is necessary but not sufficient; a two-build comparison is still needed to prove continuity.')
} else if (!isMissingTeam(app.team) && !isMissingTeam(helper.team)) {
  check(!isMissingTeam(app.team), `app TeamIdentifier present (${app.team || 'missing'})`)
  if (!leafAuthority(app) || !leafAuthority(helper)) {
    console.log('NOTE (Apple-mode limitation): Authority lines were unavailable for app/helper leaf comparison, so same-team equality is the enforced gate for this run.')
  }
}

if (!previousArg) {
  console.log('NOTE: single-bundle check only proves non-ad-hoc + identifiers + DR for this artifact; run with a previous bundle to prove continuity across two builds.')
}

if (previousArg) {
  const prevBundle = resolve(previousArg)
  const prevHelperPath = resolve(prevBundle, 'Contents/MacOS/intervjuare-macos-bridge')
  const prevExecutable = resolve(prevBundle, 'Contents/MacOS/intervjuare')
  accessSync(prevBundle, constants.R_OK)
  accessSync(prevHelperPath, constants.R_OK)
  accessSync(prevExecutable, constants.R_OK)
  console.log(`\n== two-build stability comparison`)
  console.log(`previous: ${prevBundle}`)
  console.log(`current:  ${bundlePath}`)
  // The previous bundle goes through the same read-only inspectTarget path:
  // deep strict verification, expected identifiers, present DR, and
  // non-ad-hoc are required before any continuity comparison.
  const prevApp = inspectTarget('previous app bundle', prevBundle)
  const prevHelper = inspectTarget('previous helper executable', prevHelperPath)
  console.log(`previous app identifier/team/cdhash: ${prevApp.identifier} / ${prevApp.team} / ${prevApp.cdhash}`)
  console.log(`current  app identifier/team/cdhash: ${app.identifier} / ${app.team} / ${app.cdhash}`)
  console.log(`previous helper identifier/team/cdhash: ${prevHelper.identifier} / ${prevHelper.team} / ${prevHelper.cdhash}`)
  console.log(`current  helper identifier/team/cdhash: ${helper.identifier} / ${helper.team} / ${helper.cdhash}`)
  const twoBuild = evaluateTwoBuild({
    prevApp,
    prevHelper,
    currApp: app,
    currHelper: helper,
  })
  const prevFailures = twoBuild.failures.filter((f) => f.startsWith('previous'))
  const currFailures = twoBuild.failures.filter((f) => f.startsWith('current'))
  const stabilityFailures = twoBuild.failures.filter((f) => f.startsWith('stability'))
  // Fail-closed against category loss: every two-build failure must carry an
  // explicit previous/current/stability prefix. Anything else is reported as
  // a failure rather than silently ignored.
  const unclassifiedFailures = twoBuild.failures.filter(
    (f) => !(f.startsWith('previous') || f.startsWith('current') || f.startsWith('stability')),
  )
  for (const failure of unclassifiedFailures) {
    check(false, `two-build unclassified gate (no category loss): ${failure}`)
  }
  if (unclassifiedFailures.length === 0) {
    check(true, 'two-build verdict contains no unclassified entries (no category loss)')
  }
  for (const failure of prevFailures) {
    check(false, `two-build previous-bundle gate: ${failure}`)
  }
  if (prevFailures.length === 0) {
    check(true, 'previous app/helper pass deep strict verification, identifiers, DR, non-ad-hoc, and identity coherence')
  }
  for (const failure of currFailures) {
    check(false, `two-build current-bundle gate: ${failure}`)
  }
  if (stabilityFailures.length === 0) {
    check(true, 'app/helper identifiers and exact designated requirements stable; TeamIdentifier parity+equality and leaf-Authority stability hold (app/helper DRs independent; CDHash never required)')
  } else {
    for (const failure of stabilityFailures) {
      check(false, `${failure}`)
    }
  }
  if (twoBuild.stability.cdhashChanged) {
    console.log('NOTE: CDHash changed between builds. CDHash equality is never required; changed contents normally change CDHash. Stability is proven by identifiers + exact DR (+ TeamIdentifier parity/equality + leaf-Authority stability).')
  } else {
    console.log('NOTE: CDHash identical across the two builds (bit-identical outputs).')
  }
}

if (failed) {
  console.error('\nVerification FAILED: see FAIL lines above.')
  process.exit(1)
}
console.log('\nVerification completed without failures (read-only; no TCC or signing changes).')
