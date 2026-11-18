//! Direct client for the ChatGPT subscription model backend used by Codex
//! (`https://chatgpt.com/backend-api/codex`, OpenAI Responses API format).
//!
//! Authentication is the owner's existing ChatGPT sign-in (see
//! `codex_auth.rs`); usage counts against that subscription. The app
//! identifies itself honestly (`originator: intervjuare`).
//!
//! Transport is macOS `/usr/bin/curl`, like the STT path. Bearer token and
//! workspace headers go through curl's stdin as config lines, never argv.
//! The request body (profile, transcript, optional screenshot) lives only in
//! an owner-only RAII workspace removed when the request ends. Cancelling
//! kills curl, which closes the stream.

use crate::codex_auth::Account;
use serde::Serialize;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Fastest model with good Swedish in the owner's 2026-11-18 measurement
/// (first anchor ≈ 1.2–1.7 s, answer text ≈ 1.9–2.3 s).
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
pub const DEFAULT_REASONING: &str = "none";
const ORIGINATOR: &str = "intervjuare";
const ERROR_BODY_LIMIT: usize = 4 * 1024;
const STATUS_TRAILER: &str = "__INTERVJUARE_HTTP_STATUS__:";
const STREAM_MAX_SECS: &str = "120";
const CONNECT_TIMEOUT_SECS: &str = "3";

/// `INTERVJUARE_CODEX_BASE_URL` exists for tests against a local server.
pub fn base_url() -> String {
    std::env::var("INTERVJUARE_CODEX_BASE_URL")
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub model: String,
    /// Empty: omit the field and use the model default.
    pub reasoning_effort: String,
    pub instructions: String,
    pub text: String,
    /// `data:image/jpeg;base64,…`, used only for the current request.
    pub image_data_url: Option<String>,
    /// Routes requests with the same static prefix to the same prompt cache.
    pub cache_key: Option<String>,
}

pub fn request_body(request: &ModelRequest) -> serde_json::Value {
    let mut content = vec![serde_json::json!({ "type": "input_text", "text": request.text })];
    if let Some(image) = &request.image_data_url {
        content.push(serde_json::json!({ "type": "input_image", "image_url": image }));
    }
    let mut body = serde_json::json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": [{ "type": "message", "role": "user", "content": content }],
        "store": false,
        "stream": true,
    });
    if !request.reasoning_effort.trim().is_empty() {
        body["reasoning"] = serde_json::json!({ "effort": request.reasoning_effort.trim() });
    }
    if let Some(key) = &request.cache_key {
        body["prompt_cache_key"] = serde_json::json!(key);
    }
    body
}

fn config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// curl config lines carrying the secret and workspace headers.
fn curl_auth_config(account: &Account) -> String {
    let mut config = format!("header = \"Authorization: Bearer {}\"\n", config_escape(&account.access_token));
    if let Some(account_id) = &account.account_id {
        config.push_str(&format!("header = \"ChatGPT-Account-ID: {}\"\n", config_escape(account_id)));
    }
    if let Some(residency) = &account.residency {
        config.push_str(&format!("header = \"x-openai-internal-codex-residency: {}\"\n", config_escape(residency)));
    }
    config
}

fn identity_args() -> [String; 4] {
    [
        "-H".into(),
        format!("originator: {ORIGINATOR}"),
        "-H".into(),
        format!("User-Agent: Intervjuare/{}", env!("CARGO_PKG_VERSION")),
    ]
}

/// Shared between a request thread and whoever cancels it.
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
    Completed { incomplete_reason: Option<String> },
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum SseItem {
    Delta(String),
    Completed,
    Incomplete(String),
    Error(String),
}

/// Line-oriented decoder for Responses API server-sent events.
#[derive(Default)]
struct SseDecoder {
    data: Vec<String>,
    http_status: Option<u16>,
    non_sse: String,
}

