use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// `shell.prefer` value that selects the bash shipped with Git for Windows.
pub const GIT_BASH: &str = "git-bash";
/// `shell.prefer` value for the native PowerShell. Any other value means the
/// same thing, but this is the one the settings panel writes, so the file says
/// what the user chose.
pub const POWERSHELL: &str = "powershell";

/// ConPTY answers cursor-shape/position negotiation by writing a DSR query
/// (`ESC[6n`) on the output pipe and waiting for a reply on the input pipe.
/// Nothing else in this process speaks to the pty, so respond here; without
/// this Windows shells never emit any output. Shared by the background-process
/// runner and the web terminal.
pub fn answer_terminal_query(writer: &mut dyn std::io::Write, rows: u16) {
    let reply = format!("\x1b[{rows};1R");
    let _ = writer.write_all(reply.as_bytes());
    let _ = writer.flush();
}

/// The query a terminal is waiting for.
const DSR: &[u8] = b"\x1b[6n";

/// Find a DSR query in `chunk`, remembering the last three bytes of the
/// previous chunk so a query split across two reads is still found. Nothing is
/// copied or removed: callers that forward the raw stream keep every byte.
pub fn saw_terminal_query(carry: &mut [u8; 3], chunk: &[u8]) -> bool {
    // The only window that is not inside `chunk` is the one formed by the
    // carry and the first bytes of this read.
    let head = chunk.len().min(carry.len());
    let mut probe = [0u8; 6];
    probe[..carry.len()].copy_from_slice(carry);
    probe[carry.len()..carry.len() + head].copy_from_slice(&chunk[..head]);
    let found = probe[..carry.len() + head]
        .windows(DSR.len())
        .any(|window| window == DSR)
        || chunk.windows(DSR.len()).any(|window| window == DSR);
    let keep = carry.len();
    let take = chunk.len().min(keep);
    if take > 0 {
        carry.copy_within(take.., 0);
        carry[keep - take..].copy_from_slice(&chunk[chunk.len() - take..]);
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellFamily {
    Bash,
    PowerShell,
}

#[derive(Debug, Clone)]
pub struct ShellSpec {
    pub family: ShellFamily,
    pub program: PathBuf,
    pub display_name: String,
    pub version: String,
    pub os: String,
    pub git_bash: bool,
}

impl ShellSpec {
    /// Registered tool name; unique on every platform.
    pub fn tool_name(&self) -> &'static str {
        match self.family {
            ShellFamily::Bash => "bash",
            ShellFamily::PowerShell => "powershell",
        }
    }

    /// Detect the shell, reusing the previous answer when it cannot have
    /// changed.
    ///
    /// Detection starts the shell once to read its version, which costs seconds
    /// on Windows: antivirus plus module loading make a single spawn take 3 s
    /// here. That is the slowest step of `Agent::new` - and therefore of every
    /// resume - for a string that only changes when the shell binary does.
    pub fn detect(prefer: &str) -> Self {
        let path = crate::config::Config::config_dir().join("shell.json");
        if let Some(spec) = read_cache(&path, prefer) {
            return spec;
        }
        let spec = Self::detect_uncached(prefer);
        write_cache(&path, prefer, &spec);
        spec
    }

    fn detect_uncached(prefer: &str) -> Self {
        if cfg!(windows) {
            if prefer.eq_ignore_ascii_case(GIT_BASH) {
                if let Some(bash) = find_git_bash() {
                    let version = version_of(&bash, &["--version"]);
                    return Self {
                        family: ShellFamily::Bash,
                        program: bash,
                        display_name: "Git Bash".into(),
                        version,
                        os: "windows".into(),
                        git_bash: true,
                    };
                }
            }
            let (program, version) = find_powershell();
            return Self {
                family: ShellFamily::PowerShell,
                program,
                display_name: "PowerShell".into(),
                version,
                os: "windows".into(),
                git_bash: false,
            };
        }
        let program = std::env::var("SHELL")
            .ok()
            .filter(|s| s.contains("bash"))
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("/bin/bash"));
        let version = version_of(&program, &["--version"]);
        Self {
            family: ShellFamily::Bash,
            program,
            display_name: "bash".into(),
            version,
            os: std::env::consts::OS.to_string(),
            git_bash: false,
        }
    }

    pub fn description(&self) -> String {
        match self.family {
            ShellFamily::Bash => "Execute a bash command in the session workspace. A new process \
                 is started for every call; `cd` does not persist. Output is captured through a \
                 pty and returned as the final screen lines, so progress bars collapse to one \
                 line. Set run_in_background=true for long jobs, then use `await` with the \
                 returned pid."
                .to_string(),
            ShellFamily::PowerShell => "Execute a PowerShell command in the session workspace \
                 (native Windows shell; do not use bash syntax). A new process is started for \
                 every call; `cd` does not persist. Output is captured through ConPTY and \
                 returned as final screen lines. Set run_in_background=true for long jobs, then \
                 use `await` with the returned pid."
                .to_string(),
        }
    }

    pub fn args(&self, command: &str) -> Vec<String> {
        match self.family {
            ShellFamily::Bash => vec!["-c".to_string(), command.to_string()],
            ShellFamily::PowerShell => vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!(
                    "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                     $OutputEncoding=[System.Text.Encoding]::UTF8; {command}; \
                     if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}"
                ),
            ],
        }
    }

    /// Detect bash-only syntax before running a doomed command on PowerShell.
    pub fn bashism(&self, command: &str) -> Option<String> {
        if self.family != ShellFamily::PowerShell {
            return None;
        }
        let version = self.version.clone();
        let mut fixes = Vec::new();
        if command.contains("/dev/null") {
            fixes.push("`/dev/null` → `$null`（重定向用 `2>$null`）");
        }
        if command.contains("rm -rf ") {
            fixes.push("`rm -rf <path>` → `Remove-Item -Recurse -Force <path>`");
        }
        if command.contains("<<") && command.contains("EOF") {
            fixes.push("bash heredoc → PowerShell here-string `@\"` … `\"@`");
        }
        if regex_like_export(command) {
            fixes.push("`export NAME=value` → `$env:NAME = \"value\"`");
        }
        if command.contains("&&") && !version_starts_with_7(&version) {
            fixes.push("`a && b` → `a; if ($?) { b }`");
        }
        if command.contains("$(pwd)") || command.contains("`pwd`") {
            fixes.push("`pwd` → `Get-Location` / `$PWD`");
        }
        if command.trim_start().starts_with("chmod ") || command.trim_start().starts_with("source ")
        {
            fixes.push(
                "`source`/`chmod` 不是 PowerShell 命令；直接用 `& .\\script.ps1` 或调整执行策略",
            );
        }
        if fixes.is_empty() {
            return None;
        }
        Some(format!(
            "当前是 {name} {version}，检测到 bash 语法。等价写法：\n- {}\n请改用 PowerShell 语法后重试。",
            fixes.join("\n- "),
            name = self.display_name,
            version = self.version,
        ))
    }
}

