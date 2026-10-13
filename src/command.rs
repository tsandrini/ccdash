//! Shell commands as Claude runs them, reduced to what they do: a key that
//! groups runs of the same program (`cargo test`, `git push`) and the
//! prefix a Claude Code allow rule has to match
//! (code.claude.com/docs/en/permissions#bash).

/// Programs whose first plain argument says what they do.
const SUBCOMMANDS: &[&str] = &[
    "agenix",
    "apt",
    "brew",
    "bun",
    "cargo",
    "ccdash",
    "claude",
    "deno",
    "direnv",
    "docker",
    "dotnet",
    "flatpak",
    "gh",
    "git",
    "go",
    "helm",
    "home-manager",
    "just",
    "kubectl",
    "make",
    "nh",
    "nix",
    "nixos-rebuild",
    "npm",
    "npx",
    "pip",
    "pip3",
    "pnpm",
    "podman",
    "poetry",
    "rustup",
    "systemctl",
    "terraform",
    "tmux",
    "tofu",
    "uv",
    "yarn",
];

/// Subcommands that take a second level (`nix flake check`, `gh pr view`).
const NESTED: &[(&str, &[&str])] = &[
    ("nix", &["flake", "profile", "registry", "store"]),
    (
        "gh",
        &["api", "issue", "pr", "release", "repo", "run", "workflow"],
    ),
    ("docker", &["compose"]),
    ("podman", &["compose"]),
];

/// Options that take the next word as their value.
const OPTION_ARGS: &[&str] = &[
    "-C",
    "-c",
    "-L",
    "-S",
    "--git-dir",
    "--work-tree",
    "--option",
];

/// Leading words that only set up the next segment of a chain.
const SETUP: &[&str] = &[
    "cd", "pushd", "export", "set", "source", ".", "unset", "[", "[[", "test", "true", ":",
];

/// Shell keywords in front of a command (`if grep …`, `do echo …`).
const KEYWORDS: &[&str] = &[
    "if", "elif", "then", "else", "do", "while", "until", "!", "{",
];

/// Segments that are shell structure, not commands.
const STRUCTURE: &[&str] = &[
    "for", "case", "select", "function", "done", "fi", "esac", "}", "break", "continue", "exit",
    "return",
];

/// How Claude ran a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    /// Program and subcommand, without wrappers: `cargo test`.
    pub key: String,
    /// The text an allow rule must start with, as written: Claude Code only
    /// looks through its own wrappers, so `direnv exec . cargo test` stays.
    pub prefix: String,
}

impl Command {
    /// The allow rule covering this command and its arguments.
    pub fn rule(&self) -> String {
        format!("Bash({} *)", self.prefix)
    }
}

/// How telling a segment is: a command beats a `$(…)` in an assignment,
/// which beats setup like `cd`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Command,
    Substitution,
    Setup,
}

/// The first segment of `command` (its first line) that does something.
pub fn parse(command: &str) -> Command {
    let line = command.lines().next().unwrap_or_default();
    let words = tokens(line);
    let mut best: Option<(Rank, Command)> = None;
    for segment in words.split(Option::is_none) {
        let segment: Vec<(&str, &str)> = segment
            .iter()
            .flatten()
            .map(|w| (w.0.as_str(), w.1.as_str()))
            .collect();
        let Some((rank, parsed)) = segment_command(&segment) else {
            continue;
        };
        if rank == Rank::Command {
            return parsed;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, parsed));
        }
    }
    best.map(|(_, c)| c).unwrap_or_default()
}

/// `segment` is `(raw, text)` per word, quotes kept in `raw`.
fn segment_command(segment: &[(&str, &str)]) -> Option<(Rank, Command)> {
    let mut segment = segment.to_vec();
    while let Some(first) = segment.first_mut() {
        if KEYWORDS.contains(&first.1) {
            segment.remove(0);
        } else if first.1.starts_with('(') {
            *first = (
                first.0.trim_start_matches('('),
                first.1.trim_start_matches('('),
            );
            if first.1.is_empty() {
                segment.remove(0);
            }
        } else {
            break;
        }
    }
    let first = segment.first()?;
    if STRUCTURE.contains(&first.1) {
        return None;
    }
    let text: Vec<&str> = segment.iter().map(|w| w.1).collect();
    let claude_start = skip_claude_wrappers(&text, 0);
    let start = skip_setup(&text, claude_start);
    let Some(program_word) = text.get(start) else {
        // NOTE: `X=$(cmd …)` alone runs `cmd`
        let inner = segment.iter().find_map(|w| substitution(w.0))?;
        let parsed = parse(inner);
        return (!parsed.key.is_empty()).then_some((Rank::Substitution, parsed));
    };
    if program_word.ends_with('…') {
        return None;
    }
    let program = program_word.rsplit('/').next().unwrap_or(program_word);
    let mut key = vec![program.to_owned()];
    let mut end = start;
    if SUBCOMMANDS.contains(&program)
        && let Some(sub) = subcommand(&text, start + 1)
    {
        key.push(text[sub].to_owned());
        end = sub;
        let nested = NESTED
            .iter()
            .find(|(p, _)| *p == program)
            .is_some_and(|(_, subs)| subs.contains(&text[sub]));
        if nested && let Some(next) = subcommand(&text, sub + 1) {
            key.push(text[next].to_owned());
            end = next;
        }
    }
    let prefix = segment[claude_start..=end]
        .iter()
        .map(|w| w.0)
        .collect::<Vec<_>>()
        .join(" ");
    let rank = if SETUP.contains(&program) {
        Rank::Setup
    } else {
        Rank::Command
    };
    Some((
        rank,
        Command {
            key: key.join(" "),
            prefix,
        },
    ))
}

