//! Skills on disk, found the way the bundled `skill` plugin finds them, for a
//! host that offers each one as a command. The plugin reads the process cwd;
//! an ACP server runs sessions in other folders, so it asks per session.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::command::{parse_frontmatter, project_ancestor_dirs};

const SKILL_FILE: &str = "SKILL.md";
const PROJECT_SKILL_DIRS: &[&str] = &[
    ".maki/skills",
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
];
const GLOBAL_SKILL_DIRS: &[&str] = &[
    ".claude/skills",
    ".config/opencode/skills",
    ".agents/skills",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` the skill was read from.
    pub path: PathBuf,
}

/// The skills a session in {cwd} can use, by name. Same folders and order as
/// the `skill` plugin: maki's config, the personal folders, then the project
/// up to its Git root. A later folder replaces a skill an earlier one named,
/// so a project skill wins over a personal one.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut dirs = Vec::new();
    if let Ok(config) = maki_storage::paths::config_dir() {
        dirs.push(config.join("skills"));
    }
    if let Some(home) = maki_storage::paths::home() {
        dirs.extend(GLOBAL_SKILL_DIRS.iter().map(|rel| home.join(rel)));
    }
    for ancestor in project_ancestor_dirs(cwd) {
        dirs.extend(PROJECT_SKILL_DIRS.iter().map(|rel| ancestor.join(rel)));
    }
    skills_in(&dirs)
}

fn skills_in(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `is_dir` follows links: skills installed by Nix or stow are links.
            let path = entry.path().join(SKILL_FILE);
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let (frontmatter, body) = parse_frontmatter(&content);
            if body.trim().is_empty() {
                continue;
            }
            let name = frontmatter
                .name
                .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
            let skill = Skill {
                name: name.clone(),
                description: frontmatter.description.unwrap_or_default(),
                path,
            };
            skills.insert(name, skill);
        }
    }
    skills.into_values().collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_skill(root: &Path, dir: &str, content: &str) {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join(SKILL_FILE), content).unwrap();
    }

    #[test]
    fn a_later_folder_replaces_a_skill_of_the_same_name() {
        let personal = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write_skill(
            personal.path(),
            "release",
            "---\ndescription: mine\n---\nShip it.",
        );
        write_skill(
            project.path(),
            "release",
            "---\ndescription: theirs\n---\nShip it here.",
        );
        write_skill(personal.path(), "review", "Look closely.");

        let skills = skills_in(&[personal.path().into(), project.path().into()]);
        let found: Vec<(&str, &str)> = skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect();
        assert_eq!(found, [("release", "theirs"), ("review", "")]);
        assert_eq!(
            skills[0].path,
            project.path().join("release").join(SKILL_FILE)
        );
    }

    #[test]
    fn the_frontmatter_name_wins_and_an_empty_body_is_no_skill() {
        let dir = TempDir::new().unwrap();
        write_skill(
            dir.path(),
            "folder-name",
            "---\nname: real-name\n---\nDo it.",
        );
        write_skill(dir.path(), "empty", "---\nname: empty\n---\n   ");
        fs::write(dir.path().join("loose.md"), "not a skill").unwrap();

        let names: Vec<String> = skills_in(&[dir.path().into()])
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["real-name"]);
    }
}
