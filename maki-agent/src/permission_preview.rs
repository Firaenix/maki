//! What a prompted file write would put on disk, so the user can read it
//! before answering. Scopes only name the file, which is not enough to judge
//! an edit by.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::{EDIT_TOOL_NAME, MULTIEDIT_TOOL_NAME, WRITE_TOOL_NAME};

/// Shared by every text in one preview. The event crosses to every frontend
/// and the prompt keeps it while it waits, so a model that writes a huge file
/// must not make the prompt hold all of it.
pub const PREVIEW_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionPreview {
    Write {
        path: String,
        content: String,
        #[serde(default)]
        append: bool,
        /// The content was cut to fit [`PREVIEW_MAX_BYTES`].
        #[serde(default)]
        truncated: bool,
    },
    /// `edit` is one replacement, `multiedit` several, applied in order.
    Edit {
        path: String,
        edits: Vec<PreviewEdit>,
        /// A text was cut or trailing edits were dropped to fit the budget.
        #[serde(default)]
        truncated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewEdit {
    pub old: String,
    pub new: String,
}

impl PermissionPreview {
    /// Only the builtin file tools have a known input shape. Anything else, or
    /// an input missing a field the tool needs, gets no preview and the prompt
    /// falls back to its scopes.
    pub fn from_tool_input(tool: &str, input: &Value) -> Option<Self> {
        let path = input
            .get("path")
            .or_else(|| input.get("file_path"))?
            .as_str()?
            .to_owned();
        let mut budget = Budget::new(PREVIEW_MAX_BYTES);
        match tool {
            WRITE_TOOL_NAME => {
                let content = budget.take(input.get("content")?.as_str()?);
                Some(Self::Write {
                    path,
                    content,
                    append: input.get("append").and_then(Value::as_bool) == Some(true),
                    truncated: budget.truncated,
                })
            }
            EDIT_TOOL_NAME => {
                let edit = parse_edit(input)?;
                let edits = vec![budget.edit(edit)];
                Some(Self::Edit {
                    path,
                    edits,
                    truncated: budget.truncated,
                })
            }
            MULTIEDIT_TOOL_NAME => {
                let raw = input
                    .get("edits")?
                    .as_array()?
                    .iter()
                    .map(parse_edit)
                    .collect::<Option<Vec<_>>>()?;
                let mut edits = Vec::with_capacity(raw.len());
                for edit in raw {
                    if budget.exhausted() {
                        budget.truncated = true;
                        break;
                    }
                    edits.push(budget.edit(edit));
                }
                Some(Self::Edit {
                    path,
                    edits,
                    truncated: budget.truncated,
                })
            }
            _ => None,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Write { path, .. } | Self::Edit { path, .. } => path,
        }
    }

    pub fn truncated(&self) -> bool {
        match self {
            Self::Write { truncated, .. } | Self::Edit { truncated, .. } => *truncated,
        }
    }
}

fn parse_edit(value: &Value) -> Option<(&str, &str)> {
    Some((
        value.get("old_string")?.as_str()?,
        value.get("new_string")?.as_str()?,
    ))
}

struct Budget {
    left: usize,
    truncated: bool,
}

impl Budget {
    fn new(bytes: usize) -> Self {
        Self {
            left: bytes,
            truncated: false,
        }
    }

    fn exhausted(&self) -> bool {
        self.left == 0
    }

    fn edit(&mut self, (old, new): (&str, &str)) -> PreviewEdit {
        PreviewEdit {
            old: self.take(old),
            new: self.take(new),
        }
    }

    /// Cuts at the last line break that fits, so the preview never ends on
    /// half a line, unless the first line alone is over budget.
    fn take(&mut self, text: &str) -> String {
        if text.len() <= self.left {
            self.left -= text.len();
            return text.to_owned();
        }
        self.truncated = true;
        let end = text.floor_char_boundary(self.left);
        let end = text[..end].rfind('\n').map_or(end, |nl| nl + 1);
        self.left = 0;
        text[..end].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    fn edit(old: &str, new: &str) -> PreviewEdit {
        PreviewEdit {
            old: old.into(),
            new: new.into(),
        }
    }

    #[test]
    fn write_carries_the_content_and_append_flag() {
        let preview = PermissionPreview::from_tool_input(
            WRITE_TOOL_NAME,
            &json!({"file_path": "/p/a.rs", "content": "fn a() {}\n", "append": true}),
        );
        assert_eq!(
            preview,
            Some(PermissionPreview::Write {
                path: "/p/a.rs".into(),
                content: "fn a() {}\n".into(),
                append: true,
                truncated: false,
            })
        );
    }

    #[test]
    fn edit_and_multiedit_carry_every_replacement() {
        let single = PermissionPreview::from_tool_input(
            EDIT_TOOL_NAME,
            &json!({"path": "/p/a.rs", "old_string": "a", "new_string": "b"}),
        );
        assert_eq!(
            single,
            Some(PermissionPreview::Edit {
                path: "/p/a.rs".into(),
                edits: vec![edit("a", "b")],
                truncated: false,
            })
        );
        let multi = PermissionPreview::from_tool_input(
            MULTIEDIT_TOOL_NAME,
            &json!({"path": "/p/a.rs", "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c", "new_string": "d", "replace_all": true},
            ]}),
        );
        assert_eq!(
            multi,
            Some(PermissionPreview::Edit {
                path: "/p/a.rs".into(),
                edits: vec![edit("a", "b"), edit("c", "d")],
                truncated: false,
            })
        );
    }

    #[test_case("bash", json!({"path": "/p", "content": "x"}) ; "unknown_tool")]
    #[test_case(WRITE_TOOL_NAME, json!({"path": "/p"}) ; "write_without_content")]
    #[test_case(EDIT_TOOL_NAME, json!({"old_string": "a", "new_string": "b"}) ; "edit_without_path")]
    #[test_case(MULTIEDIT_TOOL_NAME, json!({"path": "/p", "edits": [{"old_string": "a"}]}) ; "multiedit_with_broken_edit")]
    fn no_preview_without_a_known_shape(tool: &str, input: Value) {
        assert_eq!(PermissionPreview::from_tool_input(tool, &input), None);
    }

    #[test]
    fn huge_content_is_cut_at_a_line_break_and_marked() {
        let line = "x".repeat(99) + "\n";
        let content = line.repeat(PREVIEW_MAX_BYTES / line.len() + 10);
        let Some(PermissionPreview::Write {
            content: kept,
            truncated,
            ..
        }) = PermissionPreview::from_tool_input(
            WRITE_TOOL_NAME,
            &json!({"path": "/p", "content": content}),
        )
        else {
            panic!("expected a write preview");
        };
        assert!(truncated);
        assert!(kept.len() <= PREVIEW_MAX_BYTES);
        assert!(kept.ends_with('\n'));
        assert!(content.starts_with(&kept));
    }

    #[test]
    fn a_multiedit_over_budget_drops_the_edits_that_do_not_fit() {
        let big = "y".repeat(PREVIEW_MAX_BYTES);
        let preview = PermissionPreview::from_tool_input(
            MULTIEDIT_TOOL_NAME,
            &json!({"path": "/p", "edits": [
                {"old_string": big, "new_string": "z"},
                {"old_string": "a", "new_string": "b"},
            ]}),
        )
        .unwrap();
        let PermissionPreview::Edit {
            edits, truncated, ..
        } = &preview
        else {
            panic!("expected an edit preview");
        };
        assert!(truncated);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old.len(), PREVIEW_MAX_BYTES);
        assert_eq!(edits[0].new, "");
        assert_eq!(preview.path(), "/p");
    }

    #[test]
    fn a_multibyte_cut_stays_on_a_char_boundary() {
        let mut budget = Budget::new(5);
        assert_eq!(budget.take("ééé"), "éé");
        assert!(budget.truncated);
    }
}
