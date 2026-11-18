import { spawnSync } from 'node:child_process'
import { accessSync, constants } from 'node:fs'
import { resolve } from 'node:path'
import {
  ADHOC_DEVELOPER_FLAG,
  APP_IDENTIFIER,
  HELPER_IDENTIFIER,
  resolveBundleSigning,
} from './codesign-config.mjs'

const bundleArg = process.argv.find((arg) => arg.endsWith('.app'))
const bundlePath = resolve(bundleArg ?? '')
const entitlementsPath = resolve('src-tauri/Entitlements.plist')
const helperPath = resolve(bundlePath, 'Contents/MacOS/intervjuare-macos-bridge')

if (process.platform !== 'darwin') {
  throw new Error('macOS bundle signing is supported only on macOS')
}

if (!bundleArg) {
  throw new Error(
    `usage: INTERVJUARE_CODESIGN_IDENTITY="<identity>" node scripts/sign-macos-bundle.mjs <bundle.app>\n` +
      `developer-only ad-hoc escape hatch (WILL re-prompt for permissions, never a stable deliverable):\n` +
      `  INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1 node scripts/sign-macos-bundle.mjs <bundle.app> ${ADHOC_DEVELOPER_FLAG}`,
  )
}

for (const path of [bundlePath, helperPath, entitlementsPath]) {
  accessSync(path, constants.R_OK)
}

// FAIL CLOSED: without INTERVJUARE_CODESIGN_IDENTITY this throws with
// actionable guidance instead of silently ad-hoc signing. Ad-hoc is only
// reachable via the explicit developer-only flag + opt-in env var.
const { identity, developerOnlyAdhoc } = resolveBundleSigning(process.env, process.argv)

if (developerOnlyAdhoc) {
  console.warn(
    'WARNING: DEVELOPER-ONLY AD-HOC BUILD — this artifact uses a transient ad-hoc signature, ' +
      'its CDHash will change on rebuild, and macOS will repeatedly re-prompt for Microphone and ' +
      'Screen & System Audio Recording. It is NOT the stable permission-test deliverable.',
  )
} else {
  console.log(`Signing stable permission-test bundle with persistent identity (helper then app).`)
}

function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit' })
  if (result.error) throw result.error
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${result.status}`)
  }
}

// Helper first, then outer app: TCC-relevant executables share one persistent
// identity, existing identifiers, entitlements, and hardened runtime.
run('codesign', [
  '--force',
  '--sign',
  identity,
  '--identifier',
  HELPER_IDENTIFIER,
  '--options',
  'runtime',
  '--entitlements',
  entitlementsPath,
  helperPath,
])

run('codesign', [
  '--force',
  '--sign',
  identity,
  '--identifier',
  APP_IDENTIFIER,
  '--options',
  'runtime',
  '--entitlements',
  entitlementsPath,
  bundlePath,
])

run('codesign', ['--verify', '--deep', '--strict', '--verbose=4', bundlePath])