impl SseDecoder {
    fn push_line(&mut self, line: &str) -> Option<SseItem> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(code) = line.strip_prefix(STATUS_TRAILER) {
            self.http_status = code.trim().parse().ok();
            return self.dispatch();
        }
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') || line.starts_with("event:") || line.starts_with("id:") || line.starts_with("retry:") {
            return None;
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data.push(data.strip_prefix(' ').unwrap_or(data).to_string());
        } else if self.non_sse.len() < ERROR_BODY_LIMIT {
            self.non_sse.push_str(line);
            self.non_sse.push('\n');
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseItem> {
        let data = std::mem::take(&mut self.data).join("\n");
        if data.is_empty() || data.trim() == "[DONE]" {
            return None;
        }
        let event: serde_json::Value = serde_json::from_str(&data).ok()?;
        match event.get("type")?.as_str()? {
            "response.output_text.delta" => {
                let delta = event.get("delta")?.as_str()?;
                (!delta.is_empty()).then(|| SseItem::Delta(delta.to_string()))
            }
            "response.completed" => Some(SseItem::Completed),
            "response.incomplete" => Some(SseItem::Incomplete(
                event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(|value| value.as_str())
                    .unwrap_or("incomplete")
                    .to_string(),
            )),
            "response.failed" => Some(SseItem::Error(
                event
                    .pointer("/response/error/message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("The model request failed.")
                    .to_string(),
            )),
            "error" => Some(SseItem::Error(error_message(&event).unwrap_or_else(|| "The model request failed.".into()))),
            _ => None,
        }
    }

    fn error_body_message(&self) -> Option<String> {
        let body = self.non_sse.trim();
        if body.is_empty() {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| error_message(&value))
            .or_else(|| Some(body.chars().take(300).collect()))
    }
}

fn error_message(value: &serde_json::Value) -> Option<String> {
    let text = |pointer: &str| value.pointer(pointer).and_then(|value| value.as_str()).map(str::to_string);
    text("/error/message").or_else(|| text("/message")).or_else(|| text("/detail")).or_else(|| text("/error"))
}

fn http_error(account: &Account, code: u16, detail: Option<String>) -> String {
    let detail = detail.unwrap_or_default();
    match code {
        401 | 403 => format!(
            "ChatGPT rejected the sign-in for {} (HTTP {code}). Open the Codex app (or run `codex`) so it refreshes the login. {detail}",
            account.email
        ),
        429 => format!("ChatGPT usage limit or rate limit reached for {} (HTTP 429). {detail}", account.email),
        _ => format!("ChatGPT answered HTTP {code}: {detail}"),
    }
}

fn transport_error(exit_code: Option<i32>, stderr: &str) -> String {
    match exit_code {
        Some(6 | 7) => "Could not reach chatgpt.com. Check the internet connection.".into(),
        Some(28) => "The model request timed out.".into(),
        _ if !stderr.trim().is_empty() => format!("The model request failed: {}", stderr.trim()),
        _ => format!("The model request failed (curl exit {exit_code:?})."),
    }
}

/// Streams one response; `on_delta` receives output text in order.
pub fn stream_response(
    account: &Account,
    request: &ModelRequest,
    token: &CancelToken,
    curl_bin: &str,
    base_url: &str,
    mut on_delta: impl FnMut(&str),
) -> Result<StreamOutcome, String> {
    let workspace = crate::InferenceWorkspace::new()?;
    let body_path = workspace.path.join("model-request.json");
    let stderr_path = workspace.path.join("curl-stderr.txt");
    fs::write(&body_path, request_body(request).to_string()).map_err(|error| format!("could not prepare the model request: {error}"))?;
    let stderr_file = fs::File::create(&stderr_path).map_err(|error| format!("could not prepare curl log: {error}"))?;

    let url = format!("{base_url}/responses");
    let data_arg = format!("@{}", body_path.display());
    let write_out = format!("\n{STATUS_TRAILER}%{{http_code}}\n");
    let mut command = Command::new(curl_bin);
    command
        .args(["--silent", "--show-error", "--no-buffer", "--connect-timeout", CONNECT_TIMEOUT_SECS, "--max-time", STREAM_MAX_SECS])
        .args(["--config", "-"])
        .args(identity_args())
        .args(["-H", "Content-Type: application/json", "-H", "Accept: text/event-stream"])
        // Bodies over 1 KiB otherwise wait up to 1 s for `100 Continue`.
        .args(["-H", "Expect:"])
        .args(["--data-binary", &data_arg, "--write-out", &write_out, &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file));
    let mut child = command.spawn().map_err(|error| format!("could not start curl: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("curl stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("curl stdout unavailable")?;
    if let Ok(mut guard) = token.child.lock() {
        *guard = Some(child);
    }
    if token.is_cancelled() {
        token.cancel();
    }
    let config_written = stdin.write_all(curl_auth_config(account).as_bytes());
    drop(stdin);

    let mut decoder = SseDecoder::default();
    let mut completed = false;
    let mut incomplete_reason = None;
    let mut stream_error = None;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                stream_error.get_or_insert(format!("reading the model stream failed: {error}"));
                break;
            }
        }
        if token.is_cancelled() {
            break;
        }
        match decoder.push_line(&line) {
            Some(SseItem::Delta(text)) => on_delta(&text),
            Some(SseItem::Completed) => completed = true,
            Some(SseItem::Incomplete(reason)) => {
                completed = true;
                incomplete_reason = Some(reason);
            }
            Some(SseItem::Error(message)) => stream_error = Some(message),
            None => {}
        }
    }
    if let Some(item) = decoder.push_line("") {
        if let SseItem::Error(message) = item {
            stream_error = Some(message);
        }
    }

    let status = token.child.lock().ok().and_then(|mut guard| guard.take()).map(|mut child| child.wait());
    if token.is_cancelled() {
        return Ok(StreamOutcome::Cancelled);
    }
    config_written.map_err(|error| format!("could not pass credentials to curl: {error}"))?;
    let exit_code = status.and_then(|status| status.ok()).and_then(|status| status.code());
    match decoder.http_status {
        Some(200) => {}
        Some(0) | None => return Err(transport_error(exit_code, &fs::read_to_string(&stderr_path).unwrap_or_default())),
        Some(code) => return Err(http_error(account, code, decoder.error_body_message())),
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    if !completed {
        return Err("The model stream ended before the response was complete.".into());
    }
    Ok(StreamOutcome::Completed { incomplete_reason })
}

/// Collects a whole response (background work such as interview memory).
pub fn complete(account: &Account, request: &ModelRequest, token: &CancelToken, curl_bin: &str, base_url: &str) -> Result<String, String> {
    let mut text = String::new();
    match stream_response(account, request, token, curl_bin, base_url, |delta| text.push_str(delta))? {
        StreamOutcome::Completed { .. } => Ok(text),
        StreamOutcome::Cancelled => Err("cancelled".into()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub slug: String,
    pub description: String,
}

/// Models available to the account (cheap; also proves the sign-in works).
pub fn list_models(account: &Account, curl_bin: &str, base_url: &str) -> Result<Vec<ModelInfo>, String> {
    let url = format!("{base_url}/models?client_version=99.0.0");
    let mut child = Command::new(curl_bin)
        .args(["--silent", "--show-error", "--connect-timeout", CONNECT_TIMEOUT_SECS, "--max-time", "8", "--config", "-"])
        .args(identity_args())
        .args(["--write-out", &format!("\n{STATUS_TRAILER}%{{http_code}}"), &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start curl: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(curl_auth_config(account).as_bytes());
    }
    let output = child.wait_with_output().map_err(|error| format!("curl failed: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, code) = stdout.rsplit_once(STATUS_TRAILER).unwrap_or((&stdout, "0"));
    match code.trim().parse::<u16>().unwrap_or(0) {
        200 => {}
        0 => return Err(transport_error(output.status.code(), &String::from_utf8_lossy(&output.stderr))),
        code => {
            let detail = serde_json::from_str::<serde_json::Value>(body.trim()).ok().and_then(|value| error_message(&value));
            return Err(http_error(account, code, detail));
        }
    }
    let json: serde_json::Value = serde_json::from_str(body.trim()).map_err(|error| format!("unexpected model list: {error}"))?;
    let mut models: Vec<(i64, ModelInfo)> = json
        .get("models")
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let slug = entry.get("slug")?.as_str()?.trim().to_string();
                    let description = entry.get("description").and_then(|value| value.as_str()).unwrap_or("").to_string();
                    let priority = entry.get("priority").and_then(|value| value.as_i64()).unwrap_or(i64::MAX);
                    (!slug.is_empty()).then_some((priority, ModelInfo { slug, description }))
                })
                .collect()
        })
        .unwrap_or_default();
    models.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.slug.cmp(&b.1.slug)));
    Ok(models.into_iter().map(|(_, model)| model).collect())
}

