//! Local settings (`~/Library/Application Support/com.rafal.intervjuare/settings.json`).
//!
//! Holds the chosen ChatGPT account, model, profile path, job context and
//! answer preferences so they survive restarts. Local only, owner-only
//! permissions, written atomically. `profile.yaml` stays the separate source
//! of truth for candidate facts and is never copied here.

use crate::context::{AnswerMode, JobContext};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const SETTINGS_LIMIT: u64 = 256 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// ChatGPT account e-mail whose existing sign-in is used. Never guessed.
    pub account_email: Option<String>,
    pub model: String,
    /// Empty: model default. `none` is fastest.
    pub reasoning_effort: String,
    /// Model for "Låt oss se" screen questions; empty means `model`.
    pub screen_model: String,
    pub profile_path: Option<String>,
    pub job: JobContext,
    pub auto_answer: bool,
    pub answer_mode: AnswerMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            account_email: None,
            model: crate::llm::DEFAULT_MODEL.to_string(),
            reasoning_effort: crate::llm::DEFAULT_REASONING.to_string(),
            screen_model: String::new(),
            profile_path: None,
            job: JobContext::default(),
            auto_answer: true,
            answer_mode: AnswerMode::Normal,
        }
    }
}

impl Settings {
    pub fn screen_model(&self) -> &str {
        if self.screen_model.trim().is_empty() { &self.model } else { &self.screen_model }
    }
}

/// `INTERVJUARE_SETTINGS` overrides the location (tests, experiments).
pub fn settings_path() -> PathBuf {
    if let Some(path) = std::env::var_os("INTERVJUARE_SETTINGS").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/com.rafal.intervjuare/settings.json")
}

/// Missing file → defaults. A corrupt file is reported, not overwritten
/// until the owner changes a setting.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let text = match fs::File::open(path) {
        Ok(file) => {
            let mut text = String::new();
            match std::io::Read::read_to_string(&mut std::io::Read::take(file, SETTINGS_LIMIT), &mut text) {
                Ok(_) => text,
                Err(error) => return (Settings::default(), Some(format!("Could not read settings: {error}"))),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(error) => return (Settings::default(), Some(format!("Could not open settings: {error}"))),
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(settings) => (settings, None),
        Err(error) => (Settings::default(), Some(format!("Settings file {} is invalid ({error}); using defaults.", path.display()))),
    }
}

pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let directory = path.parent().ok_or("settings path has no directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|error| format!("could not create settings directory: {error}"))?;
    }
    let temporary = directory.join(format!(".settings-{}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(settings).map_err(|error| format!("could not encode settings: {error}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| format!("could not write settings: {error}"))?;
    file.write_all(&json)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("could not write settings: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("could not save settings: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Language;

    #[test]
    fn missing_file_gives_defaults_and_roundtrip_persists() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let path = workspace.path.join("nested/settings.json");
        let (settings, warning) = load(&path);
        assert_eq!(settings, Settings::default());
        assert!(warning.is_none());
        assert_eq!(settings.model, "gpt-6-luna");
        assert!(settings.auto_answer);
        assert_eq!(settings.screen_model(), "gpt-6-luna");

        let mut changed = settings.clone();
        changed.account_email = Some("me@example.com".into());
        changed.job.role = "Backend Developer".into();
        changed.job.language = Language::En;
        changed.answer_mode = AnswerMode::Star;
        changed.screen_model = "vision".into();
        save(&path, &changed).unwrap();
        assert_eq!(load(&path).0, changed);
        assert_eq!(changed.screen_model(), "vision");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupt_or_partial_files_are_handled() {
        let workspace = crate::InferenceWorkspace::new().unwrap();
        let path = workspace.path.join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        let (settings, warning) = load(&path);
        assert_eq!(settings, Settings::default());
        assert!(warning.unwrap().contains("invalid"));

        fs::write(&path, r#"{"accountEmail":"me@example.com"}"#).unwrap();
        let (settings, warning) = load(&path);
        assert!(warning.is_none());
        assert_eq!(settings.account_email.as_deref(), Some("me@example.com"));
        assert_eq!(settings.model, "gpt-6-luna", "missing fields take defaults");
    }
}
