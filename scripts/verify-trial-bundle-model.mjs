// Read-only trial bundle model verification for Intervjuare.
//
// A bundle must contain EXACTLY ONE expected Whisper model with the EXACT
// expected byte size, plus whisper-server and the six required dylibs. The
// caller must state which model is expected; there is no default, so a q5
// check can never accidentally pass on a full trial bundle (or vice versa).
//
// Usage:
//   node scripts/verify-trial-bundle-model.mjs <bundle.app> --expect <q5|full> [--sha256 <hex> | --sha1 <hex>]
//
// Expectations:
//   q5   -> ggml-large-v3-turbo-q5_0.bin, 574041195 bytes
//   full -> ggml-large-v3.bin, 3095033483 bytes
//
// Known retired/other model names are rejected wherever they appear under
// Contents/Resources. Inspection only: never signs, never captures, never
// touches TCC. Hash verification (optional) streams the file; omit it in
// fixture tests.

import { createHash } from 'node:crypto'
import { createReadStream, readdirSync, statSync } from 'node:fs'
import { join, resolve } from 'node:path'

export const EXPECTED_MODELS = {
  q5: { file: 'ggml-large-v3-turbo-q5_0.bin', bytes: 574041195 },
  full: { file: 'ggml-large-v3.bin', bytes: 3095033483 },
}

export const KNOWN_OTHER_MODELS = [
  'ggml-small-q5_1.bin',
  'ggml-large-v3-turbo.bin',
  'ggml-large-v3-turbo-q5_0.bin',
  'ggml-large-v3.bin',
]

export const REQUIRED_RUNTIME_FILES = [
  'whisper-server',
  'libwhisper.1.dylib',
  'libggml.dylib',
  'libggml-base.dylib',
  'libggml-cpu.dylib',
  'libggml-blas.dylib',
  'libggml-metal.dylib',
]

export function evaluateBundleModel(entries, expect) {
  const failures = []
  const expected = EXPECTED_MODELS[expect]
  if (!expected) {
    return { ok: false, failures: [`unknown expectation ${JSON.stringify(expect)}: use --expect q5 or --expect full`] }
  }
  const byName = new Map(entries.map((entry) => [entry.name, entry.bytes]))
  const expectedBytes = byName.get(expected.file)
  if (expectedBytes === undefined) {
    failures.push(`expected model ${expected.file} is missing from bundle Resources`)
  } else if (expectedBytes !== expected.bytes) {
    failures.push(
      `expected model ${expected.file} has wrong byte size: got ${expectedBytes}, want ${expected.bytes}`,
    )
  }
  const bundledModels = entries.filter((entry) => entry.name.endsWith('.bin')).map((entry) => entry.name)
  if (bundledModels.length !== 1) {
    failures.push(`bundle must contain exactly one model file, found ${bundledModels.length}: ${bundledModels.join(', ') || '(none)'}`)
  } else if (bundledModels[0] !== expected.file) {
    failures.push(`bundled model is ${bundledModels[0]}, expected exactly ${expected.file}`)
  }
  for (const other of KNOWN_OTHER_MODELS.filter((name) => name !== expected.file)) {
    if (byName.has(other)) {
      failures.push(`non-selected model ${other} must not be bundled alongside ${expected.file}`)
    }
  }
  for (const required of REQUIRED_RUNTIME_FILES) {
    if (!byName.has(required)) {
      failures.push(`required runtime file ${required} is missing from bundle Resources`)
    }
  }
  return { ok: failures.length === 0, failures }
}

function hashFile(path, algorithm) {
  return new Promise((resolveHash, reject) => {
    const hash = createHash(algorithm)
    const stream = createReadStream(path)
    stream.on('data', (chunk) => hash.update(chunk))
    stream.on('error', reject)
    stream.on('end', () => resolveHash(hash.digest('hex')))
  })
}

function parseArgs(argv) {
  const bundleArg = argv.find((arg) => arg.endsWith('.app'))
  const expectIndex = argv.indexOf('--expect')
  const expect = expectIndex === -1 ? undefined : argv[expectIndex + 1]
  const sha256Index = argv.indexOf('--sha256')
  const sha1Index = argv.indexOf('--sha1')
  return {
    bundleArg,
    expect,
    sha256: sha256Index === -1 ? undefined : argv[sha256Index + 1],
    sha1: sha1Index === -1 ? undefined : argv[sha1Index + 1],
  }
}

const invokedDirectly =
  process.argv[1] !== undefined && resolve(process.argv[1]).endsWith('verify-trial-bundle-model.mjs')

if (invokedDirectly) {
  const { bundleArg, expect, sha256, sha1 } = parseArgs(process.argv)
  if (!bundleArg || (expect !== 'q5' && expect !== 'full')) {
    console.error('usage: node scripts/verify-trial-bundle-model.mjs <bundle.app> --expect <q5|full> [--sha256 <hex> | --sha1 <hex>]')
    process.exit(2)
  }
  if (sha256 && sha1) {
    console.error('use at most one of --sha256 / --sha1')
    process.exit(2)
  }
  const resourcesDir = resolve(bundleArg, 'Contents/Resources')
  let entries
  try {
    entries = readdirSync(resourcesDir).map((name) => ({ name, bytes: statSync(join(resourcesDir, name)).size }))
  } catch (error) {
    console.error(`cannot list bundle Resources at ${resourcesDir}: ${error.message}`)
    process.exit(1)
  }
  const verdict = evaluateBundleModel(entries, expect)
  for (const failure of verdict.failures) {
    console.error(`FAIL: ${failure}`)
  }
  const expected = EXPECTED_MODELS[expect]
  const done = async () => {
    if (verdict.ok && (sha256 || sha1)) {
      const algorithm = sha256 ? 'sha256' : 'sha1'
      const want = (sha256 ?? sha1).toLowerCase()
      const got = await hashFile(join(resourcesDir, expected.file), algorithm).catch((error) => {
        console.error(`FAIL: cannot hash ${expected.file}: ${error.message}`)
        process.exit(1)
      })
      if (got !== want) {
        console.error(`FAIL: ${expected.file} ${algorithm} mismatch: got ${got}, want ${want}`)
        process.exit(1)
      }
      console.log(`PASS: ${expected.file} ${algorithm} matches (${got.slice(0, 16)}…)`)
    }
    if (!verdict.ok) {
      console.error(`\nTrial bundle model verification FAILED for --expect ${expect}.`)
      process.exit(1)
    }
    console.log(`Trial bundle model verification passed: exactly one ${expected.file} (${expected.bytes} bytes), runtime files present.`)
  }
  await done()
}
