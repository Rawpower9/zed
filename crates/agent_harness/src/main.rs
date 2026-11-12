mod proxy;
mod verification;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};
use futures::channel::oneshot;
use proxy::{Config, Effect, Harness, McpServerCommand};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use verification::{VerificationFailure, VerificationOutcome, run_verification};

/// How long the wrapped agent gets to exit on its own after the client
/// disconnects before it is stopped.
const AGENT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Wraps an ACP agent so that it uses the project's knowledge graph and, optionally, has to pass
/// the project's checks before a turn ends.
///
/// The harness speaks ACP on stdin and stdout and forwards everything to the wrapped agent, so it
/// can be configured anywhere an ACP agent command is accepted. Authentication, models, and
/// billing stay with the wrapped agent.
#[derive(Parser)]
#[command(name = "zed-agent-harness", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommands>,
    #[command(flatten)]
    proxy: ProxyArguments,
}

#[derive(Subcommand)]
enum Subcommands {
    /// Serve a project's knowledge graph as an MCP server on stdin and stdout.
    Mcp {
        /// The project whose `.zed/knowledge` directory holds the graph.
        #[arg(long, value_name = "DIRECTORY", default_value = ".")]
        project_root: PathBuf,
    },
}

#[derive(Args)]
struct ProxyArguments {
    /// A shell command that must succeed after any turn in which the agent edited files or ran
    /// commands. If it fails, its output is sent back to the agent to fix. May be given more than
    /// once; the commands run in order in the session's working directory.
    #[arg(long = "verify", value_name = "COMMAND")]
    verify_commands: Vec<String>,
    /// How many times the agent is asked to fix a failing verification before the failure is
    /// handed back to you.
    #[arg(long, value_name = "COUNT", default_value_t = 2)]
    max_fix_attempts: u32,
    /// How long each verification command may run before it is stopped and counted as failed.
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    verify_timeout_seconds: u64,
    /// Do not attach the knowledge graph MCP server to sessions.
    #[arg(long)]
    no_knowledge_graph: bool,
    /// The command that starts the ACP agent to wrap, for example
    /// `npx -y @agentclientprotocol/claude-agent-acp`.
    #[arg(last = true, value_name = "AGENT_COMMAND")]
    agent_command: Vec<String>,
}

