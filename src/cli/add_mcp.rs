use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context};
use clap::{Args, ValueEnum};

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum RunnerChoice {
    Auto,
    Bunx,
    Npx,
}

impl RunnerChoice {
    fn program(self) -> Option<&'static str> {
        match self {
            RunnerChoice::Auto => None,
            RunnerChoice::Bunx => Some("bunx"),
            RunnerChoice::Npx => Some("npx"),
        }
    }
}

#[derive(Args)]
pub struct AddMcpArgs {
    /// Repository root to expose through MCP (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: PathBuf,

    /// Agent host target forwarded to add-mcp; repeat for multiple hosts
    #[arg(long = "agent", value_name = "AGENT")]
    pub agents: Vec<String>,

    /// Forward add-mcp global-scope setup
    #[arg(long)]
    pub global: bool,

    /// Forward add-mcp non-interactive confirmation
    #[arg(long)]
    pub yes: bool,

    /// Package runner used to execute add-mcp
    #[arg(long, value_enum, default_value_t = RunnerChoice::Auto)]
    pub runner: RunnerChoice,
}

pub async fn exec(args: AddMcpArgs) -> anyhow::Result<()> {
    let repo_path = match canonical_repo_path(&args.path) {
        Ok(path) => path,
        Err(err) => {
            print_manual_fallback(None);
            return Err(err);
        }
    };

    let runner = match select_runner(args.runner, std::env::var_os("PATH").as_deref()) {
        Ok(runner) => runner,
        Err(err) => {
            print_manual_fallback(Some(&repo_path));
            return Err(err);
        }
    };

    let status = match Command::new(runner)
        .arg("add-mcp")
        .args(add_mcp_argv(&repo_path, &args))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            print_manual_fallback(Some(&repo_path));
            return Err(err).with_context(|| format!("failed to start {runner}"));
        }
    };

    if !status.success() {
        print_manual_fallback(Some(&repo_path));
        bail!("add-mcp exited with {}", describe_status(status));
    }

    Ok(())
}

fn canonical_repo_path(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve repository path {}", path.display()))?;
    if !canonical.is_dir() {
        bail!(
            "repository path is not a directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn select_runner(choice: RunnerChoice, path_env: Option<&OsStr>) -> anyhow::Result<&'static str> {
    match choice {
        RunnerChoice::Auto => {
            if executable_on_path("bunx", path_env) {
                return Ok("bunx");
            }
            if executable_on_path("npx", path_env) {
                return Ok("npx");
            }
            Err(anyhow!("could not find bunx or npx on PATH"))
        }
        explicit => {
            let program = explicit
                .program()
                .expect("explicit runner choices have a program");
            if executable_on_path(program, path_env) {
                Ok(program)
            } else {
                Err(anyhow!("selected runner `{program}` was not found on PATH"))
            }
        }
    }
}

fn executable_on_path(program: &str, path_env: Option<&OsStr>) -> bool {
    let Some(path_env) = path_env else {
        return false;
    };

    std::env::split_paths(path_env)
        .any(|dir| candidate_names(program).any(|name| is_executable_file(&dir.join(name))))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn candidate_names(program: &str) -> impl Iterator<Item = OsString> + '_ {
    let base = OsString::from(program);
    #[cfg(windows)]
    {
        let pathext = std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| {
                        if ext.starts_with('.') {
                            format!("{program}{ext}")
                        } else {
                            format!("{program}.{ext}")
                        }
                    })
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![OsString::from(format!("{program}.exe"))]);

        std::iter::once(base).chain(pathext)
    }
    #[cfg(not(windows))]
    {
        std::iter::once(base)
    }
}

fn add_mcp_argv(repo_path: &Path, args: &AddMcpArgs) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from(server_source(repo_path)),
        OsString::from("--name"),
        OsString::from("oneup"),
    ];

    for agent in &args.agents {
        argv.push(OsString::from("--agent"));
        argv.push(OsString::from(agent));
    }

    if args.global {
        argv.push(OsString::from("--global"));
    }

    if args.yes {
        argv.push(OsString::from("--yes"));
    }

    argv
}

fn server_source(repo_path: &Path) -> String {
    format!("1up mcp --path {}", shell_quote(repo_path.as_os_str()))
}

fn shell_quote(value: &OsStr) -> String {
    let raw = value.to_string_lossy();
    if !raw.is_empty()
        && raw.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '/' | '\\' | '.' | '_' | '-' | ':')
        })
    {
        return raw.into_owned();
    }

    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn print_manual_fallback(repo_path: Option<&Path>) {
    let path = repo_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<absolute-repo-path>".to_string());
    eprintln!("{}", manual_fallback_text(&path));
}

fn manual_fallback_text(repo_path: &str) -> String {
    format!(
        "Manual MCP setup fallback\n\
         \n\
         Configure server identity `oneup` with command `1up` and args:\n\
         [\"mcp\", \"--path\", \"{repo_path}\"]\n\
         \n\
         Generic MCP JSON:\n\
         {{\"mcpServers\":{{\"oneup\":{{\"command\":\"1up\",\"args\":[\"mcp\",\"--path\",\"{repo_path}\"]}}}}}}\n\
         \n\
         Codex TOML:\n\
         [mcp_servers.oneup]\n\
         command = \"1up\"\n\
         args = [\"mcp\", \"--path\", \"{repo_path}\"]\n\
         \n\
         After saving host-owned configuration, reload the agent host, list MCP tools, and call `oneup_status`."
    )
}

fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "a signal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_runner(path: &Path) {
        fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn auto_runner_prefers_bunx_before_npx() {
        let tmp = tempfile::tempdir().unwrap();
        write_runner(&tmp.path().join("bunx"));
        write_runner(&tmp.path().join("npx"));

        assert_eq!(
            select_runner(RunnerChoice::Auto, Some(tmp.path().as_os_str())).unwrap(),
            "bunx"
        );
    }

    #[test]
    fn explicit_runner_requires_selected_binary() {
        let tmp = tempfile::tempdir().unwrap();
        write_runner(&tmp.path().join("bunx"));

        let err = select_runner(RunnerChoice::Npx, Some(tmp.path().as_os_str())).unwrap_err();

        assert!(err.to_string().contains("selected runner `npx`"));
    }

    #[test]
    fn add_mcp_args_include_single_server_source_and_forwarded_options() {
        let args = AddMcpArgs {
            path: PathBuf::from("."),
            agents: vec!["codex".to_string(), "cursor".to_string()],
            global: true,
            yes: true,
            runner: RunnerChoice::Auto,
        };

        let argv = add_mcp_argv(Path::new("/tmp/repo"), &args);

        assert_eq!(
            argv,
            vec![
                OsString::from("1up mcp --path /tmp/repo"),
                OsString::from("--name"),
                OsString::from("oneup"),
                OsString::from("--agent"),
                OsString::from("codex"),
                OsString::from("--agent"),
                OsString::from("cursor"),
                OsString::from("--global"),
                OsString::from("--yes"),
            ]
        );
    }

    #[test]
    fn manual_fallback_mentions_verification_and_oneup_identity() {
        let text = manual_fallback_text("/tmp/repo");

        assert!(text.contains("server identity `oneup`"));
        assert!(text.contains("[\"mcp\", \"--path\", \"/tmp/repo\"]"));
        assert!(text.contains("call `oneup_status`"));
    }
}
