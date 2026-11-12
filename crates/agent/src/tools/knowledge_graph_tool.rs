use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, anyhow, ensure};
use fs::{Fs, RemoveOptions};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, Entity, SharedString, Task};
use knowledge_graph::{
    Graph, KNOWLEDGE_DIRECTORY, NodeId, QueryInput, UpdateInput, UpdateOutcome,
    normalize_anchor_path, render_load_warnings, serialize_node,
};
use project::Project;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct QueryKnowledgeGraphTool {
    project: Entity<Project>,
}

impl QueryKnowledgeGraphTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for QueryKnowledgeGraphTool {
    type Input = QueryInput;
    type Output = String;

    const NAME: &'static str = knowledge_graph::QUERY_TOOL_NAME;

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        let Ok(input) = input else {
            return "Query knowledge graph".into();
        };
        if !input.ids.is_empty() {
            format!("Read knowledge graph nodes: {}", input.ids.join(", ")).into()
        } else if let Some(text) = input.text.filter(|text| !text.trim().is_empty()) {
            format!("Search knowledge graph for “{}”", text.trim()).into()
        } else if !input.paths.is_empty() {
            format!("Search knowledge graph for {}", input.paths.join(", ")).into()
        } else {
            "List knowledge graph".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let mut input = input.recv().await.map_err(|error| error.to_string())?;
            let location = project
                .read_with(cx, |project, cx| GraphLocation::for_project(project, cx))
                .map_err(|error| format!("{error:#}"))?;
            async {
                for path in &mut input.paths {
                    *path = location.project_relative_path(path).await;
                }
                let (graph, warnings) = location.load().await?;
                let missing_anchor_paths = location.missing_anchor_paths(&graph).await;
                let mut output = render_load_warnings(&warnings);
                output.push_str(&graph.query(&input, &missing_anchor_paths));
                anyhow::Ok(output)
            }
            .await
            .map_err(|error| format!("{error:#}"))
        })
    }
}

pub struct UpdateKnowledgeGraphTool {
    project: Entity<Project>,
}

impl UpdateKnowledgeGraphTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for UpdateKnowledgeGraphTool {
    type Input = UpdateInput;
    type Output = String;

    const NAME: &'static str = knowledge_graph::UPDATE_TOOL_NAME;

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Update knowledge graph".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let mut input = input.recv().await.map_err(|error| error.to_string())?;
            let location = project
                .read_with(cx, |project, cx| GraphLocation::for_project(project, cx))
                .map_err(|error| format!("{error:#}"))?;

            // Validate against the current graph before asking the user for
            // anything, so that they are never prompted to approve an update
            // that would be rejected.
            let preview = async {
                for node in &mut input.nodes {
                    for anchor in &mut node.anchors {
                        anchor.path = location.project_relative_path(&anchor.path).await;
                    }
                }
                let (mut graph, _warnings) = location.load().await?;
                graph.apply_update(input.clone())
            }
            .await
            .map_err(|error| format!("{error:#}"))?;

            let changed_files = preview
                .written()
                .chain(&preview.deleted)
                .map(|id| location.display_path(id))
                .collect::<Vec<_>>();
            if changed_files.is_empty() {
                return Ok(preview.summary());
            }

            let authorize = cx.update(|cx| {
                event_stream.authorize(
                    format!("Update knowledge graph. {}", preview.summary()),
                    ToolPermissionContext::new(Self::NAME, changed_files),
                    cx,
                )
            });
            futures::select! {
                result = authorize.fuse() => result.map_err(|error| error.to_string())?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Knowledge graph update cancelled by user".to_string());
                }
            }

            // The graph may have changed while the user was deciding, so the
            // update is applied again to what is on disk now.
            async {
                let (mut graph, _warnings) = location.load().await?;
                let outcome = graph.apply_update(input)?;
                location.persist(&graph, &outcome).await?;
                anyhow::Ok(outcome.summary())
            }
            .await
            .map_err(|error| format!("{error:#}"))
        })
    }
}

/// Where a project's knowledge graph lives on disk.
///
/// Zed's agent reads and writes the node files directly rather than through
/// the MCP server, but shares the parsing, query, and update logic with it so
/// that both surfaces behave identically.
struct GraphLocation {
    fs: Arc<dyn Fs>,
    root: Arc<Path>,
    root_name: String,
}

impl GraphLocation {
    fn for_project(project: &Project, cx: &App) -> Result<Self> {
        ensure!(
            project.is_local(),
            "the knowledge graph is only available in local projects"
        );
        let worktree = project
            .visible_worktrees(cx)
            .next()
            .context("the project has no folder to hold a knowledge graph")?;
        let worktree = worktree.read(cx);
        Ok(Self {
            fs: project.fs().clone(),
            root: worktree.abs_path(),
            root_name: worktree.root_name_str().to_string(),
        })
    }

