use super::*;

pub(crate) const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 30;
pub(crate) const MAX_SHELL_TIMEOUT_SECS: u64 = 120;
const MAX_SHELL_OUTPUT_BYTES: usize = 20_000;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
pub const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
pub(crate) fn complete_shell_result(
    result: Result<(Value, ShellCommandSummary), String>,
) -> ToolExecution {
    match result {
        Ok((data, summary)) => ToolExecution::Completed(tool_ok_with_summary(
            data,
            ToolExecutionSummary::shell(summary),
        )),
        Err(error) => ToolExecution::error(error),
    }
}

pub(crate) async fn run_shell_command(
    root: &Path,
    command: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(Value, ShellCommandSummary), String> {
    if cancellation.is_cancelled() {
        return Err(TOOL_CANCELLED_ERROR.to_string());
    }

    let mut process = shell_command(command);
    process
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);

    let mut child = process
        .spawn()
        .map_err(|err| format!("failed to spawn shell command: {err}"))?;
    let process_id = child.id();
    let mut process_guard = ShellProcessGuard::new(process_id);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture command stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture command stderr".to_string())?;
    let (mut stdout_reader, stdout_capture) = spawn_output_capture(stdout);
    let (mut stderr_reader, stderr_capture) = spawn_output_capture(stderr);

    enum WaitOutcome {
        Completed(Result<std::process::ExitStatus, String>),
        TimedOut,
        Cancelled,
    }

    let outcome = {
        let completion =
            wait_for_shell_completion(&mut child, &mut stdout_reader, &mut stderr_reader);
        tokio::pin!(completion);
        tokio::select! {
            biased;
            result = &mut completion => WaitOutcome::Completed(result),
            _ = cancellation.cancelled() => WaitOutcome::Cancelled,
            _ = tokio::time::sleep(timeout) => WaitOutcome::TimedOut,
        }
    };

    let (status, stdout, stderr, timed_out, cancelled) = match outcome {
        WaitOutcome::Completed(status) => (
            status,
            output_capture_result(&stdout_capture, false, None),
            output_capture_result(&stderr_capture, false, None),
            false,
            false,
        ),
        WaitOutcome::TimedOut => {
            let status = terminate_shell(&mut child, process_id).await;
            let (stdout, stderr) = tokio::join!(
                finish_output_capture(stdout_reader, stdout_capture),
                finish_output_capture(stderr_reader, stderr_capture),
            );
            (status, stdout, stderr, true, false)
        }
        WaitOutcome::Cancelled => {
            let status = terminate_shell(&mut child, process_id).await;
            let (stdout, stderr) = tokio::join!(
                finish_output_capture(stdout_reader, stdout_capture),
                finish_output_capture(stderr_reader, stderr_capture),
            );
            (status, stdout, stderr, false, true)
        }
    };

    let fully_stopped = status.is_ok() && stdout.reached_eof && stderr.reached_eof;
    if fully_stopped {
        process_guard.disarm();
    }

    if cancelled {
        let cleanup_errors = [
            status.as_ref().err(),
            stdout.result.as_ref().err(),
            stderr.result.as_ref().err(),
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>();
        return if cleanup_errors.is_empty() {
            Err(TOOL_CANCELLED_ERROR.to_string())
        } else {
            Err(format!(
                "{TOOL_CANCELLED_ERROR}; cleanup errors: {}",
                cleanup_errors.join("; ")
            ))
        };
    }
    let status = status?;
    let (stdout, stdout_truncated) = stdout.result?;
    let (stderr, stderr_truncated) = stderr.result?;

    let exit_code = status.code();
    let data = json!({
        "command": command,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": stdout,
        "stderr": stderr,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    });
    let summary = ShellCommandSummary {
        command: command.to_string(),
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    };

    Ok((data, summary))
}

async fn wait_for_shell_completion(
    child: &mut Child,
    stdout_reader: &mut JoinHandle<()>,
    stderr_reader: &mut JoinHandle<()>,
) -> Result<std::process::ExitStatus, String> {
    let (status, stdout, stderr) = tokio::join!(child.wait(), stdout_reader, stderr_reader);
    let status = status.map_err(|error| format!("failed to wait for command: {error}"))?;
    stdout.map_err(|error| format!("stdout reader task failed: {error}"))?;
    stderr.map_err(|error| format!("stderr reader task failed: {error}"))?;
    Ok(status)
}

#[cfg(windows)]
fn shell_command(command: &str) -> TokioCommand {
    let mut builder = TokioCommand::new("cmd");
    builder.arg("/C").arg(command);
    builder
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> TokioCommand {
    let mut builder = TokioCommand::new("sh");
    builder.arg("-c").arg(command);
    builder
}

async fn terminate_shell(
    child: &mut Child,
    process_id: Option<u32>,
) -> Result<std::process::ExitStatus, String> {
    #[cfg(unix)]
    {
        let group_result = process_id
            .ok_or_else(|| "shell process id is unavailable".to_string())
            .and_then(|process_id| {
                kill_process_group(process_id)
                    .map_err(|error| format!("failed to kill shell process group: {error}"))
            });
        if let Err(group_error) = group_result {
            let root_error = child
                .start_kill()
                .err()
                .map(|error| format!("failed to kill root shell process: {error}"));
            let wait_error = child
                .wait()
                .await
                .err()
                .map(|error| format!("failed to wait for root shell process: {error}"));
            let mut errors = vec![group_error];
            errors.extend(root_error);
            errors.extend(wait_error);
            return Err(errors.join("; "));
        }
    }

    #[cfg(not(unix))]
    child
        .start_kill()
        .map_err(|error| format!("failed to kill shell command: {error}"))?;

    child
        .wait()
        .await
        .map_err(|error| format!("failed to wait for killed shell command: {error}"))
}

struct ShellProcessGuard {
    process_id: Option<u32>,
    armed: bool,
}

impl ShellProcessGuard {
    pub(crate) fn new(process_id: Option<u32>) -> Self {
        Self {
            process_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ShellProcessGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed
            && let Some(process_id) = self.process_id
        {
            let _ = kill_process_group(process_id);
        }
    }
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) -> std::io::Result<()> {
    let process_id = i32::try_from(process_id).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell process id exceeds the Unix pid range",
        )
    })?;

    unsafe extern "C" {
        fn kill(process_id: i32, signal: i32) -> i32;
    }

    // SAFETY: kill 只读取两个整数参数；负 pid 表示向对应进程组发送信号。
    if unsafe { kill(-process_id, 9) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[derive(Default)]
struct OutputCapture {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
    reached_eof: bool,
}

fn spawn_output_capture<R>(reader: R) -> (JoinHandle<()>, Arc<Mutex<OutputCapture>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let capture = Arc::new(Mutex::new(OutputCapture::default()));
    let task_capture = capture.clone();
    let task = tokio::spawn(capture_output(reader, task_capture));
    (task, capture)
}

async fn capture_output(mut reader: impl AsyncRead + Unpin, capture: Arc<Mutex<OutputCapture>>) {
    let mut buffer = [0_u8; 8192];

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let mut capture = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                capture.error = Some(format!("failed to read process output: {error}"));
                return;
            }
        };
        if read == 0 {
            let mut capture = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            capture.reached_eof = true;
            break;
        }

        let mut capture = capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = MAX_SHELL_OUTPUT_BYTES.saturating_sub(capture.bytes.len());
        if remaining > 0 {
            capture
                .bytes
                .extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            capture.truncated = true;
        }
    }
}

async fn finish_output_capture(
    mut task: JoinHandle<()>,
    capture: Arc<Mutex<OutputCapture>>,
) -> FinishedOutput {
    let task_error = match tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut task).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("process output reader task failed: {error}")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Some("process output pipe did not close after termination".to_string())
        }
    };
    output_capture_result(&capture, task_error.is_some(), task_error)
}

struct FinishedOutput {
    result: Result<(String, bool), String>,
    reached_eof: bool,
}

fn output_capture_result(
    capture: &Arc<Mutex<OutputCapture>>,
    incomplete: bool,
    task_error: Option<String>,
) -> FinishedOutput {
    let mut capture = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if incomplete {
        capture.truncated = true;
    }
    let reached_eof = capture.reached_eof && !incomplete && capture.error.is_none();
    let error = task_error.or_else(|| capture.error.take());
    let result = match error {
        Some(error) => Err(error),
        None => Ok((
            String::from_utf8_lossy(&capture.bytes).to_string(),
            capture.truncated,
        )),
    };
    FinishedOutput {
        result,
        reached_eof,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ShellCommandArgs {
    pub(crate) command: String,
    pub(crate) timeout_secs: Option<u64>,
}
