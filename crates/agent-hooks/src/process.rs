use crate::types::{MAX_HOOK_STDERR_BYTES, MAX_HOOK_STDOUT_BYTES};
use agent_core::{MiddlewareError, MiddlewareExecutionContext};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

pub(crate) async fn run_hook_command(
    argv: &[String],
    workspace_root: &Path,
    timeout_secs: u64,
    context: &MiddlewareExecutionContext,
    input: Vec<u8>,
) -> Result<Vec<u8>, MiddlewareError> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        MiddlewareError::new(format!(
            "failed to start hook command {:?}: {error}",
            argv[0]
        ))
    })?;
    let process_id = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stderr was not captured"))?;
    let wait = collect_child(&mut child, stdin, stdout, stderr, input);
    let result = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => ChildOutcome::Cancelled,
        result = tokio::time::timeout(Duration::from_secs(timeout_secs), wait) => {
            match result {
                Ok(result) => ChildOutcome::Completed(result),
                Err(_) => ChildOutcome::TimedOut,
            }
        }
    };
    match result {
        ChildOutcome::Completed(result) => validate_child_output(result?),
        ChildOutcome::Cancelled => {
            terminate_child(&mut child, process_id).await;
            Err(MiddlewareError::new("hook command cancelled"))
        }
        ChildOutcome::TimedOut => {
            terminate_child(&mut child, process_id).await;
            Err(MiddlewareError::new(format!(
                "hook command timed out after {timeout_secs} seconds"
            )))
        }
    }
}

enum ChildOutcome {
    Completed(Result<ChildOutput, MiddlewareError>),
    Cancelled,
    TimedOut,
}

struct ChildOutput {
    status: std::process::ExitStatus,
    stdout: LimitedOutput,
    stderr: LimitedOutput,
}

async fn collect_child(
    child: &mut Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    input: Vec<u8>,
) -> Result<ChildOutput, MiddlewareError> {
    let write = async move {
        match stdin.write_all(&input).await {
            Ok(()) => stdin.shutdown().await,
            // hook 不读 stdin 且提前退出（如直接 printf 返回结果）时，写入端会收到
            // EPIPE；输入本来就是尽力投递，这不是 hook 失败。
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error),
        }
    };
    let (write, status, stdout, stderr) = tokio::join!(
        write,
        child.wait(),
        read_limited(stdout, MAX_HOOK_STDOUT_BYTES),
        read_limited(stderr, MAX_HOOK_STDERR_BYTES),
    );
    write.map_err(|error| MiddlewareError::new(format!("failed to write hook stdin: {error}")))?;
    Ok(ChildOutput {
        status: status
            .map_err(|error| MiddlewareError::new(format!("failed to wait for hook: {error}")))?,
        stdout: stdout.map_err(|error| {
            MiddlewareError::new(format!("failed to read hook stdout: {error}"))
        })?,
        stderr: stderr.map_err(|error| {
            MiddlewareError::new(format!("failed to read hook stderr: {error}"))
        })?,
    })
}

struct LimitedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<LimitedOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    Ok(LimitedOutput { bytes, exceeded })
}

fn validate_child_output(output: ChildOutput) -> Result<Vec<u8>, MiddlewareError> {
    if output.stdout.exceeded {
        return Err(MiddlewareError::new(format!(
            "hook stdout exceeds {MAX_HOOK_STDOUT_BYTES} bytes"
        )));
    }
    if output.stderr.exceeded {
        return Err(MiddlewareError::new(format!(
            "hook stderr exceeds {MAX_HOOK_STDERR_BYTES} bytes"
        )));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr.bytes);
        let detail = stderr.trim();
        return Err(MiddlewareError::new(if detail.is_empty() {
            format!("hook command exited with {}", output.status)
        } else {
            format!("hook command exited with {}: {detail}", output.status)
        }));
    }
    Ok(output.stdout.bytes)
}

async fn terminate_child(child: &mut Child, process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_id) = process_id {
        kill_process_group(process_id);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) {
    const SIGKILL: i32 = 9;
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if let Ok(process_id) = i32::try_from(process_id) {
        unsafe {
            let _ = kill(-process_id, SIGKILL);
        }
    }
}
