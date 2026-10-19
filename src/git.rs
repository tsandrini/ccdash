//! The git branch of a directory, read from `.git/HEAD` without spawning
//! git: the collector reports it with each statusline reading and agents
//! poll.

use std::path::{Path, PathBuf};

/// Characters of a detached `HEAD` shown in place of a branch.
const SHORT_HASH: usize = 8;

/// The branch checked out in the repository holding `dir`: the name for
/// `ref: refs/heads/<name>`, the first 8 characters of the hash for a
/// detached `HEAD`. Walks up from `dir` to the first `.git`, which is a
/// directory, or a file pointing at one (`gitdir: …`, worktrees and
/// submodules). None outside a repository or when `HEAD` is unreadable.
///
/// ```ignore
/// assert_eq!(git::branch(Path::new("/src/ccdash/web")).as_deref(), Some("main"));
/// ```
pub fn branch(dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(dir)?.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(name) = head.strip_prefix("ref:") {
        let name = name.trim();
        let name = name.strip_prefix("refs/heads/").unwrap_or(name);
        return (!name.is_empty()).then(|| name.to_owned());
    }
    (head.len() >= SHORT_HASH && head.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| head[..SHORT_HASH].to_owned())
}

/// The git directory of the repository holding `dir`.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    for at in dir.ancestors() {
        let dot = at.join(".git");
        let Ok(meta) = std::fs::metadata(&dot) else {
            continue;
        };
        if meta.is_dir() {
            return Some(dot);
        }
        let text = std::fs::read_to_string(&dot).ok()?;
        let target = Path::new(text.lines().next()?.strip_prefix("gitdir:")?.trim());
        return Some(if target.is_absolute() {
            target.to_owned()
        } else {
            at.join(target)
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_of_repos_worktrees_and_detached_heads() {
        let root = std::env::temp_dir().join(format!("ccdash-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git/worktrees/feature")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(branch(&repo).as_deref(), Some("main"));
        assert_eq!(branch(&repo.join("src/deep")).as_deref(), Some("main"));

        let tree = root.join("feature");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!(
                "gitdir: {}\n",
                repo.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        std::fs::write(
            repo.join(".git/worktrees/feature/HEAD"),
            "ref: refs/heads/feat/x\n",
        )
        .unwrap();
        assert_eq!(branch(&tree).as_deref(), Some("feat/x"));

        let sub = repo.join("src/sub");
        std::fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../../.git/modules/sub\n").unwrap();
        std::fs::write(
            repo.join(".git/modules/sub/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(branch(&sub).as_deref(), Some("01234567"));

        std::fs::write(repo.join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(branch(&repo), None);
        assert_eq!(branch(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