#[cfg(test)]
pub mod test_server {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Duration,
    };

    pub const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

    /// One-shot HTTP server; reports the request head and body it saw.
    pub fn serve_once(head: &'static str, chunks: Vec<(String, u64)>) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                request_head.push_str(&line);
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let _ = sender.send((request_head, String::from_utf8_lossy(&body).into_owned()));
            let mut stream = stream;
            let _ = stream.write_all(head.as_bytes());
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

    pub fn delta(text: &str) -> String {
        format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            serde_json::json!({ "type": "response.output_text.delta", "delta": text })
        )
    }

    pub fn completed() -> String {
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n".into()
    }

    pub fn unused_base() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        base
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::*;
    use super::*;
    use crate::codex_auth;
    use std::{
        sync::{mpsc, Arc},
        thread,
        time::{Duration, Instant},
    };

    fn account() -> Account {
        let json = serde_json::json!({ "tokens": { "access_token": codex_auth::test_token("me@example.com", "plus", u64::MAX / 2_000) } });
        codex_auth::accounts_from_json("test", &json).remove(0)
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "gpt-test".into(),
            reasoning_effort: "none".into(),
            instructions: "rules".into(),
            text: "Hur testade du det?".into(),
            image_data_url: None,
            cache_key: Some("pck".into()),
        }
    }

    #[test]
    fn body_matches_the_responses_contract() {
        let mut with_image = request();
        with_image.image_data_url = Some("data:image/jpeg;base64,AAAA".into());
        with_image.reasoning_effort = String::new();
        let body = request_body(&with_image);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "rules");
        assert_eq!(body["input"][0]["content"][0]["text"], "Hur testade du det?");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert!(body.get("reasoning").is_none(), "empty effort uses the model default");
        assert_eq!(request_body(&request())["reasoning"]["effort"], "none");
        assert_eq!(body["prompt_cache_key"], "pck");
    }

    #[test]
    fn sse_decoder_handles_responses_events() {
        let mut decoder = SseDecoder::default();
        let stream = format!(
            ": keepalive\n\nevent: response.created\ndata: {{\"type\":\"response.created\"}}\n\n{}{}event: response.reasoning_summary_text.delta\ndata: {{\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"secret thoughts\"}}\n\n{}",
            delta("ANCHORS\n"),
            delta("- a"),
            completed()
        );
        let items: Vec<SseItem> = stream.split_inclusive('\n').filter_map(|line| decoder.push_line(line)).collect();
        assert_eq!(items, vec![SseItem::Delta("ANCHORS\n".into()), SseItem::Delta("- a".into()), SseItem::Completed]);

        let mut decoder = SseDecoder::default();
        decoder.push_line("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n");
        assert_eq!(decoder.push_line("\n"), Some(SseItem::Error("boom".into())));
        decoder.push_line("{\"detail\":\"Unauthorized\"}\n");
        decoder.push_line(&format!("{STATUS_TRAILER}401\n"));
        assert_eq!(decoder.http_status, Some(401));
        assert_eq!(decoder.error_body_message().as_deref(), Some("Unauthorized"));
    }

    #[test]
    fn streams_through_real_curl_with_secrets_on_stdin() {
        let (base, seen) = serve_once(SSE_HEAD, vec![(delta("ANCHORS\n- a\n"), 0), (delta("ANSWER\nHej"), 20), (completed(), 0)]);
        let mut text = String::new();
        let outcome = stream_response(&account(), &request(), &CancelToken::default(), crate::CURL_BIN, &base, |delta| text.push_str(delta)).unwrap();
        assert_eq!(outcome, StreamOutcome::Completed { incomplete_reason: None });
        assert_eq!(text, "ANCHORS\n- a\nANSWER\nHej");
        let (head, body) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("POST /responses"), "{head}");
        assert!(head.contains("Authorization: Bearer ey"), "bearer header present");
        assert!(head.contains("ChatGPT-Account-ID: acct-me@example.com"));
        assert!(head.contains("originator: intervjuare"));
        assert!(!head.to_lowercase().contains("expect: 100"));
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["model"], "gpt-test");
    }

    #[test]
    fn stream_without_completion_is_an_error() {
        let (base, _seen) = serve_once(SSE_HEAD, vec![(delta("ANCHORS\n"), 0)]);
        let error = stream_response(&account(), &request(), &CancelToken::default(), crate::CURL_BIN, &base, |_| {}).unwrap_err();
        assert!(error.contains("ended before"), "{error}");
    }

    #[test]
    fn maps_http_and_transport_errors() {
        let (base, _seen) = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 52\r\nConnection: close\r\n\r\n",
            vec![("{\"error\":{\"message\":\"You've hit your usage limit.\"}}".into(), 0)],
        );
        let error = stream_response(&account(), &request(), &CancelToken::default(), crate::CURL_BIN, &base, |_| {}).unwrap_err();
        assert!(error.contains("usage limit") && error.contains("429"), "{error}");

        let (base, _seen) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n",
            vec![("{\"detail\":\"Unauthorized\"}".into(), 0)],
        );
        let error = stream_response(&account(), &request(), &CancelToken::default(), crate::CURL_BIN, &base, |_| {}).unwrap_err();
        assert!(error.contains("rejected the sign-in for me@example.com"), "{error}");

        let error = stream_response(&account(), &request(), &CancelToken::default(), crate::CURL_BIN, &unused_base(), |_| {}).unwrap_err();
        assert!(error.contains("Could not reach"), "{error}");
    }

    #[test]
    fn cancellation_kills_a_stalled_stream_promptly() {
        let (base, _seen) = serve_once(SSE_HEAD, vec![(delta("ANCHORS\n"), 0), (delta("late"), 10_000)]);
        let token = Arc::new(CancelToken::default());
        let canceller = token.clone();
        let (first_tx, first_rx) = mpsc::channel();
        let started = Instant::now();
        let handle = thread::spawn(move || {
            let mut text = String::new();
            let outcome = stream_response(&account(), &request(), &token, crate::CURL_BIN, &base, |delta| {
                text.push_str(delta);
                let _ = first_tx.send(());
            });
            (outcome, text)
        });
        first_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        canceller.cancel();
        let (outcome, text) = handle.join().unwrap();
        assert_eq!(outcome, Ok(StreamOutcome::Cancelled));
        assert_eq!(text, "ANCHORS\n");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn lists_models_by_priority() {
        let body = r#"{"models":[{"slug":"b","priority":2,"description":"slow"},{"slug":"a","priority":1,"description":"fast"},{"slug":" "}]}"#;
        let head: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_boxed_str(),
        );
        let (base, seen) = serve_once(head, vec![(body.into(), 0)]);
        let models = list_models(&account(), crate::CURL_BIN, &base).unwrap();
        assert_eq!(models.iter().map(|model| model.slug.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        let (head, _) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("GET /models?client_version="));
        assert!(head.contains("ChatGPT-Account-ID"));
    }
}
