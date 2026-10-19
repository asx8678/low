//! Skills: folders of instructions for one kind of task, each with a SKILL.md whose frontmatter gives its name and what
//! it is for, kept in the repository and in the home directory as other coding agents keep them. The system prompt
//! lists them; the model reads the SKILL.md of one that fits the task with show(), as it reads any file.
use crate::worker::{SKILLS, home, secret, skill_reads};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The most skills the prompt lists, and the most it says of one's description.
const MOST: usize = 50;
const DESCRIPTION: usize = 200;
/// The part of a SKILL.md read for its frontmatter.
const HEAD: u64 = 16 * 1024;

pub struct Skill {
    pub name: String,
    pub description: String,
    /// The SKILL.md as show() takes it: from the repo root for the repository's, whole for the home directory's.
    pub path: String,
}

/// The system prompt's section on skills, or nothing when there are none.
pub fn section(root: &Path) -> String {
    let skills = find(root);
    if skills.is_empty() {
        return String::new();
    }
    let mut text =
        "\n\n# Skills\n\nEach skill is a folder of instructions for one kind of task. When the task is one a \
                    skill below is for, show() its SKILL.md before you start and follow it; the files it names are in \
                    its folder.\n"
            .to_string();
    for skill in &skills {
        text += &format!("- {} ({}): {}\n", skill.name, skill.path, skill.description);
    }
    text.truncate(text.trim_end().len());
    text
}

/// The skills in the repository's skills folders, then in $HOME's, by folder name; on a name both have, the
/// repository's. Only one the worker can read: in the repository and not hidden, or in what the sandbox lets it read
/// of the home directory.
pub fn find(root: &Path) -> Vec<Skill> {
    let home = home();
    let places = SKILLS.iter().map(|s| (root.join(s), true));
    let places = places.chain(SKILLS.iter().filter(|_| !home.as_os_str().is_empty()).map(|s| (home.join(s), false)));
    let mut found: Vec<Skill> = vec![];
    for (dir, ours) in places {
        let mut entries: Vec<PathBuf> =
            std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort();
        for folder in entries {
            let file = folder.join("SKILL.md");
            let Some(real) = file.canonicalize().ok().filter(|real| readable(root, real, ours)) else { continue };
            let Some(head) = head(&real) else { continue };
            let folder_name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (name, description) = frontmatter(&head);
            let name = name.filter(|n| !n.is_empty()).unwrap_or(folder_name);
            if found.iter().any(|s| s.name == name) || found.len() >= MOST {
                continue;
            }
            let path = if ours { file.strip_prefix(root).unwrap_or(&file) } else { &file };
            let description = clip(&description.unwrap_or_default());
            found.push(Skill { name, description, path: path.to_string_lossy().into_owned() });
        }
    }
    found
}

fn readable(root: &Path, real: &Path, ours: bool) -> bool {
    if ours { real.starts_with(root) && !secret(real) } else { skill_reads().iter().any(|dir| real.starts_with(dir)) }
}

/// The start of a regular file, as text; not through a link at the file itself, since `real` is resolved.
fn head(real: &Path) -> Option<String> {
    let file =
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(real).ok()?;
    file.metadata().ok().filter(std::fs::Metadata::is_file)?;
    let mut bytes = Vec::new();
    file.take(HEAD).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The name and description a SKILL.md's YAML frontmatter gives: plain, quoted, or a folded (>) or literal (|) block.
fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let lines: Vec<&str> = lines.take_while(|l| l.trim_end() != "---").collect();
    let field = |key: &str| -> Option<String> {
        let at = lines.iter().position(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':')))?;
        let value = lines[at][key.len() + 1..].trim();
        if value.starts_with(['>', '|']) {
            let block = lines[at + 1..].iter().take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']));
            let words: Vec<&str> = block.map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
            return Some(words.join(" "));
        }
        let quoted = value.len() >= 2
            && (value.starts_with('"') && value.ends_with('"') || value.starts_with('\'') && value.ends_with('\''));
        Some(if quoted { value[1..value.len() - 1].replace("\\\"", "\"").replace("''", "'") } else { value.into() })
    };
    (field("name"), field("description"))
}

/// A description on one line, at most DESCRIPTION characters, cut at a word.
fn clip(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= DESCRIPTION {
        return text;
    }
    let cut: String = text.chars().take(DESCRIPTION).collect();
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(kept, _)| kept))
}

#[cfg(test)]
mod tests {
    #[test]
    fn frontmatter() {
        let folded = "---\nname: video\ndescription: >\n  Make a video,\n  or edit one.\n\nmetadata: x\n---\nbody";
        assert_eq!(super::frontmatter(folded), (Some("video".into()), Some("Make a video, or edit one.".into())));
        let quoted = "---\nname: 'it''s'\ndescription: \"Says \\\"hi\\\"\"\n---\n";
        assert_eq!(super::frontmatter(quoted), (Some("it's".into()), Some("Says \"hi\"".into())));
        assert_eq!(super::frontmatter("# no frontmatter\nname: x"), (None, None));
        let long = "word ".repeat(100);
        let clipped = super::clip(&long);
        assert!(clipped.ends_with("word…") && clipped.chars().count() <= super::DESCRIPTION + 1, "{clipped}");
    }

    /// The repository's skills, then $HOME's, by folder name: one without a SKILL.md, or linked out of the repository,
    /// isn't listed.
    #[test]
    fn repository_skills() {
        let root = std::env::temp_dir().join(format!("low-skills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in [".agents/skills/b-skill", ".claude/skills/a-skill", ".claude/skills/empty"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(".agents/skills/b-skill/SKILL.md"), "---\nname: b\ndescription: Does b.\n---\n")
            .unwrap();
        std::fs::write(root.join(".claude/skills/a-skill/SKILL.md"), "---\ndescription: Does a.\n---\n").unwrap();
        let outside = std::env::temp_dir().join(format!("low-skills-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("SKILL.md"), "---\nname: out\n---\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".claude/skills/linked")).unwrap();
        let root = root.canonicalize().unwrap();
        let found: Vec<(String, String, String)> =
            super::find(&root).into_iter().map(|s| (s.name, s.path, s.description)).collect();
        let ours: Vec<_> = found.iter().filter(|s| !s.1.starts_with('/')).cloned().collect();
        assert_eq!(
            ours,
            [
                ("b".into(), ".agents/skills/b-skill/SKILL.md".into(), "Does b.".into()),
                ("a-skill".into(), ".claude/skills/a-skill/SKILL.md".into(), "Does a.".into()),
            ]
        );
        let section = super::section(&root);
        assert!(section.contains("\n- b (.agents/skills/b-skill/SKILL.md): Does b.\n"), "{section}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
