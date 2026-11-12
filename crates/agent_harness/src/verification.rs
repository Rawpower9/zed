//! Runs the project's verification commands on behalf of the proxy.

use anyhow::Result;
use futures::{AsyncRead, AsyncReadExt as _, FutureExt as _, channel::oneshot, pin_mut, select};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How much of a failing command's output is handed back to the agent. The
/// end of the output is kept, since that is where compilers and test runners
/// summarize what went wrong.
pub const MAX_OUTPUT_CHARACTERS: usize = 4000;
const OUTPUT_BUFFER_BYTES: usize = 64 * 1024;
/// A command's descendants can keep its output pipes open after it exits.
/// Wait this long for the remaining output before reporting without it.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationFailure {
    pub command: String,
    /// Why the command counts as failed, phrased to follow the command in a
    /// sentence, such as "exited with code 1".
    pub status: String,
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationOutcome {
    Passed,
    Failed(VerificationFailure),
    Cancelled,
}

/// Runs each command in order in `cwd`, stopping at the first failure.
/// Blocks the calling thread until the commands finish, `timeout` elapses for
/// one of them, or `cancel` fires or is dropped.
pub fn run_verification(
    commands: &[String],
    cwd: &Path,
    timeout: Duration,
    cancel: oneshot::Receiver<()>,
) -> VerificationOutcome {
    smol::block_on(async move {
        let mut cancel = cancel.fuse();
        for command in commands {
            match run_command(command, cwd, timeout, &mut cancel).await {
                CommandOutcome::Succeeded => {}
                CommandOutcome::Failed { status, output } => {
                    return VerificationOutcome::Failed(VerificationFailure {
                        command: command.clone(),
                        status,
                        output: output_tail(&output),
                    });
                }
                CommandOutcome::Cancelled => return VerificationOutcome::Cancelled,
            }
        }
        VerificationOutcome::Passed
    })
}

enum CommandOutcome {
    Succeeded,
    Failed { status: String, output: Vec<u8> },
    Cancelled,
}

#[allow(
    clippy::disallowed_methods,
    reason = "the harness does not run on GPUI's executors, so there is no test scheduler for \
              `smol::Timer` to bypass"
)]
async fn run_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &mut futures::future::Fuse<oneshot::Receiver<()>>,
) -> CommandOutcome {
    let mut child = match spawn_shell_command(command, cwd) {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome::Failed {
                status: format!("could not be started: {error:#}"),
                output: Vec::new(),
            };
        }
    };

    let output = Arc::new(Mutex::new(Vec::new()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_output = futures::future::join(
        collect_output(stdout, output.clone()),
        collect_output(stderr, output.clone()),
    )
    .fuse();
    pin_mut!(read_output);

    enum Ended {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    let ended = {
        let status = child.status().fuse();
        let timer = smol::Timer::after(timeout).fuse();
        pin_mut!(status, timer);
        loop {
            select! {
                status = status => break Ended::Exited(status),
                _ = timer => break Ended::TimedOut,
                // A dropped sender means nobody is waiting for the result.
                _ = &mut *cancel => break Ended::Cancelled,
                _ = read_output => {}
            }
        }
    };

    let status = match ended {
        Ended::Exited(status) => {
            let grace = smol::Timer::after(OUTPUT_DRAIN_GRACE).fuse();
            pin_mut!(grace);
            select! {
                _ = read_output => {}
                _ = grace => {}
            }
            status
        }
        Ended::TimedOut => {
            kill(&mut child);
            return CommandOutcome::Failed {
                status: format!("did not finish within {} seconds", timeout.as_secs()),
                output: take_output(&output),
            };
        }
        Ended::Cancelled => {
            kill(&mut child);
            return CommandOutcome::Cancelled;
        }
    };

    match status {
        Ok(status) if status.success() => CommandOutcome::Succeeded,
        Ok(status) => CommandOutcome::Failed {
            status: match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "was terminated by a signal".to_string(),
            },
            output: take_output(&output),
        },
        Err(error) => CommandOutcome::Failed {
            status: format!("could not be waited for: {error}"),
            output: take_output(&output),
        },
    }
}

fn spawn_shell_command(command: &str, cwd: &Path) -> Result<util::process::Child> {
    let mut shell = if cfg!(windows) {
        let mut shell = util::command::new_std_command("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = util::command::new_std_command("sh");
        shell.arg("-c").arg(command);
        shell
    };
    shell.current_dir(cwd);
    // `util::process::Child` puts the command in its own process group (or
    // job, on Windows), so that killing it also stops whatever it spawned.
    util::process::Child::spawn(shell, Stdio::null(), Stdio::piped(), Stdio::piped())
}

fn kill(child: &mut util::process::Child) {
    if let Err(error) = child.kill() {
        eprintln!("zed-agent-harness: failed to stop verification command: {error:#}");
    }
}

async fn collect_output(stream: Option<impl AsyncRead + Unpin>, output: Arc<Mutex<Vec<u8>>>) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut chunk = [0; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(length) => {
                let Ok(mut output) = output.lock() else {
                    return;
                };
                output.extend_from_slice(&chunk[..length]);
                // Only the end of the output is ever reported, so the
                // beginning can be dropped once enough has accumulated.
                if output.len() > 2 * OUTPUT_BUFFER_BYTES {
                    let excess = output.len() - OUTPUT_BUFFER_BYTES;
                    output.drain(..excess);
                }
            }
        }
    }
}

