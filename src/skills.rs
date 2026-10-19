//! Skills: folders of instructions for one kind of task, each with a SKILL.md whose frontmatter gives its name, kept in
//! the repository and in the home directory as other coding agents keep them. None is in the system prompt, where every
//! request would carry them all: a task that asks for one by name gets its SKILL.md, and the rest cost nothing.
use crate::worker::{SKILLS, home, secret, skill_reads};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The most skills looked through.
const MOST: usize = 50;
/// The part of a SKILL.md read for its frontmatter.
const HEAD: u64 = 16 * 1024;

pub struct Skill {
    pub name: String,
    /// The SKILL.md as show() takes it: from the repo root for the repository's, whole for the home directory's.
    pub path: String,
    /// The SKILL.md resolved: one the worker can read.
    pub real: PathBuf,
}

/// The skills a task asks for: each named next to the word skill, as in "use the gsearch skill", "skill gsearch" or "the
/// gsearch and gnews skills". A name alone is not asking: a skill named low or review would come with every task that
/// says the word.
pub fn named(task: &str, root: &Path) -> Vec<Skill> {
    let task = task.to_lowercase();
    if !task.contains("skill") {
        return vec![];
    }
    let words: Vec<&str> =
        task.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')).filter(|w| !w.is_empty()).collect();
    let skills = find(root);
    let names: Vec<String> = skills.iter().map(|s| s.name.to_lowercase()).collect();
    let asked: Vec<bool> = names.iter().map(|name| asked(&words, name, &names)).collect();
    skills.into_iter().zip(asked).filter_map(|(skill, asked)| asked.then_some(skill)).collect()
}

/// Whether `name` is in `words` next to the word skill, or in a list of skill names that is, joined by and or or (the
/// commas went with the other punctuation).
fn asked(words: &[&str], name: &str, names: &[String]) -> bool {
    let listed = |w: &&str| matches!(*w, "and" | "or") || names.iter().any(|n| n.as_str() == *w);
    let skill = |w: Option<&str>| matches!(w, Some("skill" | "skills"));
    words.iter().enumerate().filter(|(_, w)| **w == name).any(|(at, _)| {
        skill(words[at + 1..].iter().copied().find(|w| !listed(w)))
            || skill(words[..at].iter().rev().copied().find(|w| !listed(w)))
    })
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
            let name = frontmatter(&head, "name").filter(|n| !n.is_empty()).unwrap_or(folder_name);
            if found.iter().any(|s| s.name == name) || found.len() >= MOST {
                continue;
            }
            let path = if ours { file.strip_prefix(root).unwrap_or(&file) } else { &file };
            found.push(Skill { name, path: path.to_string_lossy().into_owned(), real });
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

/// A field of a SKILL.md's YAML frontmatter: plain, quoted, or a folded (>) or literal (|) block.
fn frontmatter(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return None;
    }
    let lines: Vec<&str> = lines.take_while(|l| l.trim_end() != "---").collect();
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
}

#[cfg(test)]
mod tests {
    #[test]
    fn frontmatter() {
        let field = |text: &str, key: &str| super::frontmatter(text, key);
        let folded = "---\nname: video\ndescription: >\n  Make a video,\n  or edit one.\n\nmetadata: x\n---\nbody";
        assert_eq!(
            (field(folded, "name"), field(folded, "description")),
            (Some("video".into()), Some("Make a video, or edit one.".into()))
        );
        let quoted = "---\nname: 'it''s'\ndescription: \"Says \\\"hi\\\"\"\n---\n";
        assert_eq!(
            (field(quoted, "name"), field(quoted, "description")),
            (Some("it's".into()), Some("Says \"hi\"".into()))
        );
        assert_eq!(field("# no frontmatter\nname: x", "name"), None);
    }

    #[test]
    fn asked_for() {
        let names: Vec<String> = ["gsearch", "gnews", "low", "find-skills"].map(String::from).into();
        let asked = |task: &str| -> Vec<&str> {
            let task = task.to_lowercase();
            let words: Vec<&str> = task
                .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                .filter(|w| !w.is_empty())
                .collect();
            names.iter().filter(|n| super::asked(&words, n, &names)).map(String::as_str).collect()
        };
        assert_eq!(asked("Use the gsearch skill to find it"), ["gsearch"]);
        assert_eq!(asked("skill: GNews, then summarize"), ["gnews"]);
        assert_eq!(asked("with the gsearch, gnews and low skills"), ["gsearch", "gnews", "low"]);
        assert_eq!(asked("skills gnews or gsearch"), ["gsearch", "gnews"]);
        assert_eq!(asked("use find-skills skill"), ["find-skills"]);
        // A name that isn't next to the word skill: low's own name, in a task about skills.
        assert!(asked("load the skills only when I ask, I want really low entropy").is_empty());
        assert!(asked("gsearch for it, then pick a skill").is_empty());
        assert!(asked("search with gsearch").is_empty());
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
        let found: Vec<(String, String)> = super::find(&root).into_iter().map(|s| (s.name, s.path)).collect();
        let ours: Vec<_> = found.iter().filter(|s| !s.1.starts_with('/')).cloned().collect();
        assert_eq!(
            ours,
            [
                ("b".into(), ".agents/skills/b-skill/SKILL.md".into()),
                ("a-skill".into(), ".claude/skills/a-skill/SKILL.md".into())
            ]
        );
        let named = super::named("Use the b skill", &root);
        assert_eq!(
            named.iter().map(|s| (s.name.as_str(), s.real.clone())).collect::<Vec<_>>(),
            [("b", root.join(".agents/skills/b-skill/SKILL.md"))]
        );
        assert!(super::named("b", &root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