fn version_starts_with_7(version: &str) -> bool {
    version
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .is_some_and(|t| t.starts_with('7') || t.starts_with("7."))
}

fn regex_like_export(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|token| token.starts_with("export ") || token == "export")
}

/// Cache row: the spec plus the identity of the binary it was probed from.
#[derive(Serialize, Deserialize)]
struct CachedShell {
    prefer: String,
    os: String,
    family: ShellFamily,
    program: PathBuf,
    display_name: String,
    version: String,
    git_bash: bool,
    /// `(len, mtime)` of the shell binary when it was probed; a new shell
    /// install bumps these and the cache is discarded.
    stamp: Option<(u64, i64)>,
}

impl From<&ShellSpec> for CachedShell {
    fn from(spec: &ShellSpec) -> Self {
        Self {
            prefer: String::new(),
            os: spec.os.clone(),
            family: spec.family,
            program: spec.program.clone(),
            display_name: spec.display_name.clone(),
            version: spec.version.clone(),
            git_bash: spec.git_bash,
            stamp: stamp_of(&spec.program),
        }
    }
}

fn stamp_of(program: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(program).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

fn read_cache(path: &Path, prefer: &str) -> Option<ShellSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cached: CachedShell = serde_json::from_str(&raw).ok()?;
    if cached.prefer != prefer || cached.os != std::env::consts::OS {
        return None;
    }
    if cached.stamp != stamp_of(&cached.program) {
        return None;
    }
    Some(ShellSpec {
        family: cached.family,
        program: cached.program,
        display_name: cached.display_name,
        version: cached.version,
        os: cached.os,
        git_bash: cached.git_bash,
    })
}

fn write_cache(path: &Path, prefer: &str, spec: &ShellSpec) {
    let mut cached = CachedShell::from(spec);
    cached.prefer = prefer.to_string();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string(&cached) {
        let _ = std::fs::write(path, raw);
    }
}

fn version_of(program: &Path, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output();
    let Ok(output) = output else {
        return "unknown".into();
    };
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    let first = text.lines().next().unwrap_or("unknown").trim();
    let first = first.strip_prefix("GNU bash, ").unwrap_or(first);
    first
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_powershell() -> (PathBuf, String) {
    for exe in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = which(exe) {
            let version = version_of(
                &path,
                &[
                    "-NoProfile",
                    "-Command",
                    "$PSVersionTable.PSVersion.ToString()",
                ],
            );
            return (path, version);
        }
    }
    (PathBuf::from("powershell.exe"), "unknown".into())
}

/// Whether the agent can be pointed at Git Bash here: only Windows has a choice
/// of shell, and it needs Git for Windows installed.
pub fn git_bash_available() -> bool {
    cfg!(windows) && find_git_bash().is_some()
}