enum Event {
    FromClient(String),
    ClientClosed,
    FromAgent(String),
    AgentClosed,
    VerificationFinished {
        session_id: String,
        run: u64,
        failure: Option<VerificationFailure>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Subcommands::Mcp { project_root }) => serve_knowledge_graph(project_root),
        None => run_proxy(cli.proxy),
    };
    match result {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("zed-agent-harness: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve_knowledge_graph(project_root: PathBuf) -> Result<ExitCode> {
    let store = knowledge_graph::Store::new(project_root);
    knowledge_graph::mcp_server::serve(&store, std::io::stdin().lock(), std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn run_proxy(arguments: ProxyArguments) -> Result<ExitCode> {
    let Some((agent_program, agent_arguments)) = arguments.agent_command.split_first() else {
        bail!(
            "no agent command given; pass the ACP agent to wrap after `--`, for example \
             `zed-agent-harness -- npx -y @agentclientprotocol/claude-agent-acp`"
        );
    };

    let knowledge_graph_server = if arguments.no_knowledge_graph {
        None
    } else {
        let executable = std::env::current_exe()
            .context("locating the harness executable to serve the knowledge graph")?;
        Some(McpServerCommand {
            command: executable.to_string_lossy().into_owned(),
            arguments: vec!["mcp".into(), "--project-root".into()],
        })
    };
    let verify_timeout = Duration::from_secs(arguments.verify_timeout_seconds);
    let verify_commands = arguments.verify_commands;
    let mut harness = Harness::new(Config {
        knowledge_graph_server,
        verify_commands: verify_commands.clone(),
        max_fix_attempts: arguments.max_fix_attempts,
    });

    // The agent is deliberately left in the harness's own process group, so
    // that a client which stops the harness's group stops the agent as well.
    #[allow(
        clippy::disallowed_methods,
        reason = "the harness is a synchronous command-line program with no executor to block"
    )]
    let mut agent = util::command::new_std_command(agent_program)
        .args(agent_arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting agent `{agent_program}`"))?;
    let mut agent_stdin = agent.stdin.take();
    let agent_stdout = agent.stdout.take().context("agent has no stdout")?;

    let (events, incoming) = mpsc::channel();
    thread::spawn({
        let events = events.clone();
        move || {
            forward_lines(std::io::stdin().lock(), events, Event::FromClient, || {
                Event::ClientClosed
            })
        }
    });
    thread::spawn({
        let events = events.clone();
        move || {
            forward_lines(
                BufReader::new(agent_stdout),
                events,
                Event::FromAgent,
                || Event::AgentClosed,
            )
        }
    });

    let mut client = std::io::stdout().lock();
    let mut verifications: HashMap<String, (u64, oneshot::Sender<()>)> = HashMap::new();
    let mut next_verification_run = 0;
    let mut client_closed_at = None;

    'events: loop {
        let event = match client_closed_at {
            None => match incoming.recv() {
                Ok(event) => event,
                Err(mpsc::RecvError) => break,
            },
            Some(closed_at) => {
                let remaining = AGENT_SHUTDOWN_GRACE.saturating_sub(Instant::now() - closed_at);
                match incoming.recv_timeout(remaining) {
                    Ok(event) => event,
                    Err(_) => break,
                }
            }
        };

        let effects = match event {
            Event::FromClient(line) => harness.on_client_message(&line),
            Event::FromAgent(line) => harness.on_agent_message(&line),
            Event::VerificationFinished {
                session_id,
                run,
                failure,
            } => {
                // A result from a run that was cancelled and replaced must
                // not be mistaken for the current run's.
                let is_current = verifications
                    .get(&session_id)
                    .is_some_and(|(current_run, _)| *current_run == run);
                if !is_current {
                    continue;
                }
                verifications.remove(&session_id);
                harness.on_verification_finished(&session_id, failure)
            }
            Event::ClientClosed => {
                // Closing the agent's stdin is how an ACP client asks it to
                // exit.
                agent_stdin = None;
                verifications.clear();
                client_closed_at = Some(Instant::now());
                continue;
            }
            Event::AgentClosed => break,
        };

        for effect in effects {
            match effect {
                Effect::ToAgent(line) => {
                    if let Some(agent_stdin) = &mut agent_stdin
                        && let Err(error) = write_line(agent_stdin, &line)
                    {
                        eprintln!("zed-agent-harness: failed to write to the agent: {error}");
                    }
                }
                Effect::ToClient(line) => {
                    if client_closed_at.is_none()
                        && let Err(error) = write_line(&mut client, &line)
                    {
                        // Without a client there is nobody left to serve;
                        // fall through to shutting the agent down.
                        eprintln!("zed-agent-harness: failed to write to the client: {error}");
                        break 'events;
                    }
                }
                Effect::RunVerification { session_id, cwd } => {
                    let (cancel_sender, cancel) = oneshot::channel();
                    let run = next_verification_run;
                    next_verification_run += 1;
                    // Replacing an entry drops the previous run's sender,
                    // which cancels that run.
                    verifications.insert(session_id.clone(), (run, cancel_sender));
                    thread::spawn({
                        let events = events.clone();
                        let verify_commands = verify_commands.clone();
                        move || {
                            let failure = match run_verification(
                                &verify_commands,
                                &cwd,
                                verify_timeout,
                                cancel,
                            ) {
                                VerificationOutcome::Passed => None,
                                VerificationOutcome::Failed(failure) => Some(failure),
                                VerificationOutcome::Cancelled => return,
                            };
                            // The receiver is gone only when the harness is
                            // shutting down, and then the result is moot.
                            events
                                .send(Event::VerificationFinished {
                                    session_id,
                                    run,
                                    failure,
                                })
                                .ok();
                        }
                    });
                }
                Effect::CancelVerification { session_id } => {
                    verifications.remove(&session_id);
                }
                Effect::Log(message) => eprintln!("zed-agent-harness: {message}"),
            }
        }
    }

    drop(agent_stdin);
    drop(verifications);
    Ok(wait_for_agent(&mut agent))
}

fn forward_lines(
    reader: impl BufRead,
    events: mpsc::Sender<Event>,
    line_event: fn(String) -> Event,
    closed_event: fn() -> Event,
) {
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        if events.send(line_event(line)).is_err() {
            return;
        }
    }
    // The receiver is gone only when the main loop has already ended.
    events.send(closed_event()).ok();
}

fn write_line(writer: &mut impl Write, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Gives the agent a moment to exit by itself, then stops it, and reports
/// its exit status as the harness's own.
fn wait_for_agent(agent: &mut Child) -> ExitCode {
    let deadline = Instant::now() + AGENT_SHUTDOWN_GRACE;
    loop {
        match agent.try_wait() {
            Ok(Some(status)) => {
                return match status.code() {
                    Some(0) => ExitCode::SUCCESS,
                    Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                    None => ExitCode::FAILURE,
                };
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                if let Err(error) = agent.kill() {
                    eprintln!("zed-agent-harness: failed to stop the agent: {error}");
                }
                if let Err(error) = agent.wait() {
                    eprintln!("zed-agent-harness: failed to wait for the agent: {error}");
                }
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("zed-agent-harness: failed to wait for the agent: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
}
