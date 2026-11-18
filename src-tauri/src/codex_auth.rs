//! Discovery of the ChatGPT (Codex) sign-ins already present on this Mac.
//!
//! The app does not run its own OAuth flow. It reads, read-only, the access
//! tokens that the Codex CLI/app (`~/.codex/auth.json`) and, if installed,
//! Hermes (`~/.hermes/auth.json`) keep fresh, and selects the one whose
//! ChatGPT account e-mail the owner chose. It never refreshes or writes those
//! files: OpenAI rotates refresh tokens, so refreshing someone else's token
//! would silently sign that tool out. Tokens are re-read for every request,
//! so a refresh done by the owning tool is picked up immediately.
//!
//! Token values never leave this module except as the bearer header of a
//! request; they are not serialized, logged or shown.

use serde::Serialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const AUTH_FILE_LIMIT: u64 = 512 * 1024;
/// Treat a token as expired slightly early so a request never starts with
/// one that dies mid-stream.
const EXPIRY_SKEW_SECS: u64 = 60;

#[derive(Clone)]
pub struct Account {
    pub email: String,
    pub plan: Option<String>,
    /// ChatGPT workspace id; personal Codex logins may have none.
    pub account_id: Option<String>,
    pub residency: Option<String>,
    pub expires_at: u64,
    pub source: String,
    /// 0 = Codex login (preferred), 1 = Hermes copy (fallback).
    pub source_rank: u8,
    pub access_token: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Account")
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("expires_at", &self.expires_at)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub email: String,
    pub plan: Option<String>,
    pub source: String,
    pub expires_at_ms: u64,
    pub expired: bool,
}

impl Account {
    pub fn is_expired(&self, now_secs: u64) -> bool {
        self.expires_at <= now_secs + EXPIRY_SKEW_SECS
    }

    pub fn summary(&self, now_secs: u64) -> AccountSummary {
        AccountSummary {
            email: self.email.clone(),
            plan: self.plan.clone(),
            source: self.source.clone(),
            expires_at_ms: self.expires_at * 1_000,
            expired: self.is_expired(now_secs),
        }
    }
}

pub fn now_secs() -> u64 {
    crate::now_ms() / 1_000
}

/// Codex honours `CODEX_HOME`, Hermes `HERMES_HOME`; both default under `$HOME`.
fn auth_files() -> Vec<(PathBuf, &'static str, u8)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |env: &str, default: &str| -> Option<PathBuf> {
        std::env::var_os(env)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(default)))
    };
    let mut files = Vec::new();
    if let Some(codex) = dir("CODEX_HOME", ".codex") {
        files.push((codex.join("auth.json"), "Codex", 0));
    }
    if let Some(hermes) = dir("HERMES_HOME", ".hermes") {
        files.push((hermes.join("auth.json"), "Hermes", 1));
    }
    files
}

/// Every usable sign-in found on disk, one per e-mail: a valid Codex login
/// wins over the Hermes copy, then the token that lives longest.
pub fn discover_accounts() -> Vec<Account> {
    let mut accounts = Vec::new();
    for (path, label, rank) in auth_files() {
        if let Some(json) = read_json(&path) {
            let mut found = accounts_from_json(&format!("{label} ({})", tilde(&path)), &json);
            found.iter_mut().for_each(|account| account.source_rank = rank);
            accounts.extend(found);
        }
    }
    dedupe(accounts, now_secs())
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let file = fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(AUTH_FILE_LIMIT).read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// Extracts ChatGPT access tokens from either file layout:
/// Codex `{tokens: {access_token}}` and Hermes
/// `{providers: {openai-codex: {tokens}}, credential_pool: {openai-codex: [..]}}`.
pub fn accounts_from_json(source: &str, json: &serde_json::Value) -> Vec<Account> {
    let mut candidates = vec![json.pointer("/tokens/access_token"), json.pointer("/providers/openai-codex/tokens/access_token")];
    if let Some(pool) = json.pointer("/credential_pool/openai-codex").and_then(|value| value.as_array()) {
        candidates.extend(pool.iter().map(|entry| entry.get("access_token")));
    }
    candidates
        .into_iter()
        .filter_map(|value| value.and_then(|value| value.as_str()))
        .filter_map(|token| account_from_token(source, token))
        .collect()
}

fn account_from_token(source: &str, token: &str) -> Option<Account> {
    let claims = jwt_claims(token)?;
    let auth = claims.get("https://api.openai.com/auth");
    // Only ChatGPT-issued tokens carry this claim namespace.
    auth?;
    let email = claims
        .pointer("/https:~1~1api.openai.com~1profile/email")
        .or_else(|| claims.get("email"))?
        .as_str()?
        .to_string();
    let text = |key: &str| auth.and_then(|auth| auth.get(key)).and_then(|value| value.as_str()).map(str::to_string);
    Some(Account {
        email,
        plan: text("chatgpt_plan_type"),
        account_id: text("chatgpt_account_id"),
        residency: text("chatgpt_data_residency").or_else(|| text("chatgpt_compute_residency")),
        expires_at: claims.get("exp")?.as_u64()?,
        source: source.to_string(),
        source_rank: 0,
        access_token: token.to_string(),
    })
}

/// Payload of a JWT, without signature verification: the server verifies
/// it; locally the claims only pick the account and detect expiry.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let mut standard: String = payload.chars().map(|c| match c { '-' => '+', '_' => '/', other => other }).collect();
    while standard.len() % 4 != 0 {
        standard.push('=');
    }
    let bytes = crate::decode_base64(standard.as_bytes(), 64 * 1024)?;
    serde_json::from_slice(&bytes).ok()
}