    fn directory(&self) -> PathBuf {
        self.root.join(KNOWLEDGE_DIRECTORY)
    }

    /// The path of a node file as the agent would name it, used for
    /// permission rules and prompts.
    fn display_path(&self, id: &NodeId) -> String {
        format!(
            "{}/{KNOWLEDGE_DIRECTORY}/{}",
            self.root_name,
            id.file_name()
        )
    }

    /// Zed's agent is told to start paths with the name of the project root,
    /// while the graph stores paths relative to that root so that other
    /// agents can use them. Accept either form, preferring whichever names
    /// something that exists.
    async fn project_relative_path(&self, path: &str) -> String {
        let Ok(normalized) = normalize_anchor_path(path) else {
            // Left as is, so that the graph reports what is wrong with it.
            return path.to_string();
        };
        let Some(without_root_name) = normalized
            .strip_prefix(self.root_name.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return normalized;
        };
        if self.exists(without_root_name).await || !self.exists(&normalized).await {
            without_root_name.to_string()
        } else {
            normalized
        }
    }

    async fn exists(&self, relative_path: &str) -> bool {
        // A path whose metadata cannot be read is not known to be missing.
        !matches!(
            self.fs.metadata(&self.root.join(relative_path)).await,
            Ok(None)
        )
    }

    async fn load(&self) -> Result<(Graph, Vec<String>)> {
        ensure!(
            self.fs.is_dir(&self.root).await,
            "the project root is not a folder, so it cannot hold a knowledge graph"
        );
        let directory = self.directory();
        if !self.fs.is_dir(&directory).await {
            return Ok((Graph::default(), Vec::new()));
        }
        self.ensure_directory_is_inside_project(&directory).await?;

        let mut warnings = Vec::new();
        let mut documents = Vec::new();
        let mut entries = self
            .fs
            .read_dir(&directory)
            .await
            .with_context(|| format!("reading {}", directory.display()))?;
        while let Some(entry) = entries.next().await {
            let path = entry.with_context(|| format!("reading {}", directory.display()))?;
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if NodeId::from_file_name(file_name).is_none() {
                continue;
            }
            match self.fs.load(&path).await {
                Ok(content) => documents.push((file_name.to_string(), content)),
                Err(error) => warnings.push(format!("{file_name}: {error:#}")),
            }
        }
        documents.sort();

        let (graph, mut parse_warnings) = Graph::from_documents(documents);
        warnings.append(&mut parse_warnings);
        Ok((graph, warnings))
    }

    async fn missing_anchor_paths(&self, graph: &Graph) -> BTreeSet<String> {
        let mut missing = BTreeSet::new();
        for path in graph.anchor_paths() {
            if !self.exists(path).await {
                missing.insert(path.to_string());
            }
        }
        missing
    }

    async fn persist(&self, graph: &Graph, outcome: &UpdateOutcome) -> Result<()> {
        let directory = self.directory();
        self.fs
            .create_dir(&directory)
            .await
            .with_context(|| format!("creating {}", directory.display()))?;
        self.ensure_directory_is_inside_project(&directory).await?;

        for id in outcome.written() {
            let node = graph
                .get(id)
                .ok_or_else(|| anyhow!("node `{id}` is missing from the updated graph"))?;
            let path = directory.join(id.file_name());
            self.fs
                .atomic_write(path.clone(), serialize_node(node)?)
                .await
                .with_context(|| format!("writing {}", path.display()))?;
        }
        for id in &outcome.deleted {
            let path = directory.join(id.file_name());
            self.fs
                .remove_file(
                    &path,
                    RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .with_context(|| format!("deleting {}", path.display()))?;
        }
        Ok(())
    }

    /// The knowledge directory may be a symlink. Following one that leaves
    /// the project would let a checked-in link redirect reads and writes to
    /// arbitrary locations.
    async fn ensure_directory_is_inside_project(&self, directory: &Path) -> Result<()> {
        let canonical_root = self
            .fs
            .canonicalize(&self.root)
            .await
            .with_context(|| format!("resolving {}", self.root.display()))?;
        let canonical_directory = self
            .fs
            .canonicalize(directory)
            .await
            .with_context(|| format!("resolving {}", directory.display()))?;
        ensure!(
            canonical_directory.starts_with(&canonical_root),
            "{} resolves to {}, which is outside the project",
            directory.display(),
            canonical_directory.display()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_settings::AgentSettings;
    use gpui::TestAppContext;
    use knowledge_graph::{Anchor, LinkInput, NodeInput, NodeKind, Relation};
    use project::FakeFs;
    use serde_json::json;
    use settings::{Settings as _, SettingsStore};
    use util::path;

    fn init_test(cx: &mut TestAppContext, tool_permission_mode: settings::ToolPermissionMode) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = tool_permission_mode;
            AgentSettings::override_global(settings, cx);
        });
    }

