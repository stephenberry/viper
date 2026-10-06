//! Shell selection and command execution with streaming, truncated output.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncation, truncate_tail};

/// How commands are run: `program args... <command>`.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl ShellConfig {
    /// Use the configured shell, else bash, else sh.
    pub fn resolve(configured: Option<&str>) -> ShellConfig {
        if let Some(path) = configured.filter(|p| !p.trim().is_empty()) {
            return ShellConfig { program: crate::config::expand_home(path), args: vec!["-c".into()] };
        }
        if cfg!(windows) {
            return ShellConfig { program: PathBuf::from("bash"), args: vec!["-c".into()] };
        }
        for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash", "/opt/homebrew/bin/bash"] {
            if Path::new(candidate).exists() {
                return ShellConfig { program: PathBuf::from(candidate), args: vec!["-c".into()] };
            }
        }
        ShellConfig { program: PathBuf::from("/bin/sh"), args: vec!["-c".into()] }
    }
}

/// Output kept in memory before spilling to a temp file.
const SPILL_THRESHOLD: usize = DEFAULT_MAX_BYTES * 4;
/// Tail kept in memory after spilling, enough to produce the truncated view.
const TAIL_KEEP: usize = DEFAULT_MAX_BYTES * 2;
/// Grace period for reading remaining output after the shell exits. Background processes that
/// inherited the pipes would otherwise keep the read open forever.
const DRAIN_GRACE: Duration = Duration::from_millis(250);
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Collects command output, keeping a bounded tail in memory and the full output on disk once
/// it grows large.
struct OutputAccumulator {
    buffer: Vec<u8>,
    total_bytes: usize,
    total_newlines: usize,
    spill: Option<(PathBuf, std::fs::File)>,
}

impl OutputAccumulator {
    fn new() -> Self {
        Self { buffer: Vec::new(), total_bytes: 0, total_newlines: 0, spill: None }
    }

    fn append(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len();
        self.total_newlines += chunk.iter().filter(|b| **b == b'\n').count();
        if let Some((_, file)) = &mut self.spill {
            // Best effort: losing the on-disk copy must not fail the command.
            let _ = file.write_all(chunk);
        }
        self.buffer.extend_from_slice(chunk);
        if self.spill.is_none() && self.buffer.len() > SPILL_THRESHOLD {
            let path = std::env::temp_dir().join(format!("viper-bash-{}.log", uuid::Uuid::new_v4().simple()));
            if let Ok(mut file) = std::fs::File::create(&path) {
                let _ = file.write_all(&self.buffer);
                self.spill = Some((path, file));
            }
        }
        if self.buffer.len() > SPILL_THRESHOLD {
            let excess = self.buffer.len() - TAIL_KEEP;
            self.buffer.drain(..excess);
        }
    }

    /// Truncated view of the output (tail), with ANSI escapes removed.
    fn snapshot(&self) -> Truncation {
        let text = String::from_utf8_lossy(&self.buffer);
        let text = crate::util::strip_ansi(&text);
        // After dropping the head, the first buffered line may be partial.
        let text = if self.buffer.len() < self.total_bytes {
            text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default()
        } else {
            text
        };
        let mut truncation = truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        if self.buffer.len() < self.total_bytes {
            truncation.truncated = true;
            truncation.total_bytes = self.total_bytes;
            truncation.total_lines = self.total_newlines + usize::from(self.buffer.last() != Some(&b'\n'));
            if truncation.truncated_by.is_none() {
                truncation.truncated_by = Some(super::truncate::TruncatedBy::Bytes);
            }
        }
        truncation
    }

