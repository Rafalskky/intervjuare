//! Loading and validation of the canonical candidate profile (`profile.yaml`).
//!
//! The raw YAML text is what the model receives (sending the whole profile is the
//! intended first version). Parsing here only validates the file and extracts
//! the profile ids that a suggestion may cite, so citations can be checked
//! against real profile entries.

use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use yaml_rust2::{Yaml, YamlLoader};

/// Generous bound for a human-written profile; protects the prompt and memory.
pub const MAX_PROFILE_BYTES: usize = 256 * 1024;

/// Top-level sections whose child keys are citable profile ids.
const CITABLE_SECTIONS: [&str; 5] = ["projects", "stories", "experience", "employment", "education"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub path: String,
    pub candidate_name: Option<String>,
    pub projects: Vec<String>,
    pub stories: Vec<String>,
    pub bytes: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub raw: String,
    pub summary: ProfileSummary,
    pub citable_ids: BTreeSet<String>,
}

/// `INTERVJUARE_PROFILE` wins; otherwise `profile.yaml` in the repository
/// root this binary was built from (this is a personal, built-from-source tool).
pub fn default_profile_path() -> PathBuf {
    if let Some(path) = std::env::var_os("INTERVJUARE_PROFILE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("profile.yaml")
}

pub fn load_profile(path: &Path) -> Result<Profile, String> {
    let file = fs::File::open(path).map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    let mut raw = String::new();
    file.take(MAX_PROFILE_BYTES as u64 + 1)
        .read_to_string(&mut raw)
        .map_err(|error| format!("Could not read {} as UTF-8 text: {error}", path.display()))?;
    if raw.len() > MAX_PROFILE_BYTES {
        return Err(format!("{} is larger than {} KiB.", path.display(), MAX_PROFILE_BYTES / 1024));
    }
    parse_profile(&path.display().to_string(), raw)
}

pub fn parse_profile(path: &str, raw: String) -> Result<Profile, String> {
    if raw.trim().is_empty() {
        return Err(format!("{path} is empty."));
    }
    let documents = YamlLoader::load_from_str(&raw).map_err(|error| format!("{path} is not valid YAML: {error}"))?;
    if documents.len() != 1 {
        return Err(format!("{path} must contain exactly one YAML document (found {}).", documents.len()));
    }
    let root = &documents[0];
    if root.as_hash().is_none() {
        return Err(format!("{path} must be a YAML mapping at the top level."));
    }

    let mut warnings = Vec::new();
    let candidate_name = match &root["candidate"] {
        Yaml::Hash(_) => root["candidate"]["name"].as_str().map(str::to_string),
        Yaml::BadValue => {
            warnings.push("Missing `candidate` section.".to_string());
            None
        }
        _ => {
            warnings.push("`candidate` should be a mapping.".to_string());
            None
        }
    };

    let mut citable_ids = BTreeSet::new();
    let mut projects = Vec::new();
    let mut stories = Vec::new();
    for section in CITABLE_SECTIONS {
        let node = &root[section];
        if node.is_badvalue() || matches!(node, Yaml::Null) {
            continue;
        }
        let Some(entries) = node.as_hash() else {
            warnings.push(format!("`{section}` should be a mapping of id → details."));
            continue;
        };
        for (key, value) in entries {
            let Some(id) = yaml_key(key) else {
                warnings.push(format!("`{section}` contains a non-text key."));
                continue;
            };
            if !citable_ids.insert(id.clone()) {
                warnings.push(format!("Id `{id}` is used in more than one section."));
            }
            if section == "stories" {
                check_story(&id, value, &mut warnings);
                stories.push(id);
            } else if section == "projects" {
                projects.push(id);
            }
        }
    }
    if stories.is_empty() {
        warnings.push("No `stories` found; personal answers will have no verified examples.".to_string());
    }

    let bytes = raw.len();
    Ok(Profile {
        raw,
        summary: ProfileSummary {
            path: path.to_string(),
            candidate_name,
            projects,
            stories,
            bytes,
            warnings,
        },
        citable_ids,
    })
}

fn yaml_key(key: &Yaml) -> Option<String> {
    match key {
        Yaml::String(value) => Some(value.clone()),
        Yaml::Integer(value) => Some(value.to_string()),
        _ => None,
    }
}

fn check_story(id: &str, value: &Yaml, warnings: &mut Vec<String>) {
    let Some(story) = value.as_hash() else {
        warnings.push(format!("Story `{id}` should be a mapping."));
        return;
    };
    let has_text = |field: &str| story.get(&Yaml::String(field.to_string())).is_some_and(|node| !matches!(node, Yaml::Null));
    if !["situation", "problem", "action", "result"].iter().any(|field| has_text(field)) {
        warnings.push(format!("Story `{id}` has none of situation/problem/action/result."));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
candidate:
  name: Rafael
projects:
  sagobo:
    type: personal_project
stories:
  password_reset_race_condition:
    project: sagobo
    situation: Password reset was implemented.
    action: Server-side protection was added.
  analytics_adblock:
    problem: Ad blockers blocked analytics.
"#;

    #[test]
    fn parses_summary_and_citable_ids() {
        let profile = parse_profile("profile.yaml", SAMPLE.to_string()).expect("valid profile");
        assert_eq!(profile.summary.candidate_name.as_deref(), Some("Rafael"));
        assert_eq!(profile.summary.projects, vec!["sagobo"]);
        assert_eq!(profile.summary.stories, vec!["password_reset_race_condition", "analytics_adblock"]);
        assert!(profile.summary.warnings.is_empty(), "{:?}", profile.summary.warnings);
        assert!(profile.citable_ids.contains("sagobo"));
        assert!(profile.citable_ids.contains("analytics_adblock"));
        assert_eq!(profile.raw, SAMPLE, "the model receives the unmodified text");
    }

    #[test]
    fn rejects_empty_invalid_and_non_mapping_files() {
        assert!(parse_profile("p", "  \n".into()).unwrap_err().contains("empty"));
        assert!(parse_profile("p", "candidate: [unclosed".into()).unwrap_err().contains("not valid YAML"));
        assert!(parse_profile("p", "- just\n- a list\n".into()).unwrap_err().contains("mapping"));
        assert!(parse_profile("p", "a: 1\n---\nb: 2\n".into()).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn warns_about_structural_problems_without_rejecting() {
        let profile = parse_profile(
            "p",
            "stories:\n  bare: just text\n  empty_story:\n    tags: [x]\nprojects: not-a-map\n".into(),
        )
        .expect("still usable");
        let warnings = profile.summary.warnings.join("\n");
        assert!(warnings.contains("Missing `candidate`"));
        assert!(warnings.contains("Story `bare` should be a mapping"));
        assert!(warnings.contains("Story `empty_story` has none"));
        assert!(warnings.contains("`projects` should be a mapping"));
    }

    #[test]
    fn load_profile_rejects_oversized_and_missing_files() {
        let workspace = crate::InferenceWorkspace::new().expect("workspace");
        let path = workspace.path.join("profile.yaml");
        fs::write(&path, format!("candidate:\n  name: x\nnotes: \"{}\"\n", "a".repeat(MAX_PROFILE_BYTES))).unwrap();
        assert!(load_profile(&path).unwrap_err().contains("larger than"));
        assert!(load_profile(&workspace.path.join("missing.yaml")).unwrap_err().contains("Could not open"));
        fs::write(&path, SAMPLE).unwrap();
        assert_eq!(load_profile(&path).expect("loads").summary.stories.len(), 2);
    }

    #[test]
    fn example_profile_in_repository_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../profile.example.yaml");
        let profile = load_profile(&path).expect("profile.example.yaml must stay valid");
        assert!(profile.summary.warnings.is_empty(), "{:?}", profile.summary.warnings);
        assert!(!profile.summary.stories.is_empty());
    }
}
