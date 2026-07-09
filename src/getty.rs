use std::{
    fs, io,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::cmdline::KernelCommandLine;

const ACTIVE_CONSOLES: &str = "/sys/class/tty/console/active";
const DEFAULT_BAUD: &str = "115200";
const GETTY_SUPERVISOR: &str = "/bin/sh";
const GETTY_SUPERVISOR_SCRIPT: &str =
    "while true; do /sbin/getty -i -L -n -l /bin/sh \"$1\" \"$2\" linux; sleep 1; done";
const PATH: &str = "/bin:/sbin:/usr/bin:/usr/sbin";

static STARTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn spawn(cmdline: &KernelCommandLine) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let active = fs::read_to_string(ACTIVE_CONSOLES)
        .map_err(|err| {
            tracing::debug!(path = ACTIVE_CONSOLES, error = ?err, "failed to read active consoles")
        })
        .ok();
    let consoles = selected_consoles(cmdline, active.as_deref());
    if consoles.is_empty() {
        tracing::warn!(
            path = ACTIVE_CONSOLES,
            "no consoles found for getty startup"
        );
        return;
    }

    for console in consoles {
        match spawn_console(&console) {
            Ok(pid) => tracing::info!(
                pid,
                tty = console.tty,
                baud = console.baud,
                "getty supervisor spawned"
            ),
            Err(err) => {
                tracing::warn!(tty = console.tty, baud = console.baud, error = ?err, "failed to spawn getty supervisor")
            }
        }
    }
}

fn spawn_console(console: &Console) -> io::Result<u32> {
    let child = Command::new(GETTY_SUPERVISOR)
        .arg("-c")
        .arg(GETTY_SUPERVISOR_SCRIPT)
        .arg("pocketboot-getty")
        .arg(&console.baud)
        .arg(&console.tty)
        .env("HOME", "/")
        .env("PATH", PATH)
        .env("TERM", "linux")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(child.id())
}

fn selected_consoles(cmdline: &KernelCommandLine, active: Option<&str>) -> Vec<Console> {
    let cmdline_consoles = cmdline_console_specs(cmdline);
    let active_consoles = active.map(active_console_names).unwrap_or_default();

    if active_consoles.is_empty() {
        return dedupe_console_specs(cmdline_consoles);
    }

    let mut consoles = Vec::new();
    for tty in active_consoles {
        push_console(
            &mut consoles,
            Console {
                baud: baud_for_tty(&cmdline_consoles, &tty),
                tty,
            },
        );
    }
    consoles
}

fn cmdline_console_specs(cmdline: &KernelCommandLine) -> Vec<Console> {
    cmdline
        .values("console")
        .filter_map(parse_console_spec)
        .collect()
}

fn parse_console_spec(value: &str) -> Option<Console> {
    let (tty, options) = value.split_once(',').unwrap_or((value, ""));
    let tty = tty.trim();
    if tty.is_empty() || tty == "null" {
        return None;
    }

    Some(Console {
        baud: parse_baud(options),
        tty: tty.to_string(),
    })
}

fn parse_baud(options: &str) -> String {
    let baud = options
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if baud.is_empty() {
        DEFAULT_BAUD.to_string()
    } else {
        baud
    }
}

fn active_console_names(active: &str) -> Vec<String> {
    active
        .split_whitespace()
        .filter(|tty| !tty.is_empty() && *tty != "null")
        .map(str::to_string)
        .collect()
}

fn baud_for_tty(cmdline_consoles: &[Console], tty: &str) -> String {
    cmdline_consoles
        .iter()
        .rev()
        .find(|console| console.tty == tty)
        .map(|console| console.baud.clone())
        .unwrap_or_else(|| DEFAULT_BAUD.to_string())
}

fn dedupe_console_specs(consoles: Vec<Console>) -> Vec<Console> {
    let mut deduped = Vec::new();
    for console in consoles {
        push_console(&mut deduped, console);
    }
    deduped
}

fn push_console(consoles: &mut Vec<Console>, console: Console) {
    if consoles.iter().any(|existing| existing.tty == console.tty) {
        return;
    }
    consoles.push(console);
}

#[derive(Debug, PartialEq, Eq)]
struct Console {
    tty: String,
    baud: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_active_consoles_with_cmdline_baud_rates() {
        let cmdline =
            KernelCommandLine::parse("console=tty0 console=ttyAMA0,921600n8 console=ttyS0,115200");

        assert_eq!(
            selected_consoles(&cmdline, Some("tty0 ttyAMA0\n")),
            vec![
                Console {
                    tty: "tty0".to_string(),
                    baud: DEFAULT_BAUD.to_string(),
                },
                Console {
                    tty: "ttyAMA0".to_string(),
                    baud: "921600".to_string(),
                },
            ]
        );
    }

    #[test]
    fn falls_back_to_cmdline_consoles() {
        let cmdline = KernelCommandLine::parse(
            "console=ttyS0,115200 console=ttyS0,9600 console=null console=hvc0",
        );

        assert_eq!(
            selected_consoles(&cmdline, None),
            vec![
                Console {
                    tty: "ttyS0".to_string(),
                    baud: "115200".to_string(),
                },
                Console {
                    tty: "hvc0".to_string(),
                    baud: DEFAULT_BAUD.to_string(),
                },
            ]
        );
    }
}