    /// Write the full output to a temp file if it was truncated and not yet spilled.
    fn full_output_path(&mut self, truncated: bool) -> Option<PathBuf> {
        if let Some((path, file)) = &mut self.spill {
            let _ = file.flush();
            return Some(path.clone());
        }
        if !truncated {
            return None;
        }
        let path = std::env::temp_dir().join(format!("viper-bash-{}.log", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, &self.buffer).ok()?;
        Some(path)
    }
}

#[derive(Debug, Clone)]
pub struct ShellResult {
    /// Tail-truncated output.
    pub output: String,
    pub truncation: Truncation,
    /// Exit code; `None` when cancelled, timed out, or killed by a signal.
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub full_output_path: Option<PathBuf>,
    pub duration: Duration,
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: killpg with a process-group id we created; failure (already exited) is harmless.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Run `command` through the shell in `cwd`, streaming truncated snapshots to `on_update`.
pub async fn run_shell_command(
    shell: &ShellConfig,
    command: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
    on_update: impl Fn(&Truncation),
) -> Result<ShellResult> {
    if !cwd.is_dir() {
        anyhow::bail!("Working directory does not exist: {}", cwd.display());
    }
    let mut cmd = tokio::process::Command::new(&shell.program);
    cmd.args(&shell.args)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("failed to start shell {}", shell.program.display()))?;
    let pid = child.id();
    let started = Instant::now();

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut readers = Vec::new();
    for mut pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        readers.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }
    drop(tx);

    let mut output = OutputAccumulator::new();
    let mut last_update = Instant::now() - UPDATE_INTERVAL;
    let mut dirty = false;
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let mut cancelled = false;
    let mut timed_out = false;
    let mut ticker = tokio::time::interval(UPDATE_INTERVAL);

    let status = loop {
        tokio::select! {
            chunk = rx.recv() => {
                if let Some(chunk) = chunk {
                    output.append(&chunk);
                    dirty = true;
                }
            }
            status = child.wait() => break status.ok(),
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if let Some(pid) = pid { kill_process_group(pid); }
                let _ = child.start_kill();
            }
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() && !timed_out => {
                timed_out = true;
                if let Some(pid) = pid { kill_process_group(pid); }
                let _ = child.start_kill();
            }
            _ = ticker.tick() => {}
        }
        if dirty && last_update.elapsed() >= UPDATE_INTERVAL {
            on_update(&output.snapshot());
            last_update = Instant::now();
            dirty = false;
        }
    };

    // Drain what the shell wrote before exiting.
    let drain_deadline = tokio::time::Instant::now() + DRAIN_GRACE;
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(chunk) => output.append(&chunk),
                None => break,
            },
            _ = tokio::time::sleep_until(drain_deadline) => break,
        }
    }
    for reader in readers {
        reader.abort();
    }

    let truncation = output.snapshot();
    on_update(&truncation);
    let full_output_path = output.full_output_path(truncation.truncated);
    let exit_code = if cancelled || timed_out { None } else { status.and_then(|s| exit_code(&s)) };
    Ok(ShellResult {
        output: truncation.content.trim_end_matches('\n').to_string(),
        truncation,
        exit_code,
        cancelled,
        timed_out,
        full_output_path,
        duration: started.elapsed(),
    })
}

fn exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(128 + signal);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> ShellConfig {
        ShellConfig::resolve(None)
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let dir = std::env::temp_dir();
        let result = run_shell_command(
            &shell(),
            "echo out; echo err >&2; exit 3",
            &dir,
            None,
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(3));
        assert!(result.output.contains("out"));
        assert!(result.output.contains("err"));
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = std::env::temp_dir();
        let result = run_shell_command(
            &shell(),
            "sleep 10",
            &dir,
            Some(Duration::from_millis(200)),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        assert!(result.timed_out);
        assert!(result.duration < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn background_process_does_not_hang() {
        let dir = std::env::temp_dir();
        let start = Instant::now();
        let result = run_shell_command(&shell(), "sleep 5 & echo done", &dir, None, &CancellationToken::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn large_output_spills_and_truncates() {
        let dir = std::env::temp_dir();
        let result =
            run_shell_command(&shell(), "seq 1 100000", &dir, None, &CancellationToken::new(), |_| {}).await.unwrap();
        assert!(result.truncation.truncated);
        assert!(result.output.ends_with("100000"));
        assert_eq!(result.truncation.total_lines, 100000);
        let path = result.full_output_path.unwrap();
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(full.starts_with("1\n2\n"));
        std::fs::remove_file(path).unwrap();
    }
}
