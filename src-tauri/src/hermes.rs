//! Hermes client: OpenAI-compatible streaming Chat Completions against the
//! local Hermes API server (`hermes gateway` with `API_SERVER_ENABLED=true`).
//!
//! Transport is macOS `/usr/bin/curl`, like the STT path, so no HTTP stack is
//! added to the app. The bearer key is passed on curl's stdin as a config
//! line (never in argv, where `ps` could show it). The request body holds
//! profile and transcript text, so it lives only in an owner-only RAII
//! workspace that is removed when the request ends. Cancellation kills curl;
//! Hermes interrupts the agent turn when the client disconnects.

use crate::context::ContextPackage;
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};

pub const DEFAULT_PORT: u16 = 8642;
pub const DEFAULT_MODEL: &str = "hermes-agent";
const ENV_FILE_LIMIT: u64 = 64 * 1024;
const ERROR_BODY_LIMIT: usize = 4 * 1024;
const STATUS_TRAILER: &str = "__INTERVJUARE_HTTP_STATUS__:";
const STREAM_MAX_SECS: &str = "120";
const CONNECT_TIMEOUT_SECS: &str = "2";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HermesConfig {
    /// Base URL including `/v1`, e.g. `http://127.0.0.1:8642/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub key_source: &'static str,
    pub model: String,
    pub reasoning_effort: String,
    /// `API_SERVER_ENABLED` as read from the Hermes `.env`, when present.
    pub api_server_enabled: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HermesConfigSummary {
    pub base_url: String,
    pub key_configured: bool,
    pub key_source: &'static str,
    pub model: String,
    pub api_server_enabled: Option<bool>,
}

impl HermesConfig {
    pub fn summary(&self) -> HermesConfigSummary {
        HermesConfigSummary {
            base_url: self.base_url.clone(),
            key_configured: self.api_key.is_some(),
            key_source: self.key_source,
            model: self.model.clone(),
            api_server_enabled: self.api_server_enabled,
        }
    }
}

/// Resolution order: `INTERVJUARE_HERMES_*` environment variables, then the
/// Hermes `.env` (`$HERMES_HOME/.env`, default `~/.hermes/.env`), then
/// Hermes defaults. A Finder-launched app has no shell environment, which is
/// why the Hermes `.env` is read directly instead of duplicating the key.
pub fn resolve_config() -> HermesConfig {
    let env_file = hermes_env_path()
        .and_then(|path| fs::File::open(path).ok())
        .and_then(|file| {
            let mut text = String::new();
            file.take(ENV_FILE_LIMIT).read_to_string(&mut text).ok().map(|_| text)
        })
        .map(|text| parse_env_file(&text))
        .unwrap_or_default();
    config_from(|name| std::env::var(name).ok(), &env_file)
}

fn hermes_env_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HERMES_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home).join(".env"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes/.env"))
}

pub fn config_from(env: impl Fn(&str) -> Option<String>, env_file: &HashMap<String, String>) -> HermesConfig {
    let non_empty = |value: Option<String>| value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
    let base_url = non_empty(env("INTERVJUARE_HERMES_URL"))
        .map(|url| url.trim_end_matches('/').to_string())
        .unwrap_or_else(|| {
            let host = non_empty(env_file.get("API_SERVER_HOST").cloned())
                .filter(|host| host != "0.0.0.0" && host != "::")
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = env_file
                .get("API_SERVER_PORT")
                .and_then(|port| port.trim().parse::<u16>().ok())
                .unwrap_or(DEFAULT_PORT);
            format!("http://{host}:{port}/v1")
        });
    let (api_key, key_source) = match non_empty(env("INTERVJUARE_HERMES_API_KEY")) {
        Some(key) => (Some(key), "INTERVJUARE_HERMES_API_KEY"),
        None => match non_empty(env_file.get("API_SERVER_KEY").cloned()) {
            Some(key) => (Some(key), "~/.hermes/.env API_SERVER_KEY"),
            None => (None, "not configured"),
        },
    };
    HermesConfig {
        base_url,
        api_key,
        key_source,
        model: non_empty(env("INTERVJUARE_HERMES_MODEL")).unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        // Measured on the owner's default Hermes model: disabling reasoning cut a
        // full one-shot answer from ~10.2 s to ~7.4 s with equally grounded output.
        reasoning_effort: non_empty(env("INTERVJUARE_HERMES_REASONING")).unwrap_or_else(|| "none".to_string()),
        api_server_enabled: env_file
            .get("API_SERVER_ENABLED")
            .map(|value| matches!(value.trim().to_lowercase().as_str(), "true" | "1" | "yes" | "on")),
    }
}

