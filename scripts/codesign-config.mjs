// Shared stable-permission code-signing configuration for Intervjuare.
//
// The supported permission-test/delivered build must use one explicitly
// configured persistent signing identity for both the helper and the app.
// It must FAIL CLOSED when no persistent identity is available and must
// never silently fall back to ad-hoc ("-").
//
// Environment contract:
// - INTERVJUARE_CODESIGN_IDENTITY: exact Keychain signing identity name, e.g.
//   "Apple Development: Name (TEAMID)" or "Developer ID Application: ...".
//   Required for the stable permission-test deliverable.
// - INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1: explicit opt-in for a clearly
//   labeled developer-only ad-hoc build. Without this, missing identity fails.
// - INTERVJUARE_STABLE_PERMISSION_BUILD=1: used by build.rs to fail closed
//   even for intermediate Cargo builds that participate in a stable bundle.

export const APP_IDENTIFIER = 'com.rafal.intervjuare'
export const HELPER_IDENTIFIER = 'com.rafal.intervjuare.macos-bridge'

export const ADHOC_DEVELOPER_FLAG = '--allow-adhoc-developer-only'

export function isAdhocDeveloperOptIn(env = process.env, argv = process.argv) {
  return env.INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD === '1' && argv.includes(ADHOC_DEVELOPER_FLAG)
}

export function resolveStableIdentity(env = process.env) {
  const raw = (env.INTERVJUARE_CODESIGN_IDENTITY ?? '').trim()
  if (!raw) {
    throw new Error(
      'INTERVJUARE_CODESIGN_IDENTITY is not set. ' +
        'The stable permission-test build FAILS CLOSED without a persistent signing identity. ' +
        'Set INTERVJUARE_CODESIGN_IDENTITY to the exact Keychain identity name ' +
        '(see docs/audio-pipeline.md "Stable permissions" section), or rebuild with the explicit ' +
        `developer-only escape hatch: INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1 ... ${ADHOC_DEVELOPER_FLAG}. ` +
        'Developer-only ad-hoc builds will re-prompt for Microphone / Screen & System Audio Recording.',
    )
  }
  if (raw === '-') {
    throw new Error(
      'INTERVJUARE_CODESIGN_IDENTITY must not be "-". Ad-hoc signatures change CDHash on every rebuild ' +
        'and macOS TCC treats each rebuild as a new identity, causing repeated permission prompts. ' +
        'Provide a persistent certificate identity instead.',
    )
  }
  return raw
}

// Returns { identity, developerOnlyAdhoc: boolean }. Throws on the stable
// path when no persistent identity is available.
export function resolveBundleSigning(env = process.env, argv = process.argv) {
  const raw = (env.INTERVJUARE_CODESIGN_IDENTITY ?? '').trim()
  if (raw && raw !== '-') {
    return { identity: raw, developerOnlyAdhoc: false }
  }
  if (raw === '-') {
    throw new Error(
      'INTERVJUARE_CODESIGN_IDENTITY must not be "-": refusing silent ad-hoc fallback on the stable path.',
    )
  }
  if (isAdhocDeveloperOptIn(env, argv)) {
    return { identity: '-', developerOnlyAdhoc: true }
  }
  return resolveStableIdentity(env) // throws with actionable guidance
}
