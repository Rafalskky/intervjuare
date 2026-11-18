import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Stable-permission build contract: the supported permission-test/delivered
// path must use an explicitly configured persistent identity and must never
// silently fall back to ad-hoc ("-"). These checks read the pipeline as text
// so they fail deterministically without signing, capturing, or touching TCC.

const root = resolve(import.meta.dirname, '..')
const signScript = readFileSync(resolve(root, 'scripts/sign-macos-bundle.mjs'), 'utf8')
const signConfig = readFileSync(resolve(root, 'scripts/codesign-config.mjs'), 'utf8')
const buildRs = readFileSync(resolve(root, 'src-tauri/build.rs'), 'utf8')
const libRs = readFileSync(resolve(root, 'src-tauri/src/lib.rs'), 'utf8')
const tauriConf = JSON.parse(readFileSync(resolve(root, 'src-tauri/tauri.conf.json'), 'utf8'))

test('stable bundle identifiers are fixed and coherent', () => {
  assert.equal(tauriConf.identifier, 'com.rafal.intervjuare')
  // Identifiers live canonically in codesign-config.mjs; the sign script must
  // reference those constants (not drift to new literals).
  assert.match(signConfig, /com\.rafal\.intervjuare\.macos-bridge/)
  assert.match(signConfig, /APP_IDENTIFIER.*com\.rafal\.intervjuare/)
  assert.match(signScript, /HELPER_IDENTIFIER/)
  assert.match(signScript, /APP_IDENTIFIER/)
  assert.match(buildRs, /com\.rafal\.intervjuare\.macos-bridge/)
})

test('stable sign path requires INTERVJUARE_CODESIGN_IDENTITY and fails closed', () => {
  assert.match(signScript, /INTERVJUARE_CODESIGN_IDENTITY/, 'sign script must read an explicit identity input')
  assert.match(buildRs, /INTERVJUARE_CODESIGN_IDENTITY/, 'build.rs must read an explicit identity input')
  // A bare "--sign -" with no identity gate is a silent ad-hoc fallback.
  // The stable path must branch on the identity env var before signing.
  const signIndex = signScript.indexOf('INTERVJUARE_CODESIGN_IDENTITY')
  const adhocSignIndex = signScript.indexOf("'--sign'")
  assert.ok(signIndex !== -1 && adhocSignIndex !== -1 && signIndex < adhocSignIndex,
    'identity resolution must precede any codesign invocation')
})

test('stable sign path rejects ad-hoc identity and documents developer-only escape hatch', () => {
  // The stable path must treat "-" as an error, not a signature.
  // The literal flag value lives in codesign-config.mjs; the sign script must
  // reference that constant and label the escape hatch developer-only.
  assert.match(signConfig, /allow-adhoc-developer-only/, 'config must define the explicit ad-hoc flag value')
  assert.match(signScript, /ADHOC_DEVELOPER_FLAG/, 'sign script must reference the explicit ad-hoc flag')
  assert.match(signScript, /DEVELOPER-ONLY|developer-only/i, 'ad-hoc path must be labeled developer-only')
  assert.match(buildRs, /INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD/, 'build.rs ad-hoc escape hatch must be explicit')
  assert.match(buildRs, /STABLE|persistent|FAIL/i, 'build.rs must mention fail-closed stable behavior')
})

test('post-bundle signing preserves helper-then-app order with entitlements and deep verification', () => {
  const helperPos = signScript.indexOf('intervjuare-macos-bridge')
  const appPos = signScript.lastIndexOf('bundlePath')
  assert.ok(helperPos !== -1 && helperPos < appPos, 'helper must be signed before the outer app')
  assert.match(signScript, /Entitlements\.plist/)
  assert.match(signScript, /--verify.*--deep.*--strict|--strict.*--deep.*--verify/)
})

test('build.rs re-runs deterministically when signing-mode env changes', () => {
  for (const name of [
    'INTERVJUARE_CODESIGN_IDENTITY',
    'INTERVJUARE_STABLE_PERMISSION_BUILD',
    'INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD',
  ]) {
    assert.match(
      buildRs,
      new RegExp(`cargo:rerun-if-env-changed=${name}`),
      `build.rs must declare rerun-if-env-changed for ${name}`,
    )
  }
})