/// The bash that ships with Git for Windows.
///
/// It is derived from `git.exe` rather than searched for as `bash.exe`: WSL
/// installs its own `bash.exe` on PATH, and commands started there see a
/// different filesystem, so the workspace paths handed to it would not resolve.
fn find_git_bash() -> Option<PathBuf> {
    if let Some(git) = which("git.exe") {
        if let Some(bash) = bash_beside(&git).into_iter().find(|bash| bash.exists()) {
            return Some(bash);
        }
    }
    [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|bash| bash.exists())
}

/// The bashes a Git for Windows installation keeps beside the `git.exe` that was
/// found: `bin/` holds the launcher for the shell Git puts on PATH, and
/// `usr/bin/` holds the real shell inside the MinGW tree of a portable install.
fn bash_beside(git: &Path) -> Vec<PathBuf> {
    let Some(root) = git.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    vec![
        root.join("bin").join("bash.exe"),
        root.join("usr").join("bin").join("bash.exe"),
    ]
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git for Windows keeps `git.exe` in `cmd/` and the bash it ships in
    /// `bin/`, so the shell is derived from the git that was found rather than
    /// from the first `bash.exe` on PATH (which may be WSL's).
    #[test]
    #[cfg(windows)]
    fn git_bash_is_the_bash_shipped_next_to_git() {
        assert_eq!(
            bash_beside(Path::new(r"C:\Program Files\Git\cmd\git.exe")),
            [
                PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
                PathBuf::from(r"C:\Program Files\Git\usr\bin\bash.exe"),
            ]
        );
    }

    #[test]
    fn the_choice_of_shell_exists_only_on_windows() {
        assert_eq!(git_bash_available(), cfg!(windows));
    }

    fn spec(family: ShellFamily) -> ShellSpec {
        ShellSpec {
            family,
            program: PathBuf::from(if family == ShellFamily::Bash {
                "/bin/bash"
            } else {
                "pwsh"
            }),
            display_name: if family == ShellFamily::Bash {
                "bash"
            } else {
                "PowerShell"
            }
            .into(),
            version: if family == ShellFamily::Bash {
                "5.2.21"
            } else {
                "7.4.1"
            }
            .into(),
            os: if family == ShellFamily::Bash {
                "linux"
            } else {
                "windows"
            }
            .into(),
            git_bash: false,
        }
    }

    #[test]
    fn descriptions_do_not_embed_environment_facts() {
        for family in [ShellFamily::Bash, ShellFamily::PowerShell] {
            let description = spec(family).description();
            assert!(!description.contains("<env>"));
            assert!(!description.contains("cwd:"));
            assert!(description.contains("run_in_background=true"));
        }
    }

    #[test]
    fn tool_names_are_platform_unique() {
        assert_eq!(spec(ShellFamily::Bash).tool_name(), "bash");
        assert_eq!(spec(ShellFamily::PowerShell).tool_name(), "powershell");
    }

    #[test]
    fn powershell_bashisms_are_taught_before_running() {
        let s = spec(ShellFamily::PowerShell);
        let msg = s.bashism("rm -rf target && echo done > /dev/null").unwrap();
        assert!(msg.contains("PowerShell 7.4.1"));
        assert!(msg.contains("Remove-Item -Recurse -Force"));
        assert!(msg.contains("$null"));
        // PowerShell 7 supports &&, so it is not flagged.
        assert!(!msg.contains("if ($?)"));
        assert!(s.bashism("Get-ChildItem").is_none());
    }

    #[test]
    fn powershell_5_gets_and_chain_hint() {
        let mut s = spec(ShellFamily::PowerShell);
        s.version = "5.1.22621".into();
        let msg = s.bashism("cargo build && cargo test").unwrap();
        assert!(msg.contains("if ($?)"));
    }

    #[test]
    fn bash_never_blocks_commands() {
        assert!(spec(ShellFamily::Bash)
            .bashism("rm -rf target && echo hi > /dev/null")
            .is_none());
    }

    /// Probing the shell costs seconds on Windows, so the answer is cached;
    /// the cache must expire when the binary behind it changes.
    #[test]
    fn the_shell_cache_is_reused_until_the_binary_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("shell.json");
        let program = dir.path().join("shell.exe");
        std::fs::write(&program, b"first").unwrap();

        let mut detected = spec(ShellFamily::Bash);
        detected.program = program.clone();
        detected.version = "5.2.0".into();
        detected.os = std::env::consts::OS.to_string();
        write_cache(&cache, "auto", &detected);

        let hit = read_cache(&cache, "auto").expect("cache hit");
        assert_eq!(hit.version, "5.2.0");
        assert_eq!(hit.program, program, "the same binary comes back");

        assert!(
            read_cache(&cache, "git-bash").is_none(),
            "a different preference must re-probe"
        );

        std::fs::write(&program, b"a different build entirely").unwrap();
        assert!(
            read_cache(&cache, "auto").is_none(),
            "a replaced shell must re-probe"
        );
    }
}