fn dedupe(accounts: Vec<Account>, now_secs: u64) -> Vec<Account> {
    let preference = |account: &Account| (!account.is_expired(now_secs), std::cmp::Reverse(account.source_rank), account.expires_at);
    let mut best: Vec<Account> = Vec::new();
    for account in accounts {
        match best.iter_mut().find(|other| other.email.eq_ignore_ascii_case(&account.email)) {
            Some(existing) if preference(existing) >= preference(&account) => {}
            Some(existing) => *existing = account,
            None => best.push(account),
        }
    }
    best.sort_by(|a, b| a.email.to_lowercase().cmp(&b.email.to_lowercase()));
    best
}

/// The sign-in for exactly the chosen e-mail. Never falls back to another
/// account: a different person's or workspace's quota must not be used
/// silently.
pub fn select(accounts: &[Account], email: Option<&str>, now_secs: u64) -> Result<Account, String> {
    let available = || {
        if accounts.is_empty() {
            "No ChatGPT/Codex sign-in was found on this Mac. Sign in with the Codex app or `codex login`.".to_string()
        } else {
            let list: Vec<String> = accounts
                .iter()
                .map(|account| format!("{} ({})", account.email, account.plan.as_deref().unwrap_or("unknown plan")))
                .collect();
            format!("Signed-in accounts on this Mac: {}.", list.join(", "))
        }
    };
    let Some(email) = email.map(str::trim).filter(|email| !email.is_empty()) else {
        return Err(format!("Choose which ChatGPT account Intervjuare should use. {}", available()));
    };
    let account = accounts
        .iter()
        .find(|account| account.email.eq_ignore_ascii_case(email))
        .ok_or_else(|| format!("No ChatGPT/Codex sign-in found for {email}. {}", available()))?;
    if account.is_expired(now_secs) {
        return Err(format!(
            "The ChatGPT sign-in for {email} has expired. Open the Codex app (or run `codex`) once so it refreshes the login, then try again."
        ));
    }
    Ok(account.clone())
}

#[cfg(test)]
pub fn test_token(email: &str, plan: &str, exp: u64) -> String {
    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..=chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * index)) & 63) as usize] as char);
            }
        }
        out
    }
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/profile": { "email": email },
        "https://api.openai.com/auth": { "chatgpt_account_id": format!("acct-{email}"), "chatgpt_plan_type": plan },
    });
    format!("{}.{}.sig", b64url(br#"{"alg":"none"}"#), b64url(payload.to_string().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn reads_codex_and_hermes_layouts() {
        let codex = serde_json::json!({ "tokens": { "access_token": test_token("me@example.com", "plus", NOW + 3_600) } });
        let hermes = serde_json::json!({
            "providers": { "openai-codex": { "tokens": { "access_token": test_token("me@example.com", "plus", NOW + 90_000) } } },
            "credential_pool": { "openai-codex": [
                { "access_token": test_token("other@example.com", "free", NOW + 90_000) },
                { "access_token": "not-a-jwt" }
            ] }
        });
        let mut found = accounts_from_json("Codex", &codex);
        let mut from_hermes = accounts_from_json("Hermes", &hermes);
        from_hermes.iter_mut().for_each(|account| account.source_rank = 1);
        found.extend(from_hermes);
        assert_eq!(found.len(), 3);
        let accounts = dedupe(found.clone(), NOW);
        assert_eq!(accounts.len(), 2);
        let me = accounts.iter().find(|account| account.email == "me@example.com").unwrap();
        assert_eq!(me.source, "Codex", "a valid Codex login wins over the Hermes copy");
        assert_eq!(me.plan.as_deref(), Some("plus"));
        assert_eq!(me.account_id.as_deref(), Some("acct-me@example.com"));

        let later = dedupe(found, NOW + 7_200);
        let me = later.iter().find(|account| account.email == "me@example.com").unwrap();
        assert_eq!(me.source, "Hermes", "an expired Codex token falls back to the valid copy");
    }

    #[test]
    fn non_chatgpt_tokens_are_skipped() {
        let token = format!("x.{}.y", "eyJleHAiOjF9"); // {"exp":1}, no ChatGPT claims
        assert!(account_from_token("t", &token).is_none());
    }

    #[test]
    fn selection_is_exact_and_never_falls_back() {
        let accounts = dedupe(
            vec![
                account_from_token("A", &test_token("me@example.com", "plus", NOW + 3_600)).unwrap(),
                account_from_token("B", &test_token("old@example.com", "plus", NOW + 30)).unwrap(),
            ],
            NOW,
        );
        assert_eq!(select(&accounts, Some(" ME@example.com "), NOW).unwrap().email, "me@example.com");
        let missing = select(&accounts, Some("someone@else.com"), NOW).unwrap_err();
        assert!(missing.contains("No ChatGPT/Codex sign-in found for someone@else.com"), "{missing}");
        assert!(missing.contains("me@example.com (plus)"), "lists what is available: {missing}");
        assert!(select(&accounts, None, NOW).unwrap_err().contains("Choose which ChatGPT account"));
        assert!(select(&accounts, Some("old@example.com"), NOW).unwrap_err().contains("expired"));
        assert!(select(&[], Some("me@example.com"), NOW).unwrap_err().contains("No ChatGPT/Codex sign-in was found"));
    }

    #[test]
    fn debug_output_never_contains_the_token() {
        let account = account_from_token("A", &test_token("me@example.com", "plus", NOW)).unwrap();
        let debug = format!("{account:?}");
        assert!(!debug.contains(&account.access_token));
        assert!(!serde_json::to_string(&account.summary(NOW)).unwrap().contains("sig"));
    }
}
