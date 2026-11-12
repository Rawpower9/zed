mod document;
pub mod mcp_server;
mod store;

pub use document::{parse_node, serialize_node};
pub use store::Store;

use anyhow::{Context as _, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Directory, relative to a project root, that holds one Markdown file per node.
pub const KNOWLEDGE_DIRECTORY: &str = ".zed/knowledge";
pub const NODE_FILE_EXTENSION: &str = "md";

pub const QUERY_TOOL_NAME: &str = "query_knowledge_graph";
pub const UPDATE_TOOL_NAME: &str = "update_knowledge_graph";

pub const MAX_ID_LENGTH: usize = 64;
pub const MAX_TITLE_LENGTH: usize = 80;
pub const MAX_SUMMARY_LENGTH: usize = 200;
pub const MAX_BODY_LENGTH: usize = 8000;
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const DEFAULT_INDEX_LIMIT: usize = 50;
pub const MAX_QUERY_LIMIT: usize = 50;

/// Guidance given to agents that reach the graph through a surface which
/// cannot rely on Zed's own system prompt, such as the MCP server.
pub const AGENT_INSTRUCTIONS: &str = "\
This project keeps a knowledge graph: short typed notes (concepts, components, decisions, \
conventions, pitfalls, procedures) that are linked to each other and anchored to files. \
Use it instead of writing notes, architecture summaries, or findings into documentation \
folders or scratch Markdown files.

- Before exploring unfamiliar code, call `query_knowledge_graph` with the paths or terms \
you are working on. Read full nodes only when their one-line summary is relevant.
- When you learn something non-obvious that a later session would otherwise have to \
rediscover, record it with `update_knowledge_graph`: one fact per node, a one-sentence \
summary, anchors to the files it describes, and links to related nodes.
- When a node is wrong or its anchors are reported missing, correct or delete it instead \
of adding a contradicting node.
- Do not record what the code or git history already states plainly, or progress on the \
current task.";

const WINDOWS_RESERVED_FILE_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        ensure!(!id.is_empty(), "node id must not be empty");
        ensure!(
            id.len() <= MAX_ID_LENGTH,
            "node id `{id}` is longer than {MAX_ID_LENGTH} characters"
        );
        ensure!(
            id.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !id.starts_with('-')
                && !id.ends_with('-'),
            "node id `{id}` must contain only lowercase letters, digits, and hyphens, and must \
             not start or end with a hyphen"
        );
        ensure!(
            !WINDOWS_RESERVED_FILE_STEMS.contains(&id.as_str()),
            "node id `{id}` is a reserved file name on Windows"
        );
        Ok(Self(id))
    }

    /// Derives an id from a title by lowercasing it and replacing every run of
    /// characters that cannot appear in an id with a single hyphen.
    pub fn from_title(title: &str) -> Result<Self> {
        let mut id = String::new();
        for character in title.chars() {
            if character.is_ascii_alphanumeric() {
                id.push(character.to_ascii_lowercase());
            } else if !id.is_empty() && !id.ends_with('-') {
                id.push('-');
            }
        }
        id.truncate(MAX_ID_LENGTH);
        let id = id.trim_end_matches('-');
        Self::new(id)
            .with_context(|| format!("could not derive a node id from title `{title}`; pass `id`"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> String {
        format!("{}.{NODE_FILE_EXTENSION}", self.0)
    }

    /// Returns `None` when the file is not a node file at all, so that callers
    /// can skip unrelated files without reporting them.
    pub fn from_file_name(file_name: &str) -> Option<Result<Self>> {
        let stem = file_name.strip_suffix(&format!(".{NODE_FILE_EXTENSION}"))?;
        Some(Self::new(stem))
    }
}

impl TryFrom<String> for NodeId {
    type Error = anyhow::Error;

    fn try_from(id: String) -> Result<Self> {
        Self::new(id)
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// An idea or term needed to understand the project.
    Concept,
    /// A module, service, or other unit of the system and what it is responsible for.
    Component,
    /// A choice that was made, with the reason for it.
    Decision,
    /// A rule the project follows that is not enforced by tooling.
    Convention,
    /// Something that looks right but breaks, and how to avoid it.
    Pitfall,
    /// The steps to accomplish a recurring task.
    Procedure,
}

impl NodeKind {
    pub const ALL: [NodeKind; 6] = [
        NodeKind::Concept,
        NodeKind::Component,
        NodeKind::Decision,
        NodeKind::Convention,
        NodeKind::Pitfall,
        NodeKind::Procedure,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Concept => "concept",
            NodeKind::Component => "component",
            NodeKind::Decision => "decision",
            NodeKind::Convention => "convention",
            NodeKind::Pitfall => "pitfall",
            NodeKind::Procedure => "procedure",
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// The two nodes are worth reading together.
    RelatedTo,
    /// This node cannot be understood or changed without the other one.
    DependsOn,
    /// This node is a piece of the other one.
    PartOf,
    /// This node replaces the other one, which is kept for history.
    Supersedes,
}

impl Relation {
    pub fn as_str(self) -> &'static str {
        match self {
            Relation::RelatedTo => "related_to",
            Relation::DependsOn => "depends_on",
            Relation::PartOf => "part_of",
            Relation::Supersedes => "supersedes",
        }
    }
}

impl fmt::Display for Relation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Anchor {
    /// Path of the file or directory this node describes, relative to the project root, with `/` separators.
    pub path: String,
    /// Name of a symbol in that file, when the node is about one symbol rather than the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

impl fmt::Display for Anchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.path)?;
        if let Some(symbol) = &self.symbol {
            write!(formatter, "#{symbol}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Link {
    pub relation: Relation,
    pub to: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub title: String,
    pub summary: String,
    pub body: String,
    pub anchors: Vec<Anchor>,
    pub links: Vec<Link>,
}

/// Reads the project's knowledge graph: short typed notes (concepts, components, decisions, conventions, pitfalls, procedures) recorded by earlier sessions, linked to each other and anchored to files.
///
/// Call this before exploring unfamiliar code; it is much cheaper than rediscovering the same facts by reading files. With no arguments it lists every node. `text`, `paths`, and `kind` search the graph and return one line per node. `ids` returns full nodes, including their bodies and links.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueryInput {
    /// Words to search for in node ids, titles, summaries, anchors, and bodies. Nodes matching more of the words rank higher.
    #[serde(default)]
    pub text: Option<String>,
    /// Files or directories, relative to the project root. Returns nodes anchored to these paths, to anything inside them, or to a directory containing them.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Ids of nodes to return in full.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Restricts a search or listing to one kind of node.
    #[serde(default)]
    pub kind: Option<NodeKind>,
    /// Maximum number of nodes to list. Defaults to 10 for searches and 50 for the full listing; the maximum is 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Records knowledge in the project's knowledge graph so that later sessions do not have to rediscover it. Use this instead of writing notes, architecture summaries, or findings to documentation folders or scratch files.
///
/// Write one fact per node: a non-obvious concept, a component's responsibility, a decision and its reason, a convention, a pitfall, or a procedure. Do not record what the code or git history already states plainly, or progress on the current task. Passing the `id` of an existing node replaces that node entirely. Link targets must already exist or be created in the same call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpdateInput {
    /// Nodes to create or replace.
    #[serde(default)]
    pub nodes: Vec<NodeInput>,
    /// Ids of nodes to delete. Links pointing at them are removed from the remaining nodes.
    #[serde(default)]
    pub delete: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeInput {
    /// Lowercase letters, digits, and hyphens, at most 64 characters. Omit it to derive the id from the title when creating a node; pass it to replace an existing node.
    #[serde(default)]
    pub id: Option<String>,
    pub kind: NodeKind,
    /// A single line of at most 80 characters naming the fact.
    pub title: String,
    /// A single sentence of at most 200 characters that is useful on its own, because searches return only this line.
    pub summary: String,
    /// Markdown with the detail a reader needs after the summary, at most 8000 characters. Split longer material into several linked nodes.
    #[serde(default)]
    pub body: String,
    /// Files, directories, or symbols this node describes.
    #[serde(default)]
    pub anchors: Vec<Anchor>,
    /// Relationships from this node to other nodes.
    #[serde(default)]
    pub links: Vec<LinkInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinkInput {
    pub relation: Relation,
    /// Id of the node this link points to.
    pub to: String,
}

/// Which node files an update changed, so that callers can persist exactly
/// those files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub created: Vec<NodeId>,
    pub updated: Vec<NodeId>,
    pub unchanged: Vec<NodeId>,
    pub deleted: Vec<NodeId>,
    /// Nodes that were rewritten only because they linked to a deleted node.
    pub relinked: Vec<NodeId>,
}

impl UpdateOutcome {
    pub fn written(&self) -> impl Iterator<Item = &NodeId> {
        self.created
            .iter()
            .chain(&self.updated)
            .chain(&self.relinked)
    }

    pub fn summary(&self) -> String {
        let sections = [
            ("Created", &self.created),
            ("Updated", &self.updated),
            ("Unchanged", &self.unchanged),
            ("Deleted", &self.deleted),
            ("Removed links to deleted nodes from", &self.relinked),
        ];
        let mut summary = String::new();
        for (label, ids) in sections {
            if ids.is_empty() {
                continue;
            }
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(label);
            summary.push_str(": ");
            summary.push_str(&join_ids(ids));
            summary.push('.');
        }
        summary
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    nodes: BTreeMap<NodeId, Node>,
}

impl Graph {
    /// Builds a graph from `(file name, file contents)` pairs. Files that are
    /// not node files are skipped, and files that fail to parse are reported
    /// as warnings rather than failing the whole load, so that one malformed
    /// file cannot hide the rest of the graph.
    pub fn from_documents(
        documents: impl IntoIterator<Item = (String, String)>,
    ) -> (Self, Vec<String>) {
        let mut graph = Graph::default();
        let mut warnings = Vec::new();
        for (file_name, content) in documents {
            let Some(id) = NodeId::from_file_name(&file_name) else {
                continue;
            };
            let node = id.and_then(|id| parse_node(id, &content));
            match node {
                Ok(node) => {
                    graph.nodes.insert(node.id.clone(), node);
                }
                Err(error) => warnings.push(format!("{file_name}: {error:#}")),
            }
        }
        for node in graph.nodes.values() {
            for link in &node.links {
                if !graph.nodes.contains_key(&link.to) {
                    warnings.push(format!(
                        "{}: links to missing node `{}`",
                        node.id.file_name(),
                        link.to
                    ));
                }
            }
        }
        (graph, warnings)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn get(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub fn anchor_paths(&self) -> BTreeSet<&str> {
        self.nodes
            .values()
            .flat_map(|node| node.anchors.iter().map(|anchor| anchor.path.as_str()))
            .collect()
    }

    /// Renders the answer to a query as compact text for a model to read.
    ///
    /// `missing_anchor_paths` lists anchor paths that no longer exist in the
    /// project; nodes anchored to them are flagged so that stale knowledge is
    /// visible to the reader instead of being trusted silently.
    pub fn query(&self, input: &QueryInput, missing_anchor_paths: &BTreeSet<String>) -> String {
        if self.nodes.is_empty() {
            return format!(
                "The knowledge graph is empty. Record what you learn with `{UPDATE_TOOL_NAME}`."
            );
        }

        let text_terms = input.text.as_deref().map(search_terms).unwrap_or_default();
        let is_search = !text_terms.is_empty() || !input.paths.is_empty();

        let mut output = String::new();

        let mut query_paths = Vec::new();
        for path in &input.paths {
            match normalize_anchor_path(path) {
                Ok(path) => query_paths.push(path),
                Err(error) => output.push_str(&format!("Ignored path `{path}`: {error}.\n")),
            }
        }
        // An empty filter must match nothing, so that a search whose paths
        // were all invalid does not fall back to listing the whole graph.
        let path_filter = (!input.paths.is_empty()).then_some(query_paths.as_slice());

        for id in &input.ids {
            let node = NodeId::new(id.trim())
                .ok()
                .and_then(|id| self.nodes.get(&id));
            match node {
                Some(node) => self.render_full_node(node, missing_anchor_paths, &mut output),
                None => output.push_str(&format!("No node with id `{id}`.\n\n")),
            }
        }

        if is_search {
            let limit = query_limit(input.limit, DEFAULT_SEARCH_LIMIT);
            let mut matches = self
                .nodes
                .values()
                .filter(|node| input.kind.is_none_or(|kind| node.kind == kind))
                .filter_map(|node| {
                    let relevance = search_relevance(node, &text_terms, path_filter)?;
                    Some((relevance, node))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|(left_relevance, left), (right_relevance, right)| {
                right_relevance
                    .cmp(left_relevance)
                    .then_with(|| left.id.cmp(&right.id))
            });
            if matches.is_empty() {
                output.push_str("No matching nodes.\n");
            } else {
                render_list_header("matching node", matches.len(), limit, &mut output);
                for (_, node) in matches.iter().take(limit) {
                    render_search_entry(node, missing_anchor_paths, &mut output);
                }
            }
        } else if input.ids.is_empty() {
            let limit = query_limit(input.limit, DEFAULT_INDEX_LIMIT);
            let listed = self
                .nodes
                .values()
                .filter(|node| input.kind.is_none_or(|kind| node.kind == kind))
                .collect::<Vec<_>>();
            if listed.is_empty() {
                output.push_str("No matching nodes.\n");
            } else {
                render_list_header("node", listed.len(), limit, &mut output);
                for node in listed.iter().take(limit) {
                    output.push_str(&format!("- {} [{}] {}\n", node.id, node.kind, node.title));
                }
            }
        }

        output.truncate(output.trim_end().len());
        output
    }

    fn render_full_node(
        &self,
        node: &Node,
        missing_anchor_paths: &BTreeSet<String>,
        output: &mut String,
    ) {
        output.push_str(&format!(
            "## {} [{}] {}\n{}\n",
            node.id, node.kind, node.title, node.summary
        ));
        if !node.anchors.is_empty() {
            let anchors = node
                .anchors
                .iter()
                .map(|anchor| {
                    if missing_anchor_paths.contains(&anchor.path) {
                        format!("{anchor} (missing)")
                    } else {
                        anchor.to_string()
                    }
                })
                .collect::<Vec<_>>();
            output.push_str(&format!("Anchors: {}\n", anchors.join(", ")));
        }
        if !node.links.is_empty() {
            let links = node
                .links
                .iter()
                .map(|link| match self.nodes.get(&link.to) {
                    Some(target) => format!("{} {} ({})", link.relation, link.to, target.title),
                    None => format!("{} {} (missing node)", link.relation, link.to),
                })
                .collect::<Vec<_>>();
            output.push_str(&format!("Links: {}\n", links.join("; ")));
        }
        let inbound = self
            .nodes
            .values()
            .flat_map(|source| {
                source
                    .links
                    .iter()
                    .filter(|link| link.to == node.id)
                    .map(move |link| format!("{} {} ({})", source.id, link.relation, source.title))
            })
            .collect::<Vec<_>>();
        if !inbound.is_empty() {
            output.push_str(&format!("Linked from: {}\n", inbound.join("; ")));
        }
        if !node.body.is_empty() {
            output.push('\n');
            output.push_str(&node.body);
            output.push('\n');
        }
        output.push('\n');
    }

    /// Applies an update atomically: the graph is left untouched unless every
    /// part of the input is valid.
    pub fn apply_update(&mut self, input: UpdateInput) -> Result<UpdateOutcome> {
        ensure!(
            !input.nodes.is_empty() || !input.delete.is_empty(),
            "nothing to do: pass `nodes` to create or replace nodes, or `delete` to remove them"
        );

        let mut deleted = BTreeSet::new();
        for id in &input.delete {
            let id = NodeId::new(id.trim())?;
            ensure!(
                self.nodes.contains_key(&id),
                "cannot delete `{id}`: no such node"
            );
            deleted.insert(id);
        }

        let mut replacements = BTreeMap::new();
        for node_input in input.nodes {
            let node = self.node_from_input(node_input)?;
            ensure!(
                !deleted.contains(&node.id),
                "node `{}` is both written and deleted in the same update",
                node.id
            );
            ensure!(
                !replacements.contains_key(&node.id),
                "node `{}` appears more than once in the same update",
                node.id
            );
            replacements.insert(node.id.clone(), node);
        }

        for node in replacements.values() {
            for link in &node.links {
                ensure!(
                    !deleted.contains(&link.to),
                    "node `{}` links to `{}`, which this update deletes",
                    node.id,
                    link.to
                );
                ensure!(
                    replacements.contains_key(&link.to) || self.nodes.contains_key(&link.to),
                    "node `{}` links to `{}`, which does not exist; create it in the same update \
                     or remove the link",
                    node.id,
                    link.to
                );
            }
        }

        let mut outcome = UpdateOutcome {
            deleted: deleted.iter().cloned().collect(),
            ..UpdateOutcome::default()
        };
        for id in &deleted {
            self.nodes.remove(id);
        }
        for (id, node) in replacements {
            match self.nodes.get(&id) {
                Some(existing) if *existing == node => outcome.unchanged.push(id),
                Some(_) => {
                    outcome.updated.push(id.clone());
                    self.nodes.insert(id, node);
                }
                None => {
                    outcome.created.push(id.clone());
                    self.nodes.insert(id, node);
                }
            }
        }
        if !deleted.is_empty() {
            for node in self.nodes.values_mut() {
                let link_count = node.links.len();
                node.links.retain(|link| !deleted.contains(&link.to));
                if node.links.len() != link_count {
                    outcome.relinked.push(node.id.clone());
                }
            }
        }
        Ok(outcome)
    }

    fn node_from_input(&self, input: NodeInput) -> Result<Node> {
        let title = single_line_field("title", &input.title, MAX_TITLE_LENGTH)?;
        let id = match input.id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => NodeId::new(id)?,
            _ => {
                let id = NodeId::from_title(&title)?;
                ensure!(
                    !self.nodes.contains_key(&id),
                    "a node with id `{id}` already exists; pass `id` explicitly to replace it, \
                     or choose a different title"
                );
                id
            }
        };
        let summary = single_line_field("summary", &input.summary, MAX_SUMMARY_LENGTH)
            .with_context(|| format!("node `{id}`"))?;
        let body = input.body.trim().to_string();
        let body_length = body.chars().count();
        ensure!(
            body_length <= MAX_BODY_LENGTH,
            "node `{id}`: body is {body_length} characters, the maximum is {MAX_BODY_LENGTH}; \
             split it into several linked nodes"
        );

        let mut anchors = Vec::new();
        for anchor in input.anchors {
            let path = normalize_anchor_path(&anchor.path)
                .with_context(|| format!("node `{id}`: anchor path `{}`", anchor.path))?;
            let symbol = anchor
                .symbol
                .map(|symbol| symbol.trim().to_string())
                .filter(|symbol| !symbol.is_empty());
            if let Some(symbol) = &symbol {
                ensure!(
                    !symbol.contains(['\n', '\r']),
                    "node `{id}`: anchor symbol must be a single line"
                );
            }
            let anchor = Anchor { path, symbol };
            if !anchors.contains(&anchor) {
                anchors.push(anchor);
            }
        }

        let mut links = Vec::new();
        for link in input.links {
            let to =
                NodeId::new(link.to.trim()).with_context(|| format!("node `{id}`: link target"))?;
            ensure!(to != id, "node `{id}` must not link to itself");
            let link = Link {
                relation: link.relation,
                to,
            };
            if !links.contains(&link) {
                links.push(link);
            }
        }

        Ok(Node {
            id,
            kind: input.kind,
            title,
            summary,
            body,
            anchors,
            links,
        })
    }
}

/// Normalizes a project-relative path to forward slashes without `.`
/// components or a trailing slash, rejecting anything that could point outside
/// the project.
pub fn normalize_anchor_path(path: &str) -> Result<String> {
    let path = path.trim().replace('\\', "/");
    ensure!(!path.is_empty(), "path must not be empty");
    ensure!(!path.contains(['\n', '\r']), "path must be a single line");
    ensure!(
        !path.starts_with('/'),
        "path must be relative to the project root"
    );
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => bail!("path must not contain `..`"),
            component => {
                ensure!(
                    !(components.is_empty() && component.ends_with(':')),
                    "path must be relative to the project root"
                );
                components.push(component);
            }
        }
    }
    ensure!(
        !components.is_empty(),
        "path must name a file or directory inside the project"
    );
    Ok(components.join("/"))
}

/// Formats load warnings for inclusion in tool output. Returns an empty
/// string when there are none.
pub fn render_load_warnings(warnings: &[String]) -> String {
    const MAX_REPORTED_WARNINGS: usize = 5;
    if warnings.is_empty() {
        return String::new();
    }
    let mut output = String::from("Warnings while loading the knowledge graph:\n");
    for warning in warnings.iter().take(MAX_REPORTED_WARNINGS) {
        output.push_str(&format!("- {warning}\n"));
    }
    if warnings.len() > MAX_REPORTED_WARNINGS {
        output.push_str(&format!(
            "- and {} more\n",
            warnings.len() - MAX_REPORTED_WARNINGS
        ));
    }
    output.push('\n');
    output
}

fn single_line_field(name: &str, value: &str, max_length: usize) -> Result<String> {
    let value = value.trim();
    ensure!(!value.is_empty(), "{name} must not be empty");
    ensure!(
        !value.contains(['\n', '\r']),
        "{name} must be a single line"
    );
    let length = value.chars().count();
    ensure!(
        length <= max_length,
        "{name} is {length} characters, the maximum is {max_length}"
    );
    Ok(value.to_string())
}

fn join_ids(ids: &[NodeId]) -> String {
    ids.iter()
        .map(NodeId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn query_limit(requested: Option<usize>, default: usize) -> usize {
    requested.unwrap_or(default).clamp(1, MAX_QUERY_LIMIT)
}

fn search_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for term in text.split(|character: char| !(character.is_alphanumeric() || character == '_')) {
        let term = term.to_lowercase();
        if !term.is_empty() && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// How well a node answers a search. Ordered so that a node matching more of
/// the searched words always outranks one matching fewer, however strongly
/// the latter matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SearchRelevance {
    matched_terms: usize,
    score: usize,
}

fn search_relevance(
    node: &Node,
    text_terms: &[String],
    path_filter: Option<&[String]>,
) -> Option<SearchRelevance> {
    let mut relevance = SearchRelevance {
        matched_terms: 0,
        score: 0,
    };

    if let Some(query_paths) = path_filter {
        let path_score = node
            .anchors
            .iter()
            .flat_map(|anchor| {
                query_paths
                    .iter()
                    .filter_map(|query_path| path_match_score(&anchor.path, query_path))
            })
            .max()?;
        relevance.score += path_score;
    }

    if !text_terms.is_empty() {
        let id = node.id.as_str();
        let title = node.title.to_lowercase();
        let summary = node.summary.to_lowercase();
        let body = node.body.to_lowercase();
        let anchors = node
            .anchors
            .iter()
            .map(|anchor| anchor.to_string().to_lowercase())
            .collect::<Vec<_>>();
        for term in text_terms {
            let mut term_score = 0;
            if title.contains(term) {
                term_score += 8;
            }
            if id.contains(term) {
                term_score += 6;
            }
            if summary.contains(term) {
                term_score += 4;
            }
            if anchors.iter().any(|anchor| anchor.contains(term)) {
                term_score += 3;
            }
            if body.contains(term) {
                term_score += 1;
            }
            if term_score > 0 {
                relevance.matched_terms += 1;
                relevance.score += term_score;
            }
        }
        if relevance.matched_terms == 0 {
            return None;
        }
    }

    Some(relevance)
}

/// Scores how closely an anchor relates to a queried path: the same path
/// scores highest, then an anchor inside a queried directory, then an anchor
/// on a directory that contains the queried path.
fn path_match_score(anchor_path: &str, query_path: &str) -> Option<usize> {
    if anchor_path == query_path {
        Some(3)
    } else if is_path_inside(anchor_path, query_path) {
        Some(2)
    } else if is_path_inside(query_path, anchor_path) {
        Some(1)
    } else {
        None
    }
}

fn is_path_inside(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn render_list_header(noun: &str, total: usize, limit: usize, output: &mut String) {
    let plural = if total == 1 { "" } else { "s" };
    if total > limit {
        output.push_str(&format!(
            "{total} {noun}{plural}, showing the first {limit}:\n"
        ));
    } else {
        output.push_str(&format!("{total} {noun}{plural}:\n"));
    }
}

fn render_search_entry(node: &Node, missing_anchor_paths: &BTreeSet<String>, output: &mut String) {
    output.push_str(&format!(
        "- {} [{}] {}: {}",
        node.id, node.kind, node.title, node.summary
    ));
    let missing = node
        .anchors
        .iter()
        .filter(|anchor| missing_anchor_paths.contains(&anchor.path))
        .map(|anchor| anchor.path.as_str())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        output.push_str(&format!(" (anchor missing: {})", missing.join(", ")));
    }
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn node_input(id: &str, kind: NodeKind, title: &str, summary: &str) -> NodeInput {
        NodeInput {
            id: Some(id.to_string()),
            kind,
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

    fn link(relation: Relation, to: &str) -> LinkInput {
        LinkInput {
            relation,
            to: to.to_string(),
        }
    }

    fn sample_graph() -> Graph {
        let mut graph = Graph::default();
        graph
            .apply_update(UpdateInput {
                nodes: vec![
                    NodeInput {
                        body: "Sessions are stored in Redis.\n\nKeys expire after a day.".into(),
                        anchors: vec![Anchor {
                            path: "src/auth/session.rs".into(),
                            symbol: Some("SessionStore".into()),
                        }],
                        ..node_input(
                            "session-store",
                            NodeKind::Component,
                            "Session store",
                            "Owns login sessions and their expiry.",
                        )
                    },
                    NodeInput {
                        anchors: vec![anchor("src/auth")],
                        links: vec![link(Relation::DependsOn, "session-store")],
                        ..node_input(
                            "auth-flow",
                            NodeKind::Concept,
                            "Authentication flow",
                            "Login exchanges a password for a session token.",
                        )
                    },
                    NodeInput {
                        anchors: vec![anchor("src/billing/invoice.rs")],
                        ..node_input(
                            "invoice-rounding",
                            NodeKind::Pitfall,
                            "Invoice totals round per line",
                            "Rounding the invoice total instead of each line breaks tax reports.",
                        )
                    },
                ],
                delete: Vec::new(),
            })
            .expect("sample graph is valid");
        graph
    }

    fn no_missing_anchors() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn node_id_validation() {
        assert!(NodeId::new("auth-flow-2").is_ok());
        for invalid in [
            "",
            "Auth",
            "auth flow",
            "-auth",
            "auth-",
            "a/b",
            "../x",
            "nul",
            "é",
        ] {
            assert!(
                NodeId::new(invalid).is_err(),
                "`{invalid}` should be rejected"
            );
        }
        assert!(NodeId::new("a".repeat(MAX_ID_LENGTH)).is_ok());
        assert!(NodeId::new("a".repeat(MAX_ID_LENGTH + 1)).is_err());
    }

    #[test]
    fn node_id_from_title() {
        assert_eq!(
            NodeId::from_title("  Why we use `Arc<str>`, not String!  ")
                .expect("title has usable characters")
                .as_str(),
            "why-we-use-arc-str-not-string"
        );
        let long_title = "word ".repeat(40);
        let id = NodeId::from_title(&long_title).expect("long titles are truncated");
        assert!(id.as_str().len() <= MAX_ID_LENGTH);
        assert!(!id.as_str().ends_with('-'));
        assert!(NodeId::from_title("日本語").is_err());
        assert!(NodeId::from_title("???").is_err());
    }

    #[test]
    fn node_id_from_file_name() {
        assert!(NodeId::from_file_name("README.txt").is_none());
        assert!(NodeId::from_file_name("auth-flow.md").is_some_and(|id| id.is_ok()));
        assert!(NodeId::from_file_name("Not A Slug.md").is_some_and(|id| id.is_err()));
    }

    #[test]
    fn anchor_path_normalization() {
        assert_eq!(
            normalize_anchor_path(" ./src//auth/./session.rs ").expect("valid path"),
            "src/auth/session.rs"
        );
        assert_eq!(
            normalize_anchor_path("src\\auth\\").expect("valid path"),
            "src/auth"
        );
        for invalid in [
            "",
            "  ",
            ".",
            "/etc/passwd",
            "../secrets",
            "src/../../x",
            "C:\\x",
            "a\nb",
        ] {
            assert!(
                normalize_anchor_path(invalid).is_err(),
                "`{invalid}` should be rejected"
            );
        }
    }

    #[test]
    fn empty_graph_query_points_to_the_update_tool() {
        let graph = Graph::default();
        assert_eq!(
            graph.query(&QueryInput::default(), &no_missing_anchors()),
            "The knowledge graph is empty. Record what you learn with `update_knowledge_graph`."
        );
    }

    #[test]
    fn query_without_arguments_lists_every_node() {
        assert_eq!(
            sample_graph().query(&QueryInput::default(), &no_missing_anchors()),
            "3 nodes:\n\
             - auth-flow [concept] Authentication flow\n\
             - invoice-rounding [pitfall] Invoice totals round per line\n\
             - session-store [component] Session store"
        );
    }

    #[test]
    fn listing_respects_kind_and_limit() {
        let graph = sample_graph();
        assert_eq!(
            graph.query(
                &QueryInput {
                    kind: Some(NodeKind::Pitfall),
                    ..QueryInput::default()
                },
                &no_missing_anchors()
            ),
            "1 node:\n- invoice-rounding [pitfall] Invoice totals round per line"
        );
        assert_eq!(
            graph.query(
                &QueryInput {
                    limit: Some(1),
                    ..QueryInput::default()
                },
                &no_missing_anchors()
            ),
            "3 nodes, showing the first 1:\n- auth-flow [concept] Authentication flow"
        );
        assert_eq!(
            graph.query(
                &QueryInput {
                    kind: Some(NodeKind::Procedure),
                    ..QueryInput::default()
                },
                &no_missing_anchors()
            ),
            "No matching nodes."
        );
    }

    #[test]
    fn text_search_ranks_nodes_matching_more_terms_first() {
        let output = sample_graph().query(
            &QueryInput {
                text: Some("session expiry".into()),
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(
            output,
            "2 matching nodes:\n\
             - session-store [component] Session store: Owns login sessions and their expiry.\n\
             - auth-flow [concept] Authentication flow: Login exchanges a password for a session token."
        );
    }

    #[test]
    fn text_search_matches_bodies_and_anchor_symbols() {
        let graph = sample_graph();
        let by_body = graph.query(
            &QueryInput {
                text: Some("redis".into()),
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert!(by_body.starts_with("1 matching node:\n- session-store "));
        let by_symbol = graph.query(
            &QueryInput {
                text: Some("SessionStore".into()),
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert!(by_symbol.starts_with("1 matching node:\n- session-store "));
        let nothing = graph.query(
            &QueryInput {
                text: Some("kubernetes".into()),
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(nothing, "No matching nodes.");
    }

    #[test]
    fn path_search_matches_exact_nested_and_containing_anchors() {
        let graph = sample_graph();
        let for_file = graph.query(
            &QueryInput {
                paths: vec!["./src/auth/session.rs".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(
            for_file,
            "2 matching nodes:\n\
             - session-store [component] Session store: Owns login sessions and their expiry.\n\
             - auth-flow [concept] Authentication flow: Login exchanges a password for a session token."
        );

        let for_directory = graph.query(
            &QueryInput {
                paths: vec!["src/auth/".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(
            for_directory,
            "2 matching nodes:\n\
             - auth-flow [concept] Authentication flow: Login exchanges a password for a session token.\n\
             - session-store [component] Session store: Owns login sessions and their expiry."
        );

        let sibling_prefix = graph.query(
            &QueryInput {
                paths: vec!["src/auth_legacy".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(sibling_prefix, "No matching nodes.");
    }

    #[test]
    fn invalid_search_paths_are_reported_and_match_nothing() {
        let output = sample_graph().query(
            &QueryInput {
                paths: vec!["../outside".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(
            output,
            "Ignored path `../outside`: path must not contain `..`.\nNo matching nodes."
        );
    }

    #[test]
    fn path_and_text_search_must_both_match() {
        let output = sample_graph().query(
            &QueryInput {
                text: Some("rounding".into()),
                paths: vec!["src/auth".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert_eq!(output, "No matching nodes.");
    }

    #[test]
    fn reading_ids_returns_full_nodes_with_links_in_both_directions() {
        let graph = sample_graph();
        assert_eq!(
            graph.query(
                &QueryInput {
                    ids: vec!["session-store".into(), "auth-flow".into(), "nope".into()],
                    ..QueryInput::default()
                },
                &no_missing_anchors()
            ),
            "## session-store [component] Session store\n\
             Owns login sessions and their expiry.\n\
             Anchors: src/auth/session.rs#SessionStore\n\
             Linked from: auth-flow depends_on (Authentication flow)\n\
             \n\
             Sessions are stored in Redis.\n\
             \n\
             Keys expire after a day.\n\
             \n\
             ## auth-flow [concept] Authentication flow\n\
             Login exchanges a password for a session token.\n\
             Anchors: src/auth\n\
             Links: depends_on session-store (Session store)\n\
             \n\
             No node with id `nope`."
        );
    }

    #[test]
    fn missing_anchors_are_flagged() {
        let graph = sample_graph();
        let missing = BTreeSet::from(["src/billing/invoice.rs".to_string()]);
        let search = graph.query(
            &QueryInput {
                text: Some("invoice".into()),
                ..QueryInput::default()
            },
            &missing,
        );
        assert!(search.ends_with("(anchor missing: src/billing/invoice.rs)"));
        let full = graph.query(
            &QueryInput {
                ids: vec!["invoice-rounding".into()],
                ..QueryInput::default()
            },
            &missing,
        );
        assert!(full.contains("Anchors: src/billing/invoice.rs (missing)"));
    }

    #[test]
    fn update_reports_created_updated_and_unchanged_nodes() {
        let mut graph = sample_graph();
        let unchanged = NodeInput {
            anchors: vec![anchor("src/billing/invoice.rs")],
            ..node_input(
                "invoice-rounding",
                NodeKind::Pitfall,
                "Invoice totals round per line",
                "Rounding the invoice total instead of each line breaks tax reports.",
            )
        };
        let outcome = graph
            .apply_update(UpdateInput {
                nodes: vec![
                    unchanged,
                    node_input(
                        "session-store",
                        NodeKind::Component,
                        "Session store",
                        "Owns login sessions.",
                    ),
                    NodeInput {
                        id: None,
                        ..node_input("", NodeKind::Decision, "Use Redis for sessions", "Because.")
                    },
                ],
                delete: Vec::new(),
            })
            .expect("update is valid");
        assert_eq!(
            outcome.summary(),
            "Created: use-redis-for-sessions. Updated: session-store. Unchanged: invoice-rounding."
        );
        assert_eq!(
            outcome.written().map(NodeId::as_str).collect::<Vec<_>>(),
            vec!["use-redis-for-sessions", "session-store"]
        );
        assert_eq!(graph.len(), 4);
    }

    #[test]
    fn deleting_a_node_removes_links_to_it() {
        let mut graph = sample_graph();
        let outcome = graph
            .apply_update(UpdateInput {
                nodes: Vec::new(),
                delete: vec!["session-store".into()],
            })
            .expect("delete is valid");
        assert_eq!(
            outcome.summary(),
            "Deleted: session-store. Removed links to deleted nodes from: auth-flow."
        );
        let auth_flow = graph
            .get(&NodeId::new("auth-flow").expect("valid id"))
            .expect("node still exists");
        assert!(auth_flow.links.is_empty());
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn nodes_created_together_may_link_to_each_other() {
        let mut graph = Graph::default();
        graph
            .apply_update(UpdateInput {
                nodes: vec![
                    NodeInput {
                        links: vec![link(Relation::PartOf, "b")],
                        ..node_input("a", NodeKind::Concept, "A", "First.")
                    },
                    NodeInput {
                        links: vec![link(Relation::RelatedTo, "a")],
                        ..node_input("b", NodeKind::Concept, "B", "Second.")
                    },
                ],
                delete: Vec::new(),
            })
            .expect("mutual links within one update are valid");
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn duplicate_anchors_and_links_are_collapsed() {
        let mut graph = sample_graph();
        graph
            .apply_update(UpdateInput {
                nodes: vec![NodeInput {
                    anchors: vec![anchor("src/x.rs"), anchor("./src/x.rs")],
                    links: vec![
                        link(Relation::RelatedTo, "auth-flow"),
                        link(Relation::RelatedTo, "auth-flow"),
                    ],
                    ..node_input("x", NodeKind::Concept, "X", "Something.")
                }],
                delete: Vec::new(),
            })
            .expect("update is valid");
        let node = graph
            .get(&NodeId::new("x").expect("valid id"))
            .expect("node exists");
        assert_eq!(node.anchors, vec![anchor("src/x.rs")]);
        assert_eq!(node.links.len(), 1);
    }

    #[test]
    fn invalid_updates_leave_the_graph_untouched() {
        let invalid_updates = [
            (UpdateInput::default(), "nothing to do"),
            (
                UpdateInput {
                    nodes: Vec::new(),
                    delete: vec!["ghost".into()],
                },
                "cannot delete `ghost`",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        links: vec![link(Relation::DependsOn, "ghost")],
                        ..node_input("new", NodeKind::Concept, "New", "A new node.")
                    }],
                    delete: Vec::new(),
                },
                "links to `ghost`, which does not exist",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        links: vec![link(Relation::DependsOn, "session-store")],
                        ..node_input("new", NodeKind::Concept, "New", "A new node.")
                    }],
                    delete: vec!["session-store".into()],
                },
                "which this update deletes",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input(
                        "auth-flow",
                        NodeKind::Concept,
                        "Auth",
                        "Summary.",
                    )],
                    delete: vec!["auth-flow".into()],
                },
                "both written and deleted",
            ),
            (
                UpdateInput {
                    nodes: vec![
                        node_input("twice", NodeKind::Concept, "One", "Summary."),
                        node_input("twice", NodeKind::Concept, "Two", "Summary."),
                    ],
                    delete: Vec::new(),
                },
                "appears more than once",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        links: vec![link(Relation::RelatedTo, "selfish")],
                        ..node_input("selfish", NodeKind::Concept, "Selfish", "Summary.")
                    }],
                    delete: Vec::new(),
                },
                "must not link to itself",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        id: None,
                        ..node_input("", NodeKind::Concept, "Session store", "Summary.")
                    }],
                    delete: Vec::new(),
                },
                "already exists; pass `id` explicitly",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input("new", NodeKind::Concept, "", "Summary.")],
                    delete: Vec::new(),
                },
                "title must not be empty",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input("new", NodeKind::Concept, "Title", "Two\nlines.")],
                    delete: Vec::new(),
                },
                "summary must be a single line",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input(
                        "new",
                        NodeKind::Concept,
                        "Title",
                        &"s".repeat(MAX_SUMMARY_LENGTH + 1),
                    )],
                    delete: Vec::new(),
                },
                "summary is 201 characters",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input(
                        "new",
                        NodeKind::Concept,
                        &"t".repeat(MAX_TITLE_LENGTH + 1),
                        "Summary.",
                    )],
                    delete: Vec::new(),
                },
                "title is 81 characters",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        body: "b".repeat(MAX_BODY_LENGTH + 1),
                        ..node_input("new", NodeKind::Concept, "Title", "Summary.")
                    }],
                    delete: Vec::new(),
                },
                "split it into several linked nodes",
            ),
            (
                UpdateInput {
                    nodes: vec![NodeInput {
                        anchors: vec![anchor("../outside.rs")],
                        ..node_input("new", NodeKind::Concept, "Title", "Summary.")
                    }],
                    delete: Vec::new(),
                },
                "must not contain `..`",
            ),
            (
                UpdateInput {
                    nodes: vec![node_input("Bad Id", NodeKind::Concept, "Title", "Summary.")],
                    delete: Vec::new(),
                },
                "must contain only lowercase letters",
            ),
        ];

        for (update, expected_error) in invalid_updates {
            let mut graph = sample_graph();
            let error = graph
                .apply_update(update)
                .expect_err("update should be rejected");
            let message = format!("{error:#}");
            assert!(
                message.contains(expected_error),
                "expected `{expected_error}` in `{message}`"
            );
            assert_eq!(graph, sample_graph());
        }
    }

    #[test]
    fn length_limits_count_characters_not_bytes() {
        let mut graph = Graph::default();
        graph
            .apply_update(UpdateInput {
                nodes: vec![node_input(
                    "unicode",
                    NodeKind::Concept,
                    &"é".repeat(MAX_TITLE_LENGTH),
                    &"é".repeat(MAX_SUMMARY_LENGTH),
                )],
                delete: Vec::new(),
            })
            .expect("limits are measured in characters");
    }

    #[test]
    fn from_documents_skips_other_files_and_reports_broken_ones() {
        let valid = "---\nkind: concept\ntitle: Valid\nsummary: A valid node.\nlinks:\n- relation: related_to\n  to: gone\n---\n";
        let (graph, warnings) = Graph::from_documents([
            ("valid.md".to_string(), valid.to_string()),
            ("notes.txt".to_string(), "not a node".to_string()),
            ("Bad Name.md".to_string(), valid.to_string()),
            ("broken.md".to_string(), "no frontmatter here".to_string()),
        ]);
        assert_eq!(graph.len(), 1);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].starts_with("Bad Name.md: "));
        assert!(warnings[1].starts_with("broken.md: "));
        assert_eq!(warnings[2], "valid.md: links to missing node `gone`");

        let rendered = render_load_warnings(&warnings);
        assert!(rendered.starts_with("Warnings while loading the knowledge graph:\n- Bad Name.md"));
        assert_eq!(render_load_warnings(&[]), "");
    }

    #[test]
    fn dangling_links_render_as_missing_nodes() {
        let document = "---\nkind: concept\ntitle: Valid\nsummary: A valid node.\nlinks:\n- relation: related_to\n  to: gone\n---\n";
        let (graph, _) = Graph::from_documents([("valid.md".to_string(), document.to_string())]);
        let output = graph.query(
            &QueryInput {
                ids: vec!["valid".into()],
                ..QueryInput::default()
            },
            &no_missing_anchors(),
        );
        assert!(output.contains("Links: related_to gone (missing node)"));
    }
}
