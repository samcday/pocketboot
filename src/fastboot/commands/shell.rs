use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    fastboot::{CommandContext, CommandResult},
    kexec, reaper,
};

const COMMAND_PREFIX: &str = "oem shell:";
const STAGED_SCRIPT_MAX: u64 = 64 * 1024;
const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
const OUTPUT_LIMIT_BLOCKS: u64 = OUTPUT_LIMIT / 512;
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL: &str = "/bin/sh";

pub(super) fn handle(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    let command = parse_command(command)?;
    run_and_stage(context, command)
}

pub(super) fn handle_staged(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    let command = read_staged_command(context)?;
    run_and_stage(context, &command)
}

fn run_and_stage(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    let output = run_shell(command)?;
    let size = output.size;
    let description = output.description();
    let success = output.success();
    context.stage_file("shell-output", output.file, size);
    context.info(format!("shell {description}; {size} bytes staged"))?;
    context.info(b"run fastboot get_staged /dev/stdout to view")?;

    if success {
        context.okay(description)?;
    } else {
        context.fail(description)?;
    }
    Ok(CommandResult::continue_())
}

struct ShellOutput {
    file: File,
    size: u64,
    status: ExitStatus,
    timed_out: bool,
}

impl ShellOutput {
    fn success(&self) -> bool {
        !self.timed_out && self.status.success()
    }

    fn description(&self) -> String {
        if self.timed_out {
            return "timed out".to_string();
        }
        if self.status.signal() == Some(libc::SIGXFSZ) {
            return "output too large".to_string();
        }
        if let Some(code) = self.status.code() {
            format!("exited {code}")
        } else if let Some(signal) = self.status.signal() {
            format!("killed by signal {signal}")
        } else {
            "ended without exit status".to_string()
        }
    }
}

fn parse_command(command: &str) -> io::Result<&str> {
    let command = command
        .strip_prefix(COMMAND_PREFIX)
        .ok_or_else(|| invalid_input("invalid shell command"))?
        .trim();
    if command.is_empty() {
        return Err(invalid_input("shell command is empty"));
    }
    Ok(command)
}

fn read_staged_command(context: &CommandContext<'_>) -> io::Result<String> {
    let mut staged = context.staged_file()?;
    let mut limited = Read::by_ref(&mut staged).take(STAGED_SCRIPT_MAX + 1);
    let mut command = String::new();
    limited.read_to_string(&mut command)?;
    if command.len() as u64 > STAGED_SCRIPT_MAX {
        return Err(invalid_input("staged shell command is too large"));
    }
    if command.trim().is_empty() {
        return Err(invalid_input("staged shell command is empty"));
    }
    Ok(command)
}

fn run_shell(shell_command: &str) -> io::Result<ShellOutput> {
    let mut output = kexec::create_payload_memfd("shell-output")?;
    let stdout = output.try_clone()?;
    let stderr = output.try_clone()?;
    let _guard = reaper::child_guard();
    let mut child = {
        let mut command = Command::new(SHELL);
        command
            .arg("-c")
            .arg(shell_wrapper())
            .arg("pocketboot-shell")
            .arg(shell_command)
            .env("HOME", "/")
            .env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin")
            .env("TERM", "linux")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        command.spawn()
    }
    .map_err(|err| io::Error::new(err.kind(), format!("spawn {SHELL}: {err}")))?;

    let deadline = Instant::now() + SHELL_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            terminate_process_group(child.id());
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(25));
    };

    output.seek(SeekFrom::End(0))?;
    if timed_out {
        output.write_all(b"\npocketboot: shell command timed out\n")?;
    }
    let size = output.seek(SeekFrom::End(0))?;
    output.seek(SeekFrom::Start(0))?;

    Ok(ShellOutput {
        file: output,
        size,
        status,
        timed_out,
    })
}

fn shell_wrapper() -> String {
    format!("ulimit -f {OUTPUT_LIMIT_BLOCKS}; exec {SHELL} -c \"$1\"")
}

fn terminate_process_group(pid: u32) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_command() {
        assert_eq!(parse_command("oem shell:ls /dev").unwrap(), "ls /dev");
    }

    #[test]
    fn rejects_empty_shell_command() {
        let err = parse_command("oem shell:").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn runs_shell_command_and_captures_output() {
        let mut output = run_shell("printf pocketboot-shell").unwrap();
        assert!(
            output.success(),
            "unexpected status: {}",
            output.description()
        );

        let mut contents = String::new();
        output.file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "pocketboot-shell");
    }
}
