//! Screenshot for "Låt oss se" (Milestone 5), taken by the Swift bridge.
//!
//! The image lives only in memory and in the transient request body of the
//! one model request that uses it: capture → analyse → discard.

use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub data_url: String,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Screenshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Screenshot({}x{}, {} bytes)", self.width, self.height, self.data_url.len())
    }
}

/// `application_id` is the meeting application's process id from source
/// selection; only that app's windows are captured when it has any.
pub fn capture(application_id: Option<&str>) -> Result<Screenshot, String> {
    let bridge = crate::bridge_executable()?;
    let mut command = Command::new(bridge);
    command.arg("screenshot");
    if let Some(pid) = application_id.filter(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())) {
        command.arg(pid);
    }
    run_capture(command, CAPTURE_TIMEOUT)
}

fn run_capture(mut command: Command, timeout: Duration) -> Result<Screenshot, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the screen capture helper: {error}"))?;
    let stdout = child.stdout.take().ok_or("screen capture helper has no stdout")?;
    // Read concurrently: the image is far larger than a pipe buffer.
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.take(OUTPUT_LIMIT).read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("Screen capture timed out.".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(error) => return Err(format!("could not monitor the screen capture helper: {error}")),
        }
    }
    let output = reader.join().map_err(|_| "screen capture reader panicked".to_string())?;
    parse_output(&output)
}

fn parse_output(output: &[u8]) -> Result<Screenshot, String> {
    let line = output.split(|byte| *byte == b'\n').find(|line| !line.is_empty()).ok_or("The screen capture helper returned nothing.")?;
    let json: serde_json::Value = serde_json::from_slice(line).map_err(|_| "The screen capture helper returned invalid output.".to_string())?;
    match json.get("type").and_then(|value| value.as_str()) {
        Some("screenshot") => {
            let mime = json.get("mime").and_then(|value| value.as_str()).unwrap_or("image/jpeg");
            let data = json.get("data").and_then(|value| value.as_str()).filter(|data| !data.is_empty()).ok_or("The screenshot was empty.")?;
            if mime != "image/jpeg" && mime != "image/png" {
                return Err(format!("Unexpected screenshot type {mime}."));
            }
            Ok(Screenshot {
                data_url: format!("data:{mime};base64,{data}"),
                width: json.get("width").and_then(|value| value.as_u64()).unwrap_or(0) as u32,
                height: json.get("height").and_then(|value| value.as_u64()).unwrap_or(0) as u32,
            })
        }
        Some("error") => Err(json
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("Screen capture failed.")
            .to_string()),
        _ => Err("The screen capture helper returned an unexpected message.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_screenshot_and_error_messages() {
        let shot = parse_output(b"{\"type\":\"screenshot\",\"mime\":\"image/jpeg\",\"width\":1600,\"height\":900,\"data\":\"QUJD\"}\n").unwrap();
        assert_eq!(shot.data_url, "data:image/jpeg;base64,QUJD");
        assert_eq!((shot.width, shot.height), (1600, 900));
        assert!(!format!("{shot:?}").contains("QUJD"), "debug output never dumps the image");

        let error = parse_output(b"{\"type\":\"error\",\"code\":\"screen_permission_denied\",\"message\":\"Grant it in System Settings\"}\n").unwrap_err();
        assert!(error.contains("System Settings"));
        assert!(parse_output(b"").is_err());
        assert!(parse_output(b"{\"type\":\"screenshot\",\"data\":\"\"}").is_err());
        assert!(parse_output(b"{\"type\":\"screenshot\",\"mime\":\"text/html\",\"data\":\"x\"}").is_err());
    }

    #[test]
    fn helper_output_larger_than_a_pipe_buffer_is_read_completely() {
        let payload = "A".repeat(300_000);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &format!("printf '{{\"type\":\"screenshot\",\"mime\":\"image/jpeg\",\"width\":1,\"height\":1,\"data\":\"{payload}\"}}\\n'")]);
        let shot = run_capture(command, CAPTURE_TIMEOUT).unwrap();
        assert_eq!(shot.data_url.len(), "data:image/jpeg;base64,".len() + payload.len());
    }

    #[test]
    fn a_hanging_helper_is_killed() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        assert!(run_capture(command, Duration::from_millis(300)).unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