/// Minimal dotenv reader: `KEY=value`, optional `export`, quotes, comments.
pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let value = if let Some(inner) = value.strip_prefix('"').and_then(|rest| rest.split_once('"')) {
            inner.0.to_string()
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|rest| rest.split_once('\'')) {
            inner.0.to_string()
        } else {
            value.split(" #").next().unwrap_or("").trim().to_string()
        };
        values.insert(key.to_string(), value);
    }
    values
}

fn curl_config_header(api_key: &str) -> String {
    let escaped = api_key.replace('\\', "\\\\").replace('"', "\\\"");
    format!("header = \"Authorization: Bearer {escaped}\"\n")
}

pub fn request_body(config: &HermesConfig, package: &ContextPackage) -> serde_json::Value {
    serde_json::json!({
        "model": config.model,
        "stream": true,
        "messages": [
            { "role": "system", "content": package.system },
            { "role": "user", "content": package.user },
        ],
        "model_options": { "reasoning_effort": config.reasoning_effort },
    })
}

/// Shared between the generation thread and whoever cancels it.
#[derive(Default)]
pub struct CancelToken {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamOutcome {
    Completed { finish_reason: Option<String> },
    Cancelled,
}

/// One decoded item from the SSE byte stream.
#[derive(Debug, PartialEq, Eq)]
enum SseItem {
    Content(String),
    Finish { reason: String, error: Option<String> },
    Error(String),
    Done,
}

/// Line-oriented SSE decoder for OpenAI chat chunks. Named events (Hermes
/// `hermes.tool.progress`, `hermes.status`, …) and comments are ignored;
/// reasoning deltas never reach the answer.
#[derive(Default)]
struct SseDecoder {
    event: Option<String>,
    data: Vec<String>,
    http_status: Option<u16>,
    non_sse: String,
}

impl SseDecoder {
    fn push_line(&mut self, line: &str) -> Vec<SseItem> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(code) = line.strip_prefix(STATUS_TRAILER) {
            self.http_status = code.trim().parse().ok();
            return self.dispatch();
        }
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return Vec::new();
        }
        if let Some(name) = line.strip_prefix("event:") {
            self.event = Some(name.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data.push(data.strip_prefix(' ').unwrap_or(data).to_string());
        } else if self.non_sse.len() < ERROR_BODY_LIMIT {
            self.non_sse.push_str(line);
            self.non_sse.push('\n');
        }
        Vec::new()
    }

    fn dispatch(&mut self) -> Vec<SseItem> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data).join("\n");
        if data.is_empty() || event.as_deref().is_some_and(|name| name != "message") {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            return vec![SseItem::Done];
        }
        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(&data) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if let Some(message) = chunk.get("error").and_then(error_message) {
            if chunk.pointer("/choices/0").is_none() {
                return vec![SseItem::Error(message)];
            }
        }
        let choice = &chunk["choices"][0];
        if let Some(content) = choice.pointer("/delta/content").and_then(|value| value.as_str()) {
            if !content.is_empty() {
                items.push(SseItem::Content(content.to_string()));
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(|value| value.as_str()) {
            items.push(SseItem::Finish {
                reason: reason.to_string(),
                error: chunk.get("error").and_then(error_message),
            });
        }
        items
    }

    fn error_body_message(&self) -> Option<String> {
        let body = self.non_sse.trim();
        if body.is_empty() {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("error").and_then(error_message))
            .or_else(|| Some(body.chars().take(300).collect()))
    }
}

