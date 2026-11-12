---
title: Agent Harness - Zed
description: Wrap an External Agent such as Claude Agent so that it uses the project knowledge graph and must pass your checks before a turn ends.
---

# Agent Harness {#agent-harness}

The agent harness is a small program that sits between Zed and an [External Agent](./external-agents.md). It forwards the whole conversation in both directions and changes two things:

- It attaches the project's [knowledge graph](./knowledge-graph.md) to every session, so the agent reads and records project knowledge there instead of in loose notes.
- It can run your verification commands after the agent changes something, and send the agent back to fix what fails before the turn ends.

The wrapped agent keeps its own runtime, tools, models, and sign-in. The harness does not call a model itself and has no API key setting.

> **Note:** The harness is not bundled with Zed releases yet. You build it from the Zed repository.

## Using a Claude Subscription {#claude-subscription}

Because the wrapped agent owns authentication, wrapping [Claude Agent](./external-agents.md#claude-agent) uses whatever Claude Agent is signed in with. To use a Claude Pro or Max subscription instead of an Anthropic API key, sign in with your Claude account:

- If you already use Claude Code on this machine and are signed in with your Claude account, Claude Agent uses that sign-in.
- Otherwise, start a thread with the wrapped agent, run `/login`, and choose your Claude account.

Billing and usage limits are between you and Anthropic. See [Use an Existing Subscription](./use-an-existing-subscription.md#claude).

## Setup {#setup}

Build the harness from a checkout of the Zed repository:

```sh
cargo build --release -p agent_harness
```

The binary is `target/release/zed-agent-harness`.

Then add it as a [Custom Agent](./external-agents.md#custom-agents). Put the command that starts the agent you want to wrap after `--`:

```json [settings]
{
  "agent_servers": {
    "Claude (Harness)": {
      "type": "custom",
      "command": "/path/to/zed-agent-harness",
      "args": [
        "--verify",
        "cargo check",
        "--",
        "npx",
        "-y",
        "@agentclientprotocol/claude-agent-acp"
      ],
      "env": {}
    }
  }
}
```

The agent appears in the new-thread menu in the [Agent Panel](./agent-panel.md) under the name you gave it. Threads behave like threads with the wrapped agent: tool calls, diffs, permission prompts, and `/login` all pass through.

Any agent that speaks the [Agent Client Protocol](https://agentclientprotocol.com) can be wrapped the same way.

## Verification {#verification}

Each `--verify` option is a shell command that must succeed. Give the option more than once to run several commands in order.

When the agent ends a turn in which it edited files or ran commands, the harness:

1. Runs the verification commands in the session's working directory, stopping at the first failure.
2. If they pass, ends the turn.
3. If one fails, sends the command, its exit status, and the end of its output back to the agent and asks it to fix the cause.
4. Repeats until verification passes or the agent has used its fix attempts, then ends the turn and shows you the remaining failure.

While this happens, the thread shows lines that start with **Harness:** so you can see why the turn has not ended. Stopping the thread also stops a running verification.

Turns in which the agent only read, searched, or answered are not verified.

| Option                               | Default | Purpose                                                                 |
| ------------------------------------ | ------- | ----------------------------------------------------------------------- |
| `--verify <COMMAND>`                 | none    | A command that must succeed. Without any, turns are never held.         |
| `--max-fix-attempts <COUNT>`         | `2`     | How many times the agent is asked to fix a failure                      |
| `--verify-timeout-seconds <SECONDS>` | `600`   | How long one command may run before it is stopped and counted as failed |
| `--no-knowledge-graph`               | off     | Do not attach the knowledge graph to sessions                           |

Choose commands that finish quickly, such as a type check or the tests for one package. A command that takes minutes holds every turn that long.

Verification commands come only from your settings. The harness does not read them from the project, because a command taken from a repository you opened would run without your review.

## Knowledge Graph Server {#knowledge-graph-server}

The same binary serves the knowledge graph over MCP:

```sh
zed-agent-harness mcp --project-root /path/to/project
```

The harness adds this server to each session automatically. Run it yourself only to give the graph to an agent you are not wrapping, as described in [Knowledge Graph](./knowledge-graph.md#external-agents).

## Limitations {#limitations}

- Verification works with version 1 of the Agent Client Protocol, which is what Zed uses by default. If the agent negotiates a newer version, the harness attaches the knowledge graph but does not verify turns, and says so in Zed's log.
- The harness cannot tell a command that changed files from one that did not, so a turn in which the agent ran any command is verified.
- Each configured agent has one set of verification commands. To use different commands in different projects, add one `agent_servers` entry per project type.
- If Zed is force-quit while a verification command is running, that command runs to completion.
- [Zed Agent](./zed-agent.md) is not an External Agent and cannot be wrapped. It has the knowledge graph tools built in.
