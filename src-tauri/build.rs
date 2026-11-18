use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BRIDGE_NAME: &str = "intervjuare-macos-bridge";
const WHISPER_SERVER_NAME: &str = "whisper-server";
// Whisper model selection: test-only trial gate with a q5 default.
//
// - Default (production): the verified turbo-q5 checkpoint
//   `ggml-large-v3-turbo-q5_0.bin` (574,041,195 bytes, SHA-1
//   e050f7970618a659205450ad97eb95a18d69c9ee). The default build never
//   points at the full model.
// - Test-only trial: setting INTERVJUARE_WHISPER_MODEL_TRIAL to exactly
//   `full-large-v3` selects the full unquantized `ggml-large-v3.bin`
//   (3095033483 bytes) for a separately delivered, separately signed trial
//   bundle. Any other value fails closed.
// build.rs is the single source of truth and exposes the resolved name via
// cargo:rustc-env (INTERVJUARE_MODEL_NAME); the runtime consumes it via
// env!() and never duplicates the filename literal. Production stays q5:
// full large-v3 failed the M1 speed gate under process-per-inference and is
// not shippable as the default.
const DEFAULT_MODEL_NAME: &str = "ggml-large-v3-turbo-q5_0.bin";
// Exact verified byte size of the default turbo-q5 source asset. Staging
// fails closed when the source differs, so a truncated or partial download
// can never be staged and shadow a good model.
const DEFAULT_MODEL_LEN: u64 = 574_041_195;
const TRIAL_MODEL_ENV: &str = "INTERVJUARE_WHISPER_MODEL_TRIAL";
const TRIAL_MODEL_VALUE: &str = "full-large-v3";
const FULL_TRIAL_MODEL_FILE: &str = "ggml-large-v3.bin";
// Exact byte size of the full unquantized trial source asset. Like the
// default, staging fails closed on mismatch.
const FULL_TRIAL_MODEL_LEN: u64 = 3095033483;

fn resolve_model_selection() -> (String, u64) {
    println!("cargo:rerun-if-env-changed={TRIAL_MODEL_ENV}");
    match std::env::var(TRIAL_MODEL_ENV).as_deref() {
        Ok(TRIAL_MODEL_VALUE) => (FULL_TRIAL_MODEL_FILE.to_string(), FULL_TRIAL_MODEL_LEN),
        // Unset or explicitly empty means the default production model.
        Ok("") | Err(_) => (DEFAULT_MODEL_NAME.to_string(), DEFAULT_MODEL_LEN),
        Ok(other) => panic!(
            "INTERVJUARE_WHISPER_MODEL_TRIAL has unsupported value {other:?}: refusing to stage an unknown model. Unset it for the default turbo-q5 production model, or set it to exactly {TRIAL_MODEL_VALUE:?} for the test-only full-model trial."
        ),
    }
}

fn main() {
    // Deterministic switching between stable and developer-only ad-hoc modes:
    // Cargo must re-run this script when any signing-mode input changes.
    println!("cargo:rerun-if-env-changed=INTERVJUARE_CODESIGN_IDENTITY");
    println!("cargo:rerun-if-env-changed=INTERVJUARE_STABLE_PERMISSION_BUILD");
    println!("cargo:rerun-if-env-changed=INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD");
    let (model_name, expected_model_len) = resolve_model_selection();
    println!("cargo:rustc-env=INTERVJUARE_MODEL_NAME={model_name}");
    build_swift_bridge();
    prepare_whisper_runtime(&model_name, expected_model_len);
    tauri_build::build();
}

fn build_swift_bridge() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest_dir
        .join("../native/macos/Sources/IntervjuareMacOSBridge/main.swift");
    println!("cargo:rerun-if-changed={}", source.display());

    let target_dir = cargo_target_dir(&manifest_dir);
    let profile = env::var("PROFILE").expect("PROFILE");
    let output_dir = target_dir.join(profile);
    fs::create_dir_all(&output_dir).expect("create Cargo output directory for Swift bridge");
    let output = output_dir.join(BRIDGE_NAME);
    let target_triple = env::var("TARGET").expect("TARGET");
    let sidecar_source = manifest_dir.join(format!("{BRIDGE_NAME}-{target_triple}"));

    let status = Command::new("swiftc")
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .status()
        .expect("start swiftc; install Apple Command Line Tools");

    assert!(status.success(), "swiftc failed while building {}", source.display());
    let entitlements = manifest_dir.join("Entitlements.plist");
    sign_swift_bridge(&output, &entitlements);
    fs::copy(&output, &sidecar_source).expect("copy compiled Swift bridge for Tauri sidecar bundling");
    sign_swift_bridge(&sidecar_source, &entitlements);

}

