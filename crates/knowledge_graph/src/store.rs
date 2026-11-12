use crate::{
    Graph, KNOWLEDGE_DIRECTORY, QueryInput, UpdateInput, UpdateOutcome, render_load_warnings,
    serialize_node,
};
use anyhow::{Context as _, Result, ensure};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Reads and writes a project's knowledge graph directly on the local
/// filesystem. Used by surfaces that run outside of Zed, such as the MCP
/// server.
#[derive(Clone, Debug)]
pub struct Store {
    project_root: PathBuf,
}

impl Store {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn directory(&self) -> PathBuf {
        self.project_root.join(KNOWLEDGE_DIRECTORY)
    }

    /// Loads every node file. A missing directory is an empty graph, and
    /// unreadable or malformed files are returned as warnings.
    pub fn load(&self) -> Result<(Graph, Vec<String>)> {
        let directory = self.directory();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Graph::default(), Vec::new()));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", directory.display()));
            }
        };
        self.ensure_directory_is_inside_project(&directory)?;

        let mut read_warnings = Vec::new();
        let mut documents = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if crate::NodeId::from_file_name(&file_name).is_none() {
                continue;
            }
            match fs::read_to_string(entry.path()) {
                Ok(content) => documents.push((file_name, content)),
                Err(error) => read_warnings.push(format!("{file_name}: {error}")),
            }
        }
        documents.sort();

        let (graph, mut warnings) = Graph::from_documents(documents);
        read_warnings.append(&mut warnings);
        Ok((graph, read_warnings))
    }

    pub fn missing_anchor_paths(&self, graph: &Graph) -> BTreeSet<String> {
        graph
            .anchor_paths()
            .into_iter()
            .filter(|path| !self.project_root.join(path).exists())
            .map(str::to_string)
            .collect()
    }

    /// Writes the files an update changed. Each file is written to a
    /// temporary sibling and renamed into place so that a reader never sees a
    /// half-written node.
    pub fn persist(&self, graph: &Graph, outcome: &UpdateOutcome) -> Result<()> {
        let directory = self.directory();
        fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        self.ensure_directory_is_inside_project(&directory)?;

        for id in outcome.written() {
            let node = graph
                .get(id)
                .with_context(|| format!("node `{id}` is missing from the updated graph"))?;
            let document = serialize_node(node)?;
            let path = directory.join(id.file_name());
            let temporary_path = directory.join(format!(".{}.tmp", id.file_name()));
            fs::write(&temporary_path, document)
                .with_context(|| format!("writing {}", temporary_path.display()))?;
            fs::rename(&temporary_path, &path)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        for id in &outcome.deleted {
            let path = directory.join(id.file_name());
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("deleting {}", path.display()));
                }
            }
        }
        Ok(())
    }

    pub fn query(&self, input: &QueryInput) -> Result<String> {
        let (graph, warnings) = self.load()?;
        let missing_anchor_paths = self.missing_anchor_paths(&graph);
        let mut output = render_load_warnings(&warnings);
        output.push_str(&graph.query(input, &missing_anchor_paths));
        Ok(output)
    }

    pub fn update(&self, input: UpdateInput) -> Result<String> {
        let (mut graph, _) = self.load()?;
        let outcome = graph.apply_update(input)?;
        self.persist(&graph, &outcome)?;
        Ok(outcome.summary())
    }

    /// The knowledge directory may be a symlink. Following one that leaves
    /// the project would let a checked-in link redirect reads and writes to
    /// arbitrary locations.
    fn ensure_directory_is_inside_project(&self, directory: &Path) -> Result<()> {
        let canonical_root = fs::canonicalize(&self.project_root)
            .with_context(|| format!("resolving {}", self.project_root.display()))?;
        let canonical_directory = fs::canonicalize(directory)
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
    use crate::{Anchor, LinkInput, NodeInput, NodeKind, Relation};
    use pretty_assertions::assert_eq;

    fn node_input(id: &str, title: &str, summary: &str) -> NodeInput {
        NodeInput {
            id: Some(id.to_string()),
            kind: NodeKind::Concept,
            title: title.to_string(),
            summary: summary.to_string(),
            body: String::new(),
            anchors: Vec::new(),
            links: Vec::new(),
        }
    }

    #[test]
    fn missing_directory_is_an_empty_graph() {
        let project = tempfile::tempdir().expect("temporary directory");
        let store = Store::new(project.path());
        let (graph, warnings) = store.load().expect("load succeeds");
        assert!(graph.is_empty());
        assert!(warnings.is_empty());
        assert!(
            store
                .query(&QueryInput::default())
                .expect("query succeeds")
                .starts_with("The knowledge graph is empty.")
        );
        assert!(!store.directory().exists(), "queries must not create files");
    }

    #[test]
    fn updates_round_trip_through_the_filesystem() {
        let project = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(project.path().join("src")).expect("create src");
        fs::write(project.path().join("src/present.rs"), "").expect("write file");
        let store = Store::new(project.path());

        let summary = store
            .update(UpdateInput {
                nodes: vec![
                    NodeInput {
                        body: "Details.".into(),
                        anchors: vec![
                            Anchor {
                                path: "src/present.rs".into(),
                                symbol: None,
                            },
                            Anchor {
                                path: "src/removed.rs".into(),
                                symbol: None,
                            },
                        ],
                        ..node_input("first", "First", "The first node.")
                    },
                    NodeInput {
                        links: vec![LinkInput {
                            relation: Relation::PartOf,
                            to: "first".into(),
                        }],
                        ..node_input("second", "Second", "The second node.")
                    },
                ],
                delete: Vec::new(),
            })
            .expect("update succeeds");
        assert_eq!(summary, "Created: first, second.");

        let mut file_names = fs::read_dir(store.directory())
            .expect("directory exists")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        file_names.sort();
        assert_eq!(file_names, vec!["first.md", "second.md"]);

        let output = store
            .query(&QueryInput {
                ids: vec!["first".into()],
                ..QueryInput::default()
            })
            .expect("query succeeds");
        assert_eq!(
            output,
            "## first [concept] First\n\
             The first node.\n\
             Anchors: src/present.rs, src/removed.rs (missing)\n\
             Linked from: second part_of (Second)\n\
             \n\
             Details."
        );

        let summary = store
            .update(UpdateInput {
                nodes: Vec::new(),
                delete: vec!["first".into()],
            })
            .expect("delete succeeds");
        assert_eq!(
            summary,
            "Deleted: first. Removed links to deleted nodes from: second."
        );
        assert!(!store.directory().join("first.md").exists());
        let second = fs::read_to_string(store.directory().join("second.md")).expect("file exists");
        assert!(!second.contains("links:"), "{second}");
    }

    #[test]
    fn rejected_updates_write_nothing() {
        let project = tempfile::tempdir().expect("temporary directory");
        let store = Store::new(project.path());
        let error = store
            .update(UpdateInput {
                nodes: vec![NodeInput {
                    links: vec![LinkInput {
                        relation: Relation::DependsOn,
                        to: "ghost".into(),
                    }],
                    ..node_input("first", "First", "The first node.")
                }],
                delete: Vec::new(),
            })
            .expect_err("update should be rejected");
        assert!(format!("{error:#}").contains("which does not exist"));
        assert!(!store.directory().exists());
    }

    #[test]
    fn malformed_files_become_warnings_in_query_output() {
        let project = tempfile::tempdir().expect("temporary directory");
        let store = Store::new(project.path());
        store
            .update(UpdateInput {
                nodes: vec![node_input("good", "Good", "A good node.")],
                delete: Vec::new(),
            })
            .expect("update succeeds");
        fs::write(store.directory().join("broken.md"), "not a node").expect("write file");
        fs::write(store.directory().join("README.txt"), "ignored").expect("write file");

        let output = store.query(&QueryInput::default()).expect("query succeeds");
        assert_eq!(
            output,
            "Warnings while loading the knowledge graph:\n\
             - broken.md: expected YAML frontmatter starting with `---`\n\
             \n\
             1 node:\n\
             - good [concept] Good"
        );
    }

    #[cfg(unix)]
    #[test]
    fn knowledge_directory_must_not_escape_the_project() {
        let project = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(project.path().join(".zed")).expect("create .zed");
        std::os::unix::fs::symlink(outside.path(), project.path().join(KNOWLEDGE_DIRECTORY))
            .expect("create symlink");
        let store = Store::new(project.path());

        let error = store
            .update(UpdateInput {
                nodes: vec![node_input("first", "First", "The first node.")],
                delete: Vec::new(),
            })
            .expect_err("update should be rejected");
        assert!(format!("{error:#}").contains("outside the project"));
        assert_eq!(
            fs::read_dir(outside.path())
                .expect("directory exists")
                .count(),
            0
        );
        assert!(store.load().is_err());
    }
}
