//! Small command execution wrapper for mesh network configuration.
//!
//! For v0, mesh configuration shells out to `/bin/busybox ip`. Every
//! command logs its program, arguments, exit status, and stderr on
//! failure. No command failure panics.
//!
//! Command construction is kept separate from execution so that argument
//! generation can be unit-tested without running `ip`.

use std::io;
use std::process::Command;

const BUSYBOX_IP: &str = "/bin/busybox";

/// Error from a failed command, carrying stderr for diagnostics.
#[derive(Debug)]
pub(crate) struct CommandError {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) status: Option<i32>,
    pub(crate) stderr: String,
    pub(crate) kind: CommandErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandErrorKind {
    Spawn,
    Exit,
}

impl CommandError {
    /// True when the failure was an "already exists" style error from `ip`.
    pub(crate) fn is_already_exists(&self) -> bool {
        let stderr = self.stderr.to_ascii_lowercase();
        stderr.contains("exists")
            || stderr.contains("already in use")
            || stderr.contains("file exists")
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args = self.args.join(" ");
        match self.kind {
            CommandErrorKind::Spawn => {
                write!(f, "spawn {} {}: {}", self.program, args, self.stderr)
            }
            CommandErrorKind::Exit => {
                let code = self
                    .status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                write!(
                    f,
                    "{} {} exited with {code}: {}",
                    self.program,
                    args,
                    self.stderr.trim()
                )
            }
        }
    }
}

impl std::error::Error for CommandError {}

impl From<CommandError> for io::Error {
    fn from(err: CommandError) -> io::Error {
        let kind = match err.kind {
            CommandErrorKind::Spawn => io::ErrorKind::NotFound,
            CommandErrorKind::Exit => io::ErrorKind::Other,
        };
        io::Error::new(kind, err.to_string())
    }
}

/// Build the argument vector for a `busybox ip` invocation.
///
/// Exposed for unit tests so argument construction can be checked without
/// running `ip`.
pub(crate) fn ip_args<const N: usize>(args: [&str; N]) -> Vec<String> {
    let mut out = Vec::with_capacity(N + 1);
    out.push("ip".to_string());
    for arg in args {
        out.push(arg.to_string());
    }
    out
}

/// Run `busybox ip <args>` and discard output on success.
///
/// Returns the typed `CommandError` so callers can inspect
/// `is_already_exists()` without downcasting.
pub(crate) fn run_ip<const N: usize>(args: [&str; N]) -> Result<(), CommandError> {
    let argv = ip_args(args);
    run_busybox(&argv)
}

/// Run `/bin/busybox` with the given arguments, status-only.
pub(crate) fn run_busybox(args: &[String]) -> Result<(), CommandError> {
    let mut command = Command::new(BUSYBOX_IP);
    command.args(args.iter().skip(1));
    let arg_display: Vec<String> = args.iter().skip(1).cloned().collect();
    let output = command.output().map_err(|err| CommandError {
        program: BUSYBOX_IP.to_string(),
        args: arg_display.clone(),
        status: None,
        stderr: err.to_string(),
        kind: CommandErrorKind::Spawn,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        tracing::warn!(
            program = BUSYBOX_IP,
            args = ?arg_display,
            status = ?output.status,
            stderr = %stderr.trim(),
            "command failed"
        );
        return Err(CommandError {
            program: BUSYBOX_IP.to_string(),
            args: arg_display,
            status: output.status.code(),
            stderr,
            kind: CommandErrorKind::Exit,
        });
    }

    tracing::debug!(
        program = BUSYBOX_IP,
        args = ?arg_display,
        status = ?output.status,
        "command succeeded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_args_prepends_ip_subcommand() {
        let args = ip_args(["-6", "addr", "add", "fd00::1/128", "dev", "lo"]);
        assert_eq!(
            args,
            vec![
                "ip".to_string(),
                "-6".to_string(),
                "addr".to_string(),
                "add".to_string(),
                "fd00::1/128".to_string(),
                "dev".to_string(),
                "lo".to_string(),
            ]
        );
    }

    #[test]
    fn command_error_detects_already_exists() {
        let err = CommandError {
            program: "/bin/busybox".into(),
            args: vec!["ip".into()],
            status: Some(2),
            stderr: "RTNETLINK answers: File exists".into(),
            kind: CommandErrorKind::Exit,
        };
        assert!(err.is_already_exists());
    }

    #[test]
    fn command_error_detects_already_in_use() {
        let err = CommandError {
            program: "/bin/busybox".into(),
            args: vec!["ip".into()],
            status: Some(2),
            stderr: "Address already in use".into(),
            kind: CommandErrorKind::Exit,
        };
        assert!(err.is_already_exists());
    }
}