fn sign_swift_bridge(binary: &Path, entitlements: &Path) {
    let identifier = "com.rafal.intervjuare.macos-bridge";
    let identity = resolve_bridge_signing_identity();
    // Helper and final bundle must share one persistent identity on the
    // stable permission-test path. Ad-hoc is developer-only and explicit.
    if identity == "-" {
        println!(
            "cargo:warning=DEVELOPER-ONLY AD-HOC bridge signing: transient signature will re-prompt for Microphone / Screen & System Audio Recording; not a stable permission-test deliverable"
        );
    }
    let status = Command::new("codesign")
        .args(["--force", "--sign", &identity, "--identifier", identifier, "--entitlements"])
        .arg(entitlements)
        .arg(binary)
        .status()
        .expect("start codesign for Swift bridge");
    assert!(
        status.success(),
        "could not sign {} as {identifier}",
        binary.display()
    );
}

// Stable-permission signing contract for the Swift bridge built by Cargo.
//
// - INTERVJUARE_CODESIGN_IDENTITY set to a persistent certificate identity:
//   use it (stable permission-test deliverable).
// - Otherwise, if INTERVJUARE_STABLE_PERMISSION_BUILD=1: FAIL CLOSED with an
//   actionable error instead of silently ad-hoc signing.
// - Otherwise, if INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1: ad-hoc sign the
//   bridge as an explicitly labeled developer-only iteration build.
// - Otherwise: FAIL CLOSED. Never silently fall back to ad-hoc.
fn resolve_bridge_signing_identity() -> String {
    let configured = std::env::var("INTERVJUARE_CODESIGN_IDENTITY")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if !configured.is_empty() {
        assert!(
            configured != "-",
            "INTERVJUARE_CODESIGN_IDENTITY must not be \"-\": ad-hoc signatures change CDHash on every rebuild and macOS TCC re-prompts; provide a persistent certificate identity"
        );
        return configured;
    }
    if std::env::var("INTERVJUARE_STABLE_PERMISSION_BUILD").as_deref() == Ok("1") {
        panic!(
            "INTERVJUARE_CODESIGN_IDENTITY is not set with INTERVJUARE_STABLE_PERMISSION_BUILD=1: refusing silent ad-hoc fallback. Set INTERVJUARE_CODESIGN_IDENTITY to the exact persistent Keychain identity (see docs/audio-pipeline.md \"Stable permissions\"), or set INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1 for a clearly labeled developer-only ad-hoc build that will re-prompt for permissions."
        );
    }
    if std::env::var("INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD").as_deref() == Ok("1") {
        return "-".to_string();
    }
    panic!(
        "INTERVJUARE_CODESIGN_IDENTITY is not set and INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD is not \"1\": refusing silent ad-hoc fallback. For the stable permission-test deliverable, set INTERVJUARE_CODESIGN_IDENTITY to the persistent Keychain identity; for throwaway iteration only, set INTERVJUARE_ALLOW_ADHOC_DEVELOPER_BUILD=1 (developer-only, will re-prompt for Microphone / Screen & System Audio Recording)."
    );
}

fn prepare_whisper_runtime(model_name: &str, expected_model_len: u64) {
    let manifest_dir = fs::canonicalize(PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"),
    ))
    .expect("canonical Cargo manifest directory");
    let root = manifest_dir.parent().expect("project root").to_path_buf();
    // Resolve a symlinked build (e.g. a git worktree reusing the main
    // checkout's whisper.cpp build): the binary's rpaths name the real path.
    let whisper_build_link = root.join("vendor/whisper.cpp/build");
    let whisper_build = fs::canonicalize(&whisper_build_link).unwrap_or(whisper_build_link);
    let server = whisper_build.join("bin/whisper-server");
    let model = root.join(format!("assets/models/{model_name}"));
    println!("cargo:rerun-if-changed={}", server.display());
    println!("cargo:rerun-if-changed={}", model.display());
    assert!(server.is_file(), "missing whisper.cpp server at {}; build vendor/whisper.cpp first", server.display());
    assert!(model.is_file(), "missing multilingual whisper model at {}", model.display());

    let output_dir = cargo_target_dir(&manifest_dir).join(env::var("PROFILE").expect("PROFILE"));
    let runtime_assets = output_dir.join("intervjuare-runtime");
    fs::create_dir_all(&output_dir).expect("create Cargo output directory for whisper runtime");
    fs::create_dir_all(&runtime_assets).expect("create prepared runtime asset directory");
    fs::copy(&server, output_dir.join(WHISPER_SERVER_NAME)).expect("copy whisper server beside application");
    fs::copy(&server, runtime_assets.join(WHISPER_SERVER_NAME)).expect("prepare bundled whisper server");
    make_whisper_runtime_relocatable(
        &output_dir.join(WHISPER_SERVER_NAME),
        &whisper_build,
    );
    make_whisper_runtime_relocatable(
        &runtime_assets.join(WHISPER_SERVER_NAME),
        &whisper_build,
    );
    remove_stale_whisper_cli_staging(&output_dir);
    remove_stale_whisper_cli_staging(&runtime_assets);
    remove_root_model_staging(&output_dir);
    remove_nonselected_model_staging(&output_dir, model_name);
    remove_nonselected_model_staging(&runtime_assets, model_name);
    stage_model_copy(&model, &runtime_assets.join(model_name), expected_model_len);

    for (library, bundled_name) in [
        ("src/libwhisper.1.8.2.dylib", "libwhisper.1.dylib"),
        ("ggml/src/libggml.dylib", "libggml.dylib"),
        ("ggml/src/libggml-base.dylib", "libggml-base.dylib"),
        ("ggml/src/libggml-cpu.dylib", "libggml-cpu.dylib"),
        ("ggml/src/ggml-blas/libggml-blas.dylib", "libggml-blas.dylib"),
        ("ggml/src/ggml-metal/libggml-metal.dylib", "libggml-metal.dylib"),
    ] {
        let source = whisper_build.join(library);
        assert!(source.is_file(), "missing whisper runtime library at {}", source.display());
        fs::copy(&source, output_dir.join(bundled_name)).expect("copy whisper runtime library");
        fs::copy(&source, runtime_assets.join(bundled_name)).expect("prepare bundled whisper runtime library");
    }
}