test('whisper model staging never hard-links and validates turbo-q5 size with server runtime fallback', () => {
  // Regression for the destructive hard-link defect: build.rs hard-linked the
  // source asset into target/<profile> root and intervjuare-runtime (same
  // inode, nlink=3), so a later truncating copy zeroed the source asset and
  // whisper failed with "invalid model data (bad magic)".
  assert.ok(
    !buildRs.includes('hard_link'),
    'build.rs must never use fs::hard_link for model staging; shared inodes let one truncating write zero every linked name',
  )
  assert.ok(
    !buildRs.includes('stage_immutable_model'),
    'build.rs must not keep the retired hard-link staging helper',
  )
  // Production default is exactly the verified turbo-q5 checkpoint: full
  // unquantized large-v3 failed the M1 speed gate under process-per-inference
  // and is not shippable as the default. The full model exists ONLY behind
  // the explicit test-only trial gate INTERVJUARE_WHISPER_MODEL_TRIAL.
  assert.match(
    buildRs,
    /(DEFAULT_)?MODEL_NAME:\s*&str\s*=\s*"ggml-large-v3-turbo-q5_0\.bin"/,
    'build.rs MODEL_NAME must select exactly ggml-large-v3-turbo-q5_0.bin as the default',
  )
  assert.ok(
    !buildRs.includes('"ggml-large-v3.bin"') || buildRs.includes('ggml-large-v3-turbo-q5_0.bin'),
    'build.rs must not select the full unquantized ggml-large-v3.bin as production model',
  )
  assert.match(
    buildRs,
    /INTERVJUARE_WHISPER_MODEL_TRIAL/,
    'build.rs trial gate must use an explicit env override, never a silent default',
  )
  assert.match(
    buildRs,
    /full-large-v3/,
    'build.rs trial gate must document the exact opt-in value',
  )
  assert.match(
    buildRs,
    /refusing to stage an unknown model/,
    'build.rs trial gate must fail closed on unsupported values',
  )
  assert.match(
    buildRs,
    /(remove_stale_root_model|remove_root_model_staging)/,
    'build.rs must remove any stale full model left in target/<profile> root',
  )
  assert.match(
    buildRs,
    /runtime_assets\.join\((MODEL_NAME|model_name)\)/,
    'build.rs must stage the model only into target/<profile>/intervjuare-runtime',
  )
  // Exact turbo-q5 byte-size fail-closed validation before staging.
  assert.match(
    buildRs,
    /574_?041_?195/,
    'build.rs must validate the exact turbo-q5 byte size (574,041,195 bytes) fail-closed before staging',
  )
  // The full large-v3 byte size exists only as the trial-gate validation
  // (exact opt-in, fail-closed); it must never be unconditional production
  // validation.
  assert.match(
    buildRs,
    /3095033483/,
    'build.rs trial gate must validate the exact full trial byte size (3095033483 bytes)',
  )
  assert.ok(
    !buildRs.match(/const EXPECTED_MODEL_LEN[^;]*3_?095_?033_?483/),
    'build.rs must not keep the full large-v3 size as unconditional production validation',
  )
  // Stale small and full-large staging files must be removed so the wildcard
  // Tauri resources entry cannot bundle a retired model.
  assert.match(
    buildRs,
    /ggml-small/,
    'build.rs must remove stale small-model staging files',
  )
  assert.match(
    buildRs,
    /ggml-large-v3\.bin|stale.*large|large.*stale/i,
    'build.rs must remove stale full large-v3 staging files',
  )
  // One persistent whisper-server per session is required to avoid dual Metal
  // contention; fresh CLI processes serialize pathologically. The runtime
  // stages whisper-server (plus dylibs); whisper-cli must not remain a
  // runtime dependency unless a tested fallback needs it.
  assert.match(
    buildRs,
    /whisper-server/,
    'build.rs must stage whisper-server for the persistent per-session server',
  )
  assert.ok(
    !libRs.includes('adjacent_artifact("whisper-cli")'),
    'runtime must not resolve whisper-cli; inference goes through the shared whisper-server',
  )
  assert.match(
    libRs,
    /whisper-server/,
    'runtime must resolve whisper-server fail-closed',
  )
  // Unbundled developer/test runtime must resolve the model from the
  // dedicated staging directory without reintroducing root staging.
  assert.match(
    libRs,
    /intervjuare-runtime/,
    'adjacent_artifact() must fall back to the intervjuare-runtime/<name> sibling for unbundled runtime',
  )
  assert.match(
    libRs,
    /Resources/,
    'adjacent_artifact() must preserve the adjacent bundled Resources lookup',
  )
})