    async fn test_project(
        tree: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<Project>) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), tree).await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();
        (fs, project)
    }

    fn node(id: &str, title: &str, summary: &str) -> NodeInput {
        NodeInput {
            id: Some(id.to_string()),
            kind: NodeKind::Component,
            title: title.to_string(),
            summary: summary.to_string(),
            body: String::new(),
            anchors: Vec::new(),
            links: Vec::new(),
        }
    }

    fn anchor(path: &str) -> Anchor {
        Anchor {
            path: path.to_string(),
            symbol: None,
        }
    }

    async fn query(
        project: &Entity<Project>,
        input: QueryInput,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(QueryKnowledgeGraphTool::new(project.clone()));
        let (event_stream, _event_rx) = ToolCallEventStream::test();
        cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await
    }

    async fn update(
        project: &Entity<Project>,
        input: UpdateInput,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(UpdateKnowledgeGraphTool::new(project.clone()));
        let (event_stream, _event_rx) = ToolCallEventStream::test();
        cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await
    }

    #[gpui::test]
    async fn test_update_then_query(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Allow);
        let (fs, project) = test_project(json!({ "src": { "auth.rs": "" } }), cx).await;

        let empty = query(&project, QueryInput::default(), cx).await;
        assert_eq!(
            empty.as_deref(),
            Ok(
                "The knowledge graph is empty. Record what you learn with `update_knowledge_graph`."
            )
        );
        assert!(
            !fs.is_dir(path!("/root/project/.zed").as_ref()).await,
            "queries must not create files"
        );

        let summary = update(
            &project,
            UpdateInput {
                nodes: vec![
                    NodeInput {
                        body: "Tokens are opaque.".into(),
                        // One anchor in the form Zed's agent uses, with the
                        // project root's name first, and one without it.
                        anchors: vec![anchor("project/src/auth.rs"), anchor("src/gone.rs")],
                        ..node("auth", "Authentication", "Exchanges passwords for tokens.")
                    },
                    NodeInput {
                        links: vec![LinkInput {
                            relation: Relation::PartOf,
                            to: "auth".into(),
                        }],
                        ..node("tokens", "Tokens", "What a session token contains.")
                    },
                ],
                delete: Vec::new(),
            },
            cx,
        )
        .await;
        assert_eq!(summary.as_deref(), Ok("Created: auth, tokens."));

        let document = fs
            .load(path!("/root/project/.zed/knowledge/auth.md").as_ref())
            .await
            .expect("node file exists");
        assert_eq!(
            document,
            "---\n\
             kind: component\n\
             title: Authentication\n\
             summary: Exchanges passwords for tokens.\n\
             anchors:\n\
             - path: src/auth.rs\n\
             - path: src/gone.rs\n\
             ---\n\
             \n\
             Tokens are opaque.\n"
        );

        let by_path = query(
            &project,
            QueryInput {
                paths: vec!["project/src/auth.rs".into()],
                ..QueryInput::default()
            },
            cx,
        )
        .await;
        assert_eq!(
            by_path.as_deref(),
            Ok(
                "1 matching node:\n- auth [component] Authentication: Exchanges passwords for \
                 tokens. (anchor missing: src/gone.rs)"
            )
        );

        let full = query(
            &project,
            QueryInput {
                ids: vec!["auth".into()],
                ..QueryInput::default()
            },
            cx,
        )
        .await;
        assert_eq!(
            full.as_deref(),
            Ok("## auth [component] Authentication\n\
                Exchanges passwords for tokens.\n\
                Anchors: src/auth.rs, src/gone.rs (missing)\n\
                Linked from: tokens part_of (Tokens)\n\
                \n\
                Tokens are opaque.")
        );

        let summary = update(
            &project,
            UpdateInput {
                nodes: Vec::new(),
                delete: vec!["auth".into()],
            },
            cx,
        )
        .await;
        assert_eq!(
            summary.as_deref(),
            Ok("Deleted: auth. Removed links to deleted nodes from: tokens.")
        );
        assert!(
            !fs.is_file(path!("/root/project/.zed/knowledge/auth.md").as_ref())
                .await
        );
        let tokens = fs
            .load(path!("/root/project/.zed/knowledge/tokens.md").as_ref())
            .await
            .expect("node file exists");
        assert!(!tokens.contains("links:"), "{tokens}");
    }

    #[gpui::test]
    async fn test_query_reads_hand_written_files_and_reports_broken_ones(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Allow);
        let (_fs, project) = test_project(
            json!({
                ".zed": {
                    "knowledge": {
                        "good.md": "---\nkind: pitfall\ntitle: Good\nsummary: A good node.\n---\n",
                        "broken.md": "not a node",
                        "README.txt": "ignored",
                    }
                }
            }),
            cx,
        )
        .await;

        let output = query(&project, QueryInput::default(), cx).await;
        assert_eq!(
            output.as_deref(),
            Ok("Warnings while loading the knowledge graph:\n\
                - broken.md: expected YAML frontmatter starting with `---`\n\
                \n\
                1 node:\n\
                - good [pitfall] Good")
        );
    }

    #[gpui::test]
    async fn test_invalid_update_is_rejected_without_prompting_or_writing(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Confirm);
        let (fs, project) = test_project(json!({}), cx).await;

        let tool = Arc::new(UpdateKnowledgeGraphTool::new(project.clone()));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(UpdateInput {
                        nodes: vec![NodeInput {
                            links: vec![LinkInput {
                                relation: Relation::DependsOn,
                                to: "ghost".into(),
                            }],
                            ..node("first", "First", "The first node.")
                        }],
                        delete: Vec::new(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;
        let error = result.expect_err("update should be rejected");
        assert!(error.contains("which does not exist"), "{error}");
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "an invalid update must not ask for permission"
        );
        assert!(!fs.is_dir(path!("/root/project/.zed").as_ref()).await);
    }

    #[gpui::test]
    async fn test_update_asks_for_permission(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Confirm);
        let (fs, project) = test_project(json!({}), cx).await;
        let node_path = path!("/root/project/.zed/knowledge/first.md");

        for (allow, expect_written) in [(false, false), (true, true)] {
            let tool = Arc::new(UpdateKnowledgeGraphTool::new(project.clone()));
            let (event_stream, mut event_rx) = ToolCallEventStream::test();
            let task = cx.update(|cx| {
                tool.run(
                    ToolInput::resolved(UpdateInput {
                        nodes: vec![node("first", "First", "The first node.")],
                        delete: Vec::new(),
                    }),
                    event_stream,
                    cx,
                )
            });

            let authorization = event_rx.expect_authorization().await;
            let title = authorization
                .tool_call
                .title
                .value()
                .expect("authorization has a title")
                .to_string();
            assert_eq!(title, "Update knowledge graph. Created: first.");
            assert!(
                !fs.is_file(node_path.as_ref()).await,
                "nothing may be written before the user decides"
            );
            let outcome = if allow {
                acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("allow"),
                    acp::PermissionOptionKind::AllowOnce,
                )
            } else {
                acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("deny"),
                    acp::PermissionOptionKind::RejectOnce,
                )
            };
            authorization
                .response
                .send(outcome)
                .expect("authorization response should send");

            let result = task.await;
            assert_eq!(result.is_ok(), allow, "{result:?}");
            assert_eq!(fs.is_file(node_path.as_ref()).await, expect_written);
        }
    }

    #[gpui::test]
    async fn test_unchanged_update_does_not_ask_for_permission(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Allow);
        let (_fs, project) = test_project(json!({}), cx).await;
        let input = UpdateInput {
            nodes: vec![node("first", "First", "The first node.")],
            delete: Vec::new(),
        };
        assert_eq!(
            update(&project, input.clone(), cx).await.as_deref(),
            Ok("Created: first.")
        );

        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Confirm;
            AgentSettings::override_global(settings, cx);
        });
        let tool = Arc::new(UpdateKnowledgeGraphTool::new(project.clone()));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;
        assert_eq!(result.as_deref(), Ok("Unchanged: first."));
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "an update that changes nothing must not ask for permission"
        );
    }

    #[gpui::test]
    async fn test_knowledge_directory_must_not_escape_the_project(cx: &mut TestAppContext) {
        init_test(cx, settings::ToolPermissionMode::Allow);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": { ".zed": {} },
                "outside": { "secret.md": "---\nkind: concept\ntitle: Secret\nsummary: Leaked.\n---\n" },
            }),
        )
        .await;
        fs.create_symlink(
            path!("/root/project/.zed/knowledge").as_ref(),
            PathBuf::from(path!("/root/outside")),
        )
        .await
        .expect("symlink is created");
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let error = query(&project, QueryInput::default(), cx)
            .await
            .expect_err("query should be rejected");
        assert!(error.contains("outside the project"), "{error}");

        let error = update(
            &project,
            UpdateInput {
                nodes: vec![node("first", "First", "The first node.")],
                delete: Vec::new(),
            },
            cx,
        )
        .await
        .expect_err("update should be rejected");
        assert!(error.contains("outside the project"), "{error}");
        assert!(!fs.is_file(path!("/root/outside/first.md").as_ref()).await);
    }
}