/// The command inside the first `$(…)` of `word`; not `$((…))` arithmetic.
fn substitution(word: &str) -> Option<&str> {
    let open = word.find("$(")? + 2;
    if word[open..].starts_with('(') {
        return None;
    }
    let mut depth = 1;
    for (i, c) in word[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&word[open..open + i]);
                }
            }
            _ => {}
        }
    }
    Some(&word[open..])
}

/// Past the wrappers Claude Code strips before matching rules.
fn skip_claude_wrappers(words: &[&str], mut i: usize) -> usize {
    while let Some(&word) = words.get(i) {
        i = match word {
            "time" | "nohup" | "noglob" => i + 1,
            // NOTE: `command -v x` looks a program up instead of running it
            "command" | "builtin" if words.get(i + 1).is_some_and(|w| !w.starts_with('-')) => i + 1,
            "nice" | "stdbuf" => skip_options(words, i + 1),
            "timeout" => skip_options(words, i + 1) + 1,
            "xargs" if words.get(i + 1).is_some_and(|w| !w.starts_with('-')) => i + 1,
            _ => return i,
        };
    }
    i
}

/// Past environment setup and runners that execute their arguments.
fn skip_setup(words: &[&str], mut i: usize) -> usize {
    while let Some(&word) = words.get(i) {
        let next = match word {
            _ if is_assignment(word) => i + 1,
            "," | "exec" | "setsid" => i + 1,
            "sudo" | "env" | "ionice" => skip_options(words, i + 1),
            "direnv" if words.get(i + 1) == Some(&"exec") => i + 3,
            "nix" if matches!(words.get(i + 1), Some(&("develop" | "shell"))) => {
                match words[i..]
                    .iter()
                    .position(|w| matches!(*w, "-c" | "--command"))
                {
                    Some(at) => i + at + 1,
                    None => return i,
                }
            }
            _ => skip_claude_wrappers(words, i),
        };
        if next == i {
            return i;
        }
        i = next;
    }
    i
}

/// Past `-x`-style options (and assignments for `env`); a lone `-n 10` keeps
/// its value with it.
fn skip_options(words: &[&str], mut i: usize) -> usize {
    while let Some(&word) = words.get(i) {
        if is_assignment(word) {
            i += 1;
        } else if word.starts_with('-') {
            let takes_value = matches!(word, "-n" | "-s" | "-k" | "-u" | "-c" | "-g")
                && words.get(i + 1).is_some();
            i += if takes_value { 2 } else { 1 };
        } else {
            break;
        }
    }
    i
}