fn error_message(error: &serde_json::Value) -> Option<String> {
    match error {
        serde_json::Value::String(message) => Some(message.clone()),
        serde_json::Value::Object(map) => map.get("message").and_then(|value| value.as_str()).map(str::to_string),
        _ => None,
    }
}

/// Streams one Chat Completion. `on_delta` receives answer text in order.
pub fn stream_chat(
    config: &HermesConfig,
    package: &ContextPackage,
    token: &CancelToken,
    curl_bin: &str,
    mut on_delta: impl FnMut(&str),
) -> Result<StreamOutcome, String> {
    let Some(api_key) = config.api_key.as_deref() else {
        return Err("Hermes API key is not configured (API_SERVER_KEY in ~/.hermes/.env or INTERVJUARE_HERMES_API_KEY).".into());
    };
    let workspace = crate::InferenceWorkspace::new()?;
    let body_path = workspace.path.join("hermes-request.json");
    let stderr_path = workspace.path.join("curl-stderr.txt");
    fs::write(&body_path, request_body(config, package).to_string())
        .map_err(|error| format!("could not prepare Hermes request: {error}"))?;
    let stderr_file = fs::File::create(&stderr_path).map_err(|error| format!("could not prepare curl log: {error}"))?;

    let url = format!("{}/chat/completions", config.base_url);
    let data_arg = format!("@{}", body_path.display());
    let write_out = format!("\n{STATUS_TRAILER}%{{http_code}}\n");
    let mut child = Command::new(curl_bin)
        .args([
            "--silent",
            "--show-error",
            "--no-buffer",
            "--connect-timeout",
            CONNECT_TIMEOUT_SECS,
            "--max-time",
            STREAM_MAX_SECS,
            "--config",
            "-",
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: text/event-stream",
            // Bodies over 1 KiB otherwise wait up to 1 s for `100 Continue`.
            "-H",
            "Expect:",
            "--data-binary",
            &data_arg,
            "--write-out",
            &write_out,
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| format!("could not start curl for Hermes: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("curl stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("curl stdout unavailable")?;
    if let Ok(mut guard) = token.child.lock() {
        *guard = Some(child);
    }
    if token.is_cancelled() {
        token.cancel();
    }
    let config_write = stdin.write_all(curl_config_header(api_key).as_bytes());
    drop(stdin);

    let mut decoder = SseDecoder::default();
    let mut finish_reason = None;
    let mut stream_error = None;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                stream_error.get_or_insert(format!("reading Hermes stream failed: {error}"));
                break;
            }
        }
        if token.is_cancelled() {
            break;
        }
        for item in decoder.push_line(&line) {
            match item {
                SseItem::Content(text) => on_delta(&text),
                SseItem::Finish { reason, error } => {
                    if matches!(reason.as_str(), "error" | "cancelled") {
                        stream_error = Some(error.unwrap_or_else(|| format!("Hermes finished with `{reason}`.")));
                    }
                    finish_reason = Some(reason);
                }
                SseItem::Error(message) => stream_error = Some(message),
                SseItem::Done => {}
            }
        }
    }
    decoder.push_line("");

    let status = token
        .child
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .map(|mut child| child.wait());
    if token.is_cancelled() {
        return Ok(StreamOutcome::Cancelled);
    }
    if let Err(error) = config_write {
        return Err(format!("could not pass Hermes credentials to curl: {error}"));
    }
    let exit_code = status.and_then(|status| status.ok()).and_then(|status| status.code());
    match decoder.http_status {
        Some(200) => {}
        Some(0) | None => {
            let detail = fs::read_to_string(&stderr_path).unwrap_or_default();
            let detail = detail.trim();
            return Err(match exit_code {
                Some(7) => format!("Hermes is not reachable at {} (is `hermes gateway` running with API_SERVER_ENABLED=true?).", config.base_url),
                Some(28) => "Hermes request timed out.".to_string(),
                _ if !detail.is_empty() => format!("Hermes request failed: {detail}"),
                _ => format!("Hermes request failed (curl exit {exit_code:?})."),
            });
        }
        Some(code) => {
            let detail = decoder.error_body_message().unwrap_or_default();
            return Err(match code {
                401 | 403 => format!("Hermes rejected the API key (HTTP {code}). Check API_SERVER_KEY."),
                _ => format!("Hermes returned HTTP {code}: {detail}"),
            });
        }
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    Ok(StreamOutcome::Completed { finish_reason })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HermesStatus {
    pub reachable: bool,
    pub authorized: bool,
    pub message: String,
    pub latency_ms: u64,
    pub config: HermesConfigSummary,
}

/// Cheap connectivity + auth probe (`GET /v1/models`), bounded to ~3 s.
pub fn check_status(config: &HermesConfig, curl_bin: &str) -> HermesStatus {
    let started = Instant::now();
    let result = probe_models(config, curl_bin);
    let latency_ms = started.elapsed().as_millis() as u64;
    let (reachable, authorized, message) = match result {
        Ok(200) => (true, true, "Hermes API server is reachable and the key is accepted.".to_string()),
        Ok(401 | 403) => (true, false, "Hermes is reachable but rejected the API key. Check API_SERVER_KEY.".to_string()),
        Ok(code) => (true, false, format!("Hermes answered HTTP {code} on /models.")),
        Err(message) => (false, false, message),
    };
    HermesStatus { reachable, authorized, message, latency_ms, config: config.summary() }
}

fn probe_models(config: &HermesConfig, curl_bin: &str) -> Result<u16, String> {
    let url = format!("{}/models", config.base_url);
    let mut child = Command::new(curl_bin)
        .args([
            "--silent",
            "--connect-timeout",
            CONNECT_TIMEOUT_SECS,
            "--max-time",
            "3",
            "--config",
            "-",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start curl: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let header = config.api_key.as_deref().map(curl_config_header).unwrap_or_default();
        let _ = stdin.write_all(header.as_bytes());
    }
    let output = child.wait_with_output().map_err(|error| format!("curl failed: {error}"))?;
    let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
    if code == 0 {
        let hint = match config.api_server_enabled {
            Some(true) => "API_SERVER_ENABLED is set; restart `hermes gateway` so it starts the API server.",
            _ => "Enable it: add API_SERVER_ENABLED=true and API_SERVER_KEY=… to ~/.hermes/.env, then restart `hermes gateway`.",
        };
        return Err(format!("Hermes API server is not reachable at {}. {hint}", config.base_url));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{mpsc, Arc},
        thread,
        time::Duration,
    };

    fn test_config(base_url: String, key: Option<&str>) -> HermesConfig {
        HermesConfig {
            base_url,
            api_key: key.map(str::to_string),
            key_source: "test",
            model: DEFAULT_MODEL.into(),
            reasoning_effort: "low".into(),
            api_server_enabled: Some(true),
        }
    }

    fn package() -> ContextPackage {
        ContextPackage { system: "rules".into(), user: "Hur testade du det?".into() }
    }

    #[test]
    fn env_file_parsing_handles_quotes_exports_and_comments() {
        let values = parse_env_file(
            "# comment\nexport API_SERVER_ENABLED=true\nAPI_SERVER_KEY=\"se cret\" # trailing\nAPI_SERVER_PORT=8650 # port\nBAD LINE\nQUOTED='x=y'\n",
        );
        assert_eq!(values.get("API_SERVER_ENABLED").map(String::as_str), Some("true"));
        assert_eq!(values.get("API_SERVER_KEY").map(String::as_str), Some("se cret"));
        assert_eq!(values.get("API_SERVER_PORT").map(String::as_str), Some("8650"));
        assert_eq!(values.get("QUOTED").map(String::as_str), Some("x=y"));
        assert!(!values.contains_key("BAD LINE"));
    }

    #[test]
    fn config_prefers_app_env_then_hermes_env_then_defaults() {
        let empty = HashMap::new();
        let defaults = config_from(|_| None, &empty);
        assert_eq!(defaults.base_url, "http://127.0.0.1:8642/v1");
        assert_eq!(defaults.api_key, None);
        assert_eq!(defaults.model, "hermes-agent");
        assert_eq!(defaults.reasoning_effort, "none", "latency first");
        assert_eq!(defaults.api_server_enabled, None);

        let file = parse_env_file("API_SERVER_KEY=file-key\nAPI_SERVER_PORT=8650\nAPI_SERVER_HOST=0.0.0.0\nAPI_SERVER_ENABLED=false\n");
        let from_file = config_from(|_| None, &file);
        assert_eq!(from_file.base_url, "http://127.0.0.1:8650/v1", "wildcard bind is reached via loopback");
        assert_eq!(from_file.api_key.as_deref(), Some("file-key"));
        assert_eq!(from_file.api_server_enabled, Some(false));

        let overridden = config_from(
            |name| match name {
                "INTERVJUARE_HERMES_URL" => Some("http://127.0.0.1:9000/v1/".into()),
                "INTERVJUARE_HERMES_API_KEY" => Some("env-key".into()),
                _ => None,
            },
            &file,
        );
        assert_eq!(overridden.base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(overridden.api_key.as_deref(), Some("env-key"));
        assert_eq!(overridden.key_source, "INTERVJUARE_HERMES_API_KEY");
    }

    #[test]
    fn curl_config_escapes_the_key() {
        assert_eq!(curl_config_header("a\"b\\c"), "header = \"Authorization: Bearer a\\\"b\\\\c\"\n");
    }

    #[test]
    fn sse_decoder_extracts_content_and_ignores_named_events_and_reasoning() {
        let mut decoder = SseDecoder::default();
        let mut items = Vec::new();
        let stream = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
: keepalive\n\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n\
event: hermes.tool.progress\ndata: {\"tool\":\"web\",\"choices\":[{\"delta\":{\"content\":\"NOT ANSWER\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"ANCHORS\\n\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"- x\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
        for line in stream.split_inclusive('\n') {
            items.extend(decoder.push_line(line));
        }
        assert_eq!(
            items,
            vec![
                SseItem::Content("ANCHORS\n".into()),
                SseItem::Content("- x".into()),
                SseItem::Finish { reason: "stop".into(), error: None },
                SseItem::Done,
            ]
        );
    }

    #[test]
    fn sse_decoder_reports_errors_and_http_status() {
        let mut decoder = SseDecoder::default();
        let items = [
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}],\"error\":{\"message\":\"boom\"}}\n",
            "\n",
        ]
        .iter()
        .flat_map(|line| decoder.push_line(line))
        .collect::<Vec<_>>();
        assert_eq!(items, vec![SseItem::Finish { reason: "error".into(), error: Some("boom".into()) }]);

        let mut decoder = SseDecoder::default();
        decoder.push_line("{\"error\":{\"message\":\"Invalid API key\"}}\n");
        decoder.push_line(&format!("{STATUS_TRAILER}401\n"));
        assert_eq!(decoder.http_status, Some(401));
        assert_eq!(decoder.error_body_message().as_deref(), Some("Invalid API key"));
    }

    /// Minimal one-shot HTTP server. Returns the request head and body it saw.
    fn serve_once(response_head: &'static str, chunks: Vec<(&'static str, u64)>) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; content_length];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            let _ = sender.send((head, String::from_utf8(body).unwrap()));
            let mut stream = stream;
            let _ = stream.write_all(response_head.as_bytes());
            for (chunk, delay_ms) in chunks {
                thread::sleep(Duration::from_millis(delay_ms));
                if stream.write_all(chunk.as_bytes()).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
        });
        (base, receiver)
    }

    const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

    #[test]
    fn streams_through_real_curl_with_key_on_stdin() {
        let (base, seen) = serve_once(
            SSE_HEAD,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"ANCHORS\\n- a\\n\"}}]}\n\n", 0),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"ANSWER\\nHej\"}}]}\n\n", 20),
                ("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", 0),
            ],
        );
        let mut text = String::new();
        let outcome = stream_chat(&test_config(base, Some("k3y")), &package(), &CancelToken::default(), crate::CURL_BIN, |delta| {
            text.push_str(delta)
        })
        .expect("stream succeeds");
        assert_eq!(outcome, StreamOutcome::Completed { finish_reason: Some("stop".into()) });
        assert_eq!(text, "ANCHORS\n- a\nANSWER\nHej");
        let (head, body) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("POST /v1/chat/completions"), "{head}");
        assert!(head.contains("Authorization: Bearer k3y"));
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][1]["content"], "Hur testade du det?");
        assert_eq!(body["model_options"]["reasoning_effort"], "low");
    }

    #[test]
    fn maps_http_errors_and_unreachable_server() {
        let (base, _seen) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 39\r\nConnection: close\r\n\r\n",
            vec![("{\"error\":{\"message\":\"Invalid API key\"}}", 0)],
        );
        let error = stream_chat(&test_config(base, Some("bad")), &package(), &CancelToken::default(), crate::CURL_BIN, |_| {}).unwrap_err();
        assert!(error.contains("rejected the API key"), "{error}");

        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", unused.local_addr().unwrap());
        drop(unused);
        let error = stream_chat(&test_config(base, Some("k")), &package(), &CancelToken::default(), crate::CURL_BIN, |_| {}).unwrap_err();
        assert!(error.contains("not reachable"), "{error}");

        let error = stream_chat(&test_config("http://127.0.0.1:1/v1".into(), None), &package(), &CancelToken::default(), crate::CURL_BIN, |_| {}).unwrap_err();
        assert!(error.contains("API key is not configured"));
    }

    #[test]
    fn cancellation_kills_a_stalled_stream_promptly() {
        let (base, _seen) = serve_once(
            SSE_HEAD,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"ANCHORS\\n\"}}]}\n\n", 0),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n", 10_000),
            ],
        );
        let token = Arc::new(CancelToken::default());
        let canceller = token.clone();
        let started = Instant::now();
        let (first_tx, first_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut text = String::new();
            let outcome = stream_chat(&test_config(base, Some("k")), &package(), &token, crate::CURL_BIN, |delta| {
                text.push_str(delta);
                let _ = first_tx.send(());
            });
            (outcome, text)
        });
        first_rx.recv_timeout(Duration::from_secs(5)).expect("first delta");
        canceller.cancel();
        let (outcome, text) = handle.join().unwrap();
        assert_eq!(outcome, Ok(StreamOutcome::Cancelled));
        assert_eq!(text, "ANCHORS\n");
        assert!(started.elapsed() < Duration::from_secs(5), "cancel must not wait for the stalled chunk");
    }

    #[test]
    fn status_probe_reports_unreachable_with_actionable_hint() {
        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", unused.local_addr().unwrap());
        drop(unused);
        let mut config = test_config(base, Some("k"));
        config.api_server_enabled = None;
        let status = check_status(&config, crate::CURL_BIN);
        assert!(!status.reachable);
        assert!(status.message.contains("API_SERVER_ENABLED=true"), "{}", status.message);
    }
}