// Stage the model with a plain copy only. Hard links are forbidden here:
// shared inodes mean one truncating write zeroes the source asset and every
// staged name at once. Only the dedicated intervjuare-runtime directory
// receives the model; target/<profile> root must never hold a full model.
// The Tauri resources mapping copies intervjuare-runtime into
// Contents/Resources during bundling.
fn stage_model_copy(source: &Path, dest: &Path, expected_model_len: u64) {
    let source_len = fs::metadata(source)
        .expect("stat whisper source model")
        .len();
    assert_eq!(
        source_len, expected_model_len,
        "whisper source model has unexpected byte size (fail-closed): expected {expected_model_len}, found {source_len} at {}",
        source.display()
    );
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest).expect("replace stale staged whisper model");
    }
    let copied = fs::copy(source, dest).expect("copy whisper model into intervjuare-runtime staging");
    let dest_len = fs::metadata(dest)
        .expect("stat staged whisper model")
        .len();
    assert_eq!(
        copied, expected_model_len,
        "staged whisper model copied byte count mismatch (fail-closed)"
    );
    assert_eq!(
        dest_len, expected_model_len,
        "staged whisper model destination length mismatch (fail-closed)"
    );
    assert_eq!(
        dest_len, source_len,
        "staged whisper model source/destination length mismatch (fail-closed)"
    );
}

// Remove a stale model left in target/<profile> root by the retired
// hard-link staging. Only build-output root staging is touched; the source
// assets under assets/models are never modified here. Covers the default
// turbo-q5 name, the test-only full trial name, and retired names so no root
// model survives regardless of which asset a previous build staged.
fn remove_root_model_staging(output_dir: &Path) {
    for stale in [
        DEFAULT_MODEL_NAME,
        FULL_TRIAL_MODEL_FILE,
        "ggml-small-q5_1.bin",
        "ggml-large-v3-turbo.bin",
    ] {
        let root_model = output_dir.join(stale);
        if fs::symlink_metadata(&root_model).is_ok() {
            fs::remove_file(&root_model).expect("remove stale root whisper model; model lives only in intervjuare-runtime");
        }
    }
}

// Remove the retired whisper-cli staging now that inference goes through one
// persistent whisper-server per session. Only build-output staging is
// touched; vendor build output is never modified here.
fn remove_stale_whisper_cli_staging(dir: &Path) {
    for stale in ["whisper-cli"] {
        let path = dir.join(stale);
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).expect("remove stale whisper-cli staging; server is the runtime");
        }
    }
}

// Keep exactly one model in staging: the selected one. Every other known
// model name (the non-selected trial/default model and retired names) is
// removed so the wildcard Tauri `resources` entry cannot bundle a second
// model alongside the selected one. Only build-output staging directories
// are touched; source assets under assets/models are left alone.
fn remove_nonselected_model_staging(dir: &Path, selected: &str) {
    for known in [
        DEFAULT_MODEL_NAME,
        FULL_TRIAL_MODEL_FILE,
        "ggml-large-v3-turbo.bin",
        "ggml-small-q5_1.bin",
    ] {
        if known == selected {
            continue;
        }
        let path = dir.join(known);
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).expect("remove non-selected whisper model from staging");
        }
    }
    // Defensive: any other ggml-small* leftover from the retired family.
    if selected.starts_with("ggml-small") {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("ggml-small") && name.ends_with(".bin") {
            fs::remove_file(entry.path()).expect("remove stale small whisper model from staging");
        }
    }
}

fn make_whisper_runtime_relocatable(binary: &Path, build_dir: &Path) {
    for path in [
        build_dir.join("src"),
        build_dir.join("ggml/src"),
        build_dir.join("ggml/src/ggml-blas"),
        build_dir.join("ggml/src/ggml-metal"),
    ] {
        let status = Command::new("install_name_tool")
            .arg("-delete_rpath")
            .arg(path)
            .arg(binary)
            .status()
            .expect("start install_name_tool");
        assert!(status.success(), "could not remove development runtime path from {}", binary.display());
    }
    let status = Command::new("install_name_tool")
        .args(["-add_rpath", "@executable_path"])
        .arg(binary)
        .status()
        .expect("start install_name_tool");
    assert!(status.success(), "could not add local runtime path to {}", binary.display());
}

fn cargo_target_dir(manifest_dir: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                manifest_dir.join(path)
            }
        }
        None => manifest_dir.join("target"),
    }
}