/// The first plain word from `i`, past options.
fn subcommand(words: &[&str], mut i: usize) -> Option<usize> {
    while let Some(&word) = words.get(i) {
        if OPTION_ARGS.contains(&word) {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            let plain = word.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'));
            return plain.then_some(i);
        }
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Words as `(raw, unquoted)`; `None` marks `&&`, `||`, `;`, `|`, `&`.
fn tokens(line: &str) -> Vec<Option<(String, String)>> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    let (mut raw, mut text) = (String::new(), String::new());
    let mut quote: Option<char> = None;
    // NOTE: `$(…)`, `<(…)` and backticks stay inside the word they start in
    let mut depth = 0u32;
    let mut backtick = false;
    let flush = |raw: &mut String, text: &mut String, out: &mut Vec<_>| {
        if !raw.is_empty() {
            out.push(Some((std::mem::take(raw), std::mem::take(text))));
        }
    };
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            raw.push(c);
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    raw.push(next);
                    text.push(next);
                }
            } else {
                text.push(c);
            }
            continue;
        }
        if c == '(' && (depth > 0 || raw.ends_with(['$', '<', '>'])) {
            depth += 1;
        } else if c == ')' && depth > 0 {
            depth -= 1;
        } else if c == '`' {
            backtick = !backtick;
        }
        if (depth > 0 || backtick || c == ')' || c == '`') && !matches!(c, '\'' | '"') {
            raw.push(c);
            text.push(c);
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                raw.push(c);
            }
            '\\' => {
                raw.push(c);
                if let Some(next) = chars.next() {
                    raw.push(next);
                    text.push(next);
                }
            }
            '#' if raw.is_empty() => break,
            c if c.is_whitespace() => flush(&mut raw, &mut text, &mut out),
            // NOTE: `2>&1` and `&>file` are redirections, not separators
            '&' if raw.ends_with(['>', '<']) || chars.peek() == Some(&'>') => {
                raw.push(c);
                text.push(c);
            }
            ';' | '&' | '|' => {
                flush(&mut raw, &mut text, &mut out);
                if matches!(chars.peek(), Some('&' | '|')) {
                    chars.next();
                }
                out.push(None);
            }
            _ => {
                raw.push(c);
                text.push(c);
            }
        }
    }
    flush(&mut raw, &mut text, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(command: &str) -> String {
        parse(command).key
    }

    #[test]
    fn keys_name_program_and_subcommand() {
        assert_eq!(key("cargo test -q --all-targets"), "cargo test");
        assert_eq!(key("git -C ../repo status --short"), "git status");
        assert_eq!(key("ls -la /tmp"), "ls");
        assert_eq!(
            key("/tmp/ccdash-target/debug/ccdash daemon --config x"),
            "ccdash daemon"
        );
        assert_eq!(
            key("nix flake check --no-write-lock-file"),
            "nix flake check"
        );
        assert_eq!(key("gh pr view 12"), "gh pr view");
        assert_eq!(key("nix build .#ccdash"), "nix build");
        assert_eq!(key("python3 -c 'print(1)'"), "python3");
        assert_eq!(key(""), "");
    }

    #[test]
    fn keys_look_through_wrappers_and_setup() {
        assert_eq!(
            key("CARGO_TARGET_DIR=/tmp/t direnv exec . cargo clippy -q"),
            "cargo clippy"
        );
        assert_eq!(
            key("cd ccdash && direnv exec . cargo test 2>&1 | tail -5"),
            "cargo test"
        );
        assert_eq!(key("timeout 60 nix run nixpkgs#dig -- +short x"), "nix run");
        assert_eq!(
            key("sudo -u root systemctl --user restart ccdash"),
            "systemctl restart"
        );
        assert_eq!(key("nix develop -c cargo build"), "cargo build");
        assert_eq!(key(", jq . file.json"), "jq");
        assert_eq!(key("cd /tmp"), "cd");
        assert_eq!(key("command -v tmux"), "command");
    }

    #[test]
    fn rules_keep_what_claude_code_matches_literally() {
        let c = parse("cd x && direnv exec . cargo test -q");
        assert_eq!(c.rule(), "Bash(direnv exec . cargo test *)");
        assert_eq!(parse("timeout 30 npm test").rule(), "Bash(npm test *)");
        assert_eq!(
            parse("git -C ../repo push origin main").rule(),
            "Bash(git -C ../repo push *)"
        );
        assert_eq!(parse("ls -la").rule(), "Bash(ls *)");
    }

    #[test]
    fn keys_look_into_substitutions_loops_and_conditions() {
        assert_eq!(
            key(r#"TOK=$(curl -s -X POST "http://x/login"); echo $TOK"#),
            "echo"
        );
        assert_eq!(key("cd /tmp && F=$(ls ~/.claude/x.jsonl)"), "ls");
        assert_eq!(key("for p in $(pgrep -x a); do echo $p; done"), "echo");
        assert_eq!(
            key("until timeout 30 ssh host true; do sleep 5; done"),
            "ssh"
        );
        assert_eq!(key("if grep -q x f; then echo y; fi"), "grep");
        assert_eq!(key("(timeout 300 python3 -m http.server)"), "python3");
        assert_eq!(
            key("end=$((SECONDS+180)); until [ 1 ]; do a=$(ssh h uptime); done"),
            "ssh"
        );
        assert_eq!(key("tmux -L ccchan send-keys -t t Enter"), "tmux send-keys");
        assert_eq!(key("b=/nix/store/x"), "");
    }

    #[test]
    fn quotes_and_redirections_stay_inside_words() {
        assert_eq!(
            tokens(r#"echo "a && b" 2>&1; ls"#),
            [
                Some(("echo".into(), "echo".into())),
                Some((r#""a && b""#.into(), "a && b".into())),
                Some(("2>&1".into(), "2>&1".into())),
                None,
                Some(("ls".into(), "ls".into())),
            ]
        );
    }
}
