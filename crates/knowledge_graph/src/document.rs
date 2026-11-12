use crate::{Anchor, Link, Node, NodeId, NodeKind, normalize_anchor_path};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

const FRONTMATTER_DELIMITER: &str = "---";

#[derive(Serialize, Deserialize)]
struct Frontmatter {
    kind: NodeKind,
    title: String,
    summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    anchors: Vec<Anchor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    links: Vec<Link>,
}

/// Parses a node file: YAML frontmatter between `---` lines, followed by the
/// Markdown body.
///
/// Length limits are deliberately not enforced here. They apply when a node is
/// written through the tools; a file that a person edited by hand should still
/// load even if it runs long.
pub fn parse_node(id: NodeId, content: &str) -> Result<Node> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.split_inclusive('\n');
    let Some(first_line) = lines.next() else {
        bail!("file is empty; expected YAML frontmatter starting with `---`");
    };
    if first_line.trim_end() != FRONTMATTER_DELIMITER {
        bail!("expected YAML frontmatter starting with `---`");
    }

    let frontmatter_start = first_line.len();
    let mut boundaries = None;
    let mut offset = frontmatter_start;
    for line in lines {
        if line.trim_end() == FRONTMATTER_DELIMITER {
            boundaries = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((frontmatter_end, body_start)) = boundaries else {
        bail!("frontmatter is missing its closing `---` line");
    };

    let frontmatter_text = content
        .get(frontmatter_start..frontmatter_end)
        .context("frontmatter boundaries are not valid")?;
    let frontmatter: Frontmatter =
        serde_yaml::from_str(frontmatter_text).context("invalid frontmatter")?;
    let body = content
        .get(body_start..)
        .context("body boundary is not valid")?
        .trim()
        .to_string();

    let mut anchors = Vec::with_capacity(frontmatter.anchors.len());
    for anchor in frontmatter.anchors {
        let path = normalize_anchor_path(&anchor.path)
            .with_context(|| format!("anchor path `{}`", anchor.path))?;
        anchors.push(Anchor {
            path,
            symbol: anchor.symbol,
        });
    }

    Ok(Node {
        id,
        kind: frontmatter.kind,
        title: frontmatter.title.trim().to_string(),
        summary: frontmatter.summary.trim().to_string(),
        body,
        anchors,
        links: frontmatter.links,
    })
}

pub fn serialize_node(node: &Node) -> Result<String> {
    let frontmatter = Frontmatter {
        kind: node.kind,
        title: node.title.clone(),
        summary: node.summary.clone(),
        anchors: node.anchors.clone(),
        links: node.links.clone(),
    };
    let yaml = serde_yaml::to_string(&frontmatter)
        .with_context(|| format!("serializing node `{}`", node.id))?;
    let mut document = format!("{FRONTMATTER_DELIMITER}\n{yaml}");
    if !document.ends_with('\n') {
        document.push('\n');
    }
    document.push_str(FRONTMATTER_DELIMITER);
    document.push('\n');
    if !node.body.is_empty() {
        document.push('\n');
        document.push_str(&node.body);
        document.push('\n');
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Relation;
    use pretty_assertions::assert_eq;

    fn id(id: &str) -> NodeId {
        NodeId::new(id).expect("valid id")
    }

    fn full_node() -> Node {
        Node {
            id: id("session-store"),
            kind: NodeKind::Component,
            title: "Session store: \"owns\" sessions #1".into(),
            summary: "Owns login sessions, and their expiry - see `SessionStore`.".into(),
            body: "Sessions are stored in Redis.\n\n---\n\nKeys expire after a day.".into(),
            anchors: vec![
                Anchor {
                    path: "src/auth/session.rs".into(),
                    symbol: Some("SessionStore".into()),
                },
                Anchor {
                    path: "src/auth".into(),
                    symbol: None,
                },
            ],
            links: vec![Link {
                relation: Relation::DependsOn,
                to: id("auth-flow"),
            }],
        }
    }

    #[test]
    fn serialized_nodes_parse_back_unchanged() {
        let node = full_node();
        let document = serialize_node(&node).expect("node serializes");
        assert_eq!(
            parse_node(node.id.clone(), &document).expect("document parses"),
            node
        );

        let minimal = Node {
            id: id("minimal"),
            kind: NodeKind::Decision,
            title: "Minimal".into(),
            summary: "Nothing but the required fields.".into(),
            body: String::new(),
            anchors: Vec::new(),
            links: Vec::new(),
        };
        let document = serialize_node(&minimal).expect("node serializes");
        assert_eq!(
            document,
            "---\nkind: decision\ntitle: Minimal\nsummary: Nothing but the required fields.\n---\n"
        );
        assert_eq!(
            parse_node(minimal.id.clone(), &document).expect("document parses"),
            minimal
        );
    }

    #[test]
    fn serialized_layout_is_stable() {
        assert_eq!(
            serialize_node(&Node {
                title: "Session store".into(),
                summary: "Owns login sessions.".into(),
                body: "Body text.".into(),
                ..full_node()
            })
            .expect("node serializes"),
            "---\n\
             kind: component\n\
             title: Session store\n\
             summary: Owns login sessions.\n\
             anchors:\n\
             - path: src/auth/session.rs\n\
             \x20 symbol: SessionStore\n\
             - path: src/auth\n\
             links:\n\
             - relation: depends_on\n\
             \x20 to: auth-flow\n\
             ---\n\
             \n\
             Body text.\n"
        );
    }

    #[test]
    fn parses_hand_written_files() {
        let document = "\u{feff}---\r\nkind: pitfall\r\ntitle:   Padded title  \r\nsummary: A summary.\r\nanchors:\r\n  - path: ./src//lib.rs\r\n---\r\n\r\nFirst line.\r\nSecond line.\r\n\r\n";
        let node = parse_node(id("hand-written"), document).expect("document parses");
        assert_eq!(node.kind, NodeKind::Pitfall);
        assert_eq!(node.title, "Padded title");
        assert_eq!(node.anchors[0].path, "src/lib.rs");
        assert_eq!(node.body, "First line.\r\nSecond line.");
    }

    #[test]
    fn rejects_malformed_files() {
        let cases = [
            ("", "file is empty"),
            ("# Just markdown\n", "expected YAML frontmatter"),
            (
                "---\nkind: concept\ntitle: T\nsummary: S\n",
                "missing its closing",
            ),
            (
                "---\nkind: opinion\ntitle: T\nsummary: S\n---\n",
                "invalid frontmatter",
            ),
            (
                "---\nkind: concept\nsummary: S\n---\n",
                "invalid frontmatter",
            ),
            (
                "---\nkind: concept\ntitle: T\nsummary: S\nlinks:\n- relation: related_to\n  to: Not A Slug\n---\n",
                "invalid frontmatter",
            ),
            (
                "---\nkind: concept\ntitle: T\nsummary: S\nanchors:\n- path: ../escape\n---\n",
                "anchor path `../escape`",
            ),
        ];
        for (document, expected_error) in cases {
            let error = parse_node(id("node"), document).expect_err("document should be rejected");
            let message = format!("{error:#}");
            assert!(
                message.contains(expected_error),
                "expected `{expected_error}` in `{message}`"
            );
        }
    }
}
