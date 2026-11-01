use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    Bash,
    PowerShell,
}

#[derive(Debug, Clone)]
pub struct ShellSpec {
    pub family: ShellFamily,
    /// Registered tool name; unique on every platform.
    pub tool_name: &'static str,
    pub program: PathBuf,
    pub display_name: String,
    pub version: String,
    pub os: String,
    pub git_bash: bool,
}

impl ShellSpec {
    pub fn detect(prefer: &str) -> Self {
        if cfg!(windows) {
            if prefer.eq_ignore_ascii_case("git-bash") {
                if let Some(bash) = find_git_bash() {
                    let version = version_of(&bash, &["--version"]);
                    return Self {
                        family: ShellFamily::Bash,
                        tool_name: "bash",
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
                tool_name: "powershell",
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
            tool_name: "bash",
            program,
            display_name: "bash".into(),
            version,
            os: std::env::consts::OS.to_string(),
            git_bash: false,
        }
    }

    /// The `<env>` block injected into the system prompt and reused verbatim in
    /// the tool description (single source of truth).
    pub fn env_block(&self, cwd: &Path) -> String {
        let cwd = cwd.display();
        match self.family {
            ShellFamily::Bash => format!(
                "<env>\nOS: {os}\nShell: {name} {ver}\ncwd: {cwd}\n\
                 syntax: variables=$NAME, separator `;` and `&&`, quotes '...' \"...\", \
                 paths `/a/b`, null `/dev/null`, heredoc `<<'EOF'`\n\
                 note: every call starts a new process at cwd; `cd` does not persist.\n</env>",
                os = self.os,
                name = self.display_name,
                ver = self.version,
            ),
            ShellFamily::PowerShell => format!(
                "<env>\nOS: {os}\nShell: {name} {ver}\ncwd: {cwd}\n\
                 syntax: variables=$env:NAME, separator `;`, chain `cmd1; if ($?) {{ cmd2 }}`, \
                 quotes '...' \"...\", paths `C:\\a\\b`, null `$null`, here-string `@\"` ... `\"@`\n\
                 note: every call starts a new process at cwd; `cd` does not persist.\n</env>",
                os = self.os,
                name = self.display_name,
                ver = self.version,
            ),
        }
    }

    pub fn description(&self, cwd: &Path) -> String {
        match self.family {
            ShellFamily::Bash => format!(
                "Execute a bash command in the session workspace. A new process is started for \
                 every call; `cd` does not persist. Output is captured through a pty and returned \
                 as the final screen lines, so progress bars collapse to one line. Set \
                 run_in_background=true for long jobs, then use `await` with the returned pid.\n\n{}",
                self.env_block(cwd)
            ),
            ShellFamily::PowerShell => format!(
                "Execute a PowerShell command in the session workspace (native Windows shell; \
                 do not use bash syntax). A new process is started for every call; `cd` does not \
                 persist. Output is captured through ConPTY and returned as final screen lines. \
                 Set run_in_background=true for long jobs, then use `await` with the returned pid.\n\n{}",
                self.env_block(cwd)
            ),
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

fn find_git_bash() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    which("bash.exe")
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

    fn spec(family: ShellFamily) -> ShellSpec {
        ShellSpec {
            family,
            tool_name: if family == ShellFamily::Bash {
                "bash"
            } else {
                "powershell"
            },
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
    fn env_block_le_8_lines() {
        for family in [ShellFamily::Bash, ShellFamily::PowerShell] {
            let block = spec(family).env_block(Path::new("/tmp/work"));
            assert!(block.starts_with("<env>"));
            assert!(block.ends_with("</env>"));
            assert!(block.lines().count() <= 8, "{block}");
        }
    }

    #[test]
    fn description_contains_exact_env_block() {
        for family in [ShellFamily::Bash, ShellFamily::PowerShell] {
            let s = spec(family);
            let cwd = Path::new("/tmp/work");
            assert!(s.description(cwd).contains(&s.env_block(cwd)));
        }
    }

    #[test]
    fn tool_names_are_platform_unique() {
        assert_eq!(spec(ShellFamily::Bash).tool_name, "bash");
        assert_eq!(spec(ShellFamily::PowerShell).tool_name, "powershell");
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
}
