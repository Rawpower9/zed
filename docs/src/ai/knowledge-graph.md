---
title: Knowledge Graph - Zed
description: Give agents a structured, versioned store of project knowledge to read and write instead of a loose documentation folder.
---

# Knowledge Graph {#knowledge-graph}

The knowledge graph is a set of short, typed notes about your project that agents read before exploring code and write to when they learn something worth keeping.

Without it, each agent session rediscovers the same facts by reading files, and many agents then save what they found to an ad-hoc notes folder that the next session has to read in full. The knowledge graph replaces that folder with nodes that are small, linked to each other, anchored to the files they describe, and searchable. An agent asks for the nodes related to the files it is working on, gets one line per node, and reads a full node only when the line is relevant.

## How It Is Stored {#storage}

Each node is one Markdown file in `.zed/knowledge/` at the root of your project. The file name is the node's id.

```md
---
kind: decision
title: Sessions live in Redis
summary: Sessions are stored in Redis so that any server can validate a token.
anchors:
  - path: src/auth/session.rs
    symbol: SessionStore
links:
  - relation: part_of
    to: authentication
---

Tokens are opaque ids. The session record holds the user id and expiry, and
expires after 24 hours of inactivity.
```

Commit the directory with the rest of your project. Because every node is its own file, two branches that add different nodes merge without conflicts, and knowledge changes show up in pull requests next to the code they describe.

### Node Fields {#node-fields}

| Field     | Purpose                                                                  | Limit           |
| --------- | ------------------------------------------------------------------------ | --------------- |
| id        | The file name without `.md`. Lowercase letters, digits, and hyphens.     | 64 characters   |
| `kind`    | What sort of fact this is. See [Node Kinds](#node-kinds).                |                 |
| `title`   | Names the fact on one line.                                              | 80 characters   |
| `summary` | One sentence that is useful on its own. Searches return only this line.  | 200 characters  |
| `anchors` | Files, directories, or symbols the node describes, relative to the root. |                 |
| `links`   | Relationships to other nodes. See [Relations](#relations).               |                 |
| body      | Markdown with the detail a reader needs after the summary.               | 8000 characters |

The limits apply when an agent writes a node. They keep nodes small enough that reading one costs little, and they push longer material into several linked nodes. Files you edit by hand still load if they run long.

### Node Kinds {#node-kinds}

| Kind         | Use it for                                                      |
| ------------ | --------------------------------------------------------------- |
| `concept`    | An idea or term needed to understand the project                |
| `component`  | A module, service, or other unit and what it is responsible for |
| `decision`   | A choice that was made, with the reason for it                  |
| `convention` | A rule the project follows that tooling does not enforce        |
| `pitfall`    | Something that looks right but breaks, and how to avoid it      |
| `procedure`  | The steps for a recurring task                                  |

### Relations {#relations}

| Relation     | Meaning                                                         |
| ------------ | --------------------------------------------------------------- |
| `related_to` | The two nodes are worth reading together                        |
| `depends_on` | This node cannot be understood or changed without the other one |
| `part_of`    | This node is a piece of the other one                           |
| `supersedes` | This node replaces the other one, which is kept for history     |

Links are stored on the node they start from. When you read a node, Zed also shows which nodes link to it.

## Using It with Zed Agent {#zed-agent}

[Zed Agent](./zed-agent.md) has two built-in [tools](./tools.md) for the graph:

- `query_knowledge_graph` lists nodes, searches them by text, file path, or kind, and reads full nodes by id.
- `update_knowledge_graph` creates, replaces, and deletes nodes.

When these tools are enabled, Zed tells the agent to check the graph before exploring unfamiliar code, to record non-obvious findings in it, and not to write notes or architecture summaries to documentation folders or scratch files. You do not need to add this to your [instructions](./instructions.md).

Both tools are enabled in the `Write` [profile](./agent-profiles.md). The `Ask` profile has `query_knowledge_graph` only.

`update_knowledge_graph` follows your [tool permissions](./tool-permissions.md). With the default `confirm` setting, Zed asks before each update and shows which nodes it creates, changes, or deletes. To let the agent record knowledge without asking:

```json [settings]
{
  "agent": {
    "tool_permissions": {
      "tools": {
        "update_knowledge_graph": {
          "default": "allow"
        }
      }
    }
  }
}
```

Knowledge updates are written straight to `.zed/knowledge/`. They appear in your Git changes, not in the agent's edit review.

## Using It with External Agents {#external-agents}

[External Agents](./external-agents.md) and CLIs in [Terminal Threads](./terminal-threads.md) do not have Zed Agent's built-in tools. They reach the same graph through an MCP server that exposes the same two tools and gives the agent the same guidance.

The [Agent Harness](./agent-harness.md) attaches that server to every session of the agent it wraps, which is the recommended setup for agents such as Claude Agent.

To attach the server yourself, run the harness binary in `mcp` mode. For example, to add it to Claude Code for the current project:

```sh
claude mcp add zed-knowledge-graph -- /path/to/zed-agent-harness mcp --project-root "$PWD"
```

All agents read and write the same files, so a node recorded by one agent is available to the others.

## What to Record {#what-to-record}

Agents are told to record facts that a later session would otherwise have to rediscover:

- why something is built the way it is
- which component owns a responsibility
- conventions that are not enforced by a linter
- traps that cost time to find

They are told not to record what the code or Git history already states, or progress on the current task.

You can write and edit nodes by hand. To bring an existing notes folder into the graph, ask the agent to convert it into nodes and then delete the folder.

## Keeping It Accurate {#accuracy}

Stale knowledge is worse than none. Zed checks every anchor when the graph is queried and marks nodes whose file or directory no longer exists, so that the agent can correct or delete them. Agents are told to fix a wrong node instead of adding a contradicting one.

A node file that fails to parse is reported as a warning in query results and does not hide the rest of the graph.

Review knowledge changes in pull requests the same way you review code. A node is trusted by every later session that reads it.

## Limitations {#limitations}

- Zed Agent's built-in tools work in local projects only. In remote projects, they return an error.
- In a project with several folders, the graph belongs to the first folder.
- Search matches words in ids, titles, summaries, anchors, and bodies. It is not semantic search.
- There is no panel for browsing the graph. Open the files in `.zed/knowledge/` to read or edit nodes.