fn take_output(output: &Mutex<Vec<u8>>) -> Vec<u8> {
    match output.lock() {
        Ok(mut output) => std::mem::take(&mut *output),
        Err(_) => Vec::new(),
    }
}

fn output_tail(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    let character_count = text.chars().count();
    if character_count <= MAX_OUTPUT_CHARACTERS {
        return text.to_string();
    }
    let tail = text
        .chars()
        .skip(character_count - MAX_OUTPUT_CHARACTERS)
        .collect::<String>();
    format!("[earlier output omitted]\n{tail}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::time::Instant;

    fn run(commands: &[&str], cwd: &Path, timeout: Duration) -> VerificationOutcome {
        let (_keep_alive, cancel) = oneshot::channel();
        let commands = commands
            .iter()
            .map(|command| command.to_string())
            .collect::<Vec<_>>();
        run_verification(&commands, cwd, timeout, cancel)
    }

    #[test]
    fn passes_when_every_command_succeeds() {
        let directory = tempfile::tempdir().expect("temporary directory");
        assert_eq!(
            run(
                &["true", "echo checked > marker"],
                directory.path(),
                Duration::from_secs(30)
            ),
            VerificationOutcome::Passed
        );
        assert!(
            directory.path().join("marker").exists(),
            "commands run in the given directory"
        );
    }

    #[test]
    fn reports_the_first_failure_with_both_output_streams() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let outcome = run(
            &[
                "true",
                "echo to-stdout; echo to-stderr >&2; exit 3",
                "touch should-not-run",
            ],
            directory.path(),
            Duration::from_secs(30),
        );
        let VerificationOutcome::Failed(failure) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(
            failure.command,
            "echo to-stdout; echo to-stderr >&2; exit 3"
        );
        assert_eq!(failure.status, "exited with code 3");
        assert!(failure.output.contains("to-stdout"), "{}", failure.output);
        assert!(failure.output.contains("to-stderr"), "{}", failure.output);
        assert!(!directory.path().join("should-not-run").exists());
    }

    #[test]
    fn keeps_only_the_end_of_long_output() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let outcome = run(
            &["i=0; while [ $i -lt 3000 ]; do echo line-$i; i=$((i+1)); done; exit 1"],
            directory.path(),
            Duration::from_secs(30),
        );
        let VerificationOutcome::Failed(failure) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(failure.output.starts_with("[earlier output omitted]\n"));
        assert!(failure.output.ends_with("line-2999"));
        assert!(!failure.output.contains("line-0\n"));
        assert!(failure.output.chars().count() <= MAX_OUTPUT_CHARACTERS + 30);
    }

    #[test]
    fn a_command_that_runs_too_long_fails_and_is_stopped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let started = Instant::now();
        let outcome = run(
            &["echo started; sleep 30; touch finished"],
            directory.path(),
            Duration::from_secs(1),
        );
        let VerificationOutcome::Failed(failure) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(failure.status, "did not finish within 1 seconds");
        assert_eq!(failure.output, "started");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn cancelling_stops_the_running_command() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (cancel_sender, cancel) = oneshot::channel();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel_sender.send(()).ok();
        });
        let started = Instant::now();
        let outcome = run_verification(
            &["sleep 30".to_string(), "touch should-not-run".to_string()],
            directory.path(),
            Duration::from_secs(60),
            cancel,
        );
        canceller.join().expect("canceller thread finishes");
        assert_eq!(outcome, VerificationOutcome::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(!directory.path().join("should-not-run").exists());
    }

    #[test]
    fn a_background_process_holding_the_pipes_does_not_block_the_result() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let started = Instant::now();
        let outcome = run(
            &["sleep 30 & echo done; exit 2"],
            directory.path(),
            Duration::from_secs(60),
        );
        let VerificationOutcome::Failed(failure) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(failure.status, "exited with code 2");
        assert_eq!(failure.output, "done");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn a_missing_working_directory_is_a_failure_not_a_panic() {
        let outcome = run(
            &["true"],
            Path::new("/nonexistent/zed-agent-harness-test"),
            Duration::from_secs(30),
        );
        let VerificationOutcome::Failed(failure) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(failure.status.starts_with("could not be started: "));
    }

    #[test]
    fn output_tail_handles_multibyte_text() {
        let text = "é".repeat(MAX_OUTPUT_CHARACTERS + 10);
        let tail = output_tail(text.as_bytes());
        assert!(tail.starts_with("[earlier output omitted]\n"));
        assert_eq!(
            tail.chars().filter(|character| *character == 'é').count(),
            MAX_OUTPUT_CHARACTERS
        );
        assert_eq!(output_tail(b"  short  \n"), "short");
    }
}
