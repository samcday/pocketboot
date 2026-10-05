use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{self, Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use clap::ValueEnum;
use serde_json::{Value, json};

use crate::Result;

use super::{
    path_command_exists,
    qemu::{self, QEMU_CONSOLE as CONSOLE},
    run_command, target_dir, workspace_root,
};

const DECISION: &str = "POCKETBOOT_BOOT_DECISION decision=\"";
const BOOT_FAILED: &str = "decision=\"menu\" reason=boot-failed entry=\"broken kernel\" error=\"";
const GEN_START: &str = "Booting Linux on physical CPU";
const TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(20);
const KEY_REPEAT: Duration = Duration::from_millis(50);
const QMP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const QMP_CONNECT_BACKOFF: Duration = Duration::from_millis(50);
const QMP_IO_TIMEOUT: Duration = Duration::from_secs(2);
const ESP_SIZE: u64 = 48 << 20;
const DISK_SIZE: u64 = 64 << 20;
const ESP_OFFSET: u64 = 2048 * 512;

#[derive(clap::Args, Debug)]
pub(crate) struct QemuBootPolicyArgs {
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: PathBuf,
    #[arg(
        value_enum,
        value_name = "SCENARIO",
        help = "scenario to run (default: all)"
    )]
    scenarios: Vec<Scenario>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Scenario {
    Autoboot,
    VolumeDownHold,
    EmptyDisk,
    BrokenDefault,
    CmdlineMenu,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Autoboot => "autoboot",
            Self::VolumeDownHold => "volume-down-hold",
            Self::EmptyDisk => "empty-disk",
            Self::BrokenDefault => "broken-default",
            Self::CmdlineMenu => "cmdline-menu",
        }
    }
}

pub(crate) fn run(args: QemuBootPolicyArgs) -> Result<()> {
    let qemu = qemu::qemu_binary();
    require_tools(&qemu)?;
    let workspace_root = workspace_root()?;
    let build = qemu::build_qemu_kernel(&workspace_root, &args.kernel_tree)?;
    let work = target_dir(&workspace_root).join("qemu-boot-policy");
    fs::create_dir_all(&work).map_err(|err| format!("create {}: {err}", work.display()))?;
    let harness = Harness {
        qemu,
        image: build.image,
        work,
    };

    let scenarios = if args.scenarios.is_empty() {
        Scenario::value_variants().to_vec()
    } else {
        args.scenarios
    };
    let mut failed = Vec::new();
    for scenario in scenarios {
        let report = harness.run(scenario).map_err(|err| {
            format!(
                "{}: {err}; logs in {}",
                scenario.name(),
                harness.work.display()
            )
        })?;
        let verdict = if report.passed() { "PASS" } else { "FAIL" };
        println!("{verdict} {}", scenario.name());
        for check in &report.checks {
            let status = if check.ok { "ok  " } else { "FAIL" };
            let detail = if check.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", check.detail)
            };
            println!("  {status} {}{detail}", check.label);
        }
        if !report.passed() {
            failed.push(scenario.name());
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed: {}; logs in {}",
            failed.join(", "),
            harness.work.display()
        ))
    }
}

fn require_tools(qemu: &OsStr) -> Result<()> {
    let tools = [
        (qemu, "install qemu-system-aarch64 or set QEMU"),
        (OsStr::new("mkfs.vfat"), "install dosfstools"),
        (OsStr::new("mmd"), "install mtools"),
        (OsStr::new("mcopy"), "install mtools"),
        (OsStr::new("sgdisk"), "install gdisk"),
    ];
    let missing = tools
        .iter()
        .filter(|(tool, _)| !tool_exists(tool))
        .map(|(tool, hint)| format!("{} ({hint})", tool.to_string_lossy()))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing host tools: {}", missing.join(", ")))
    }
}

fn tool_exists(tool: &OsStr) -> bool {
    let path = Path::new(tool);
    if path.components().count() > 1 {
        path.is_file()
    } else {
        tool.to_str().is_some_and(path_command_exists)
    }
}

struct Harness {
    qemu: OsString,
    image: PathBuf,
    work: PathBuf,
}

impl Harness {
    fn run(&self, scenario: Scenario) -> Result<Report> {
        match scenario {
            Scenario::Autoboot => self.autoboot(),
            Scenario::VolumeDownHold => self.volume_down_hold(),
            Scenario::EmptyDisk => self.empty_disk(),
            Scenario::BrokenDefault => self.broken_default(),
            Scenario::CmdlineMenu => self.cmdline_menu(),
        }
    }

    fn autoboot(&self) -> Result<Report> {
        let scenario = Scenario::Autoboot;
        let disk = self.good_disk(scenario, &["pocketboot-gen2"])?;
        let mut q = self.boot(scenario, &disk, "", None)?;
        let mut r = Report::default();
        r.check(
            "gen1 decides autoboot",
            q.decides(1, TIMEOUT, "autoboot", None),
        );
        r.check("kexec starts", q.wait_for("Starting new kernel", TIMEOUT));
        let text = q.text();
        let gen1 = generation(&text, 1);
        for marker in [
            "UI thread spawned",
            "USB gadget thread spawned",
            "battery watcher thread spawned",
        ] {
            r.check(
                format!("gen1 never logs '{marker}'"),
                !gen1.contains(marker),
            );
        }
        r.check(
            "gen2 menu via pocketboot.menu",
            q.decides(2, TIMEOUT, "menu", Some("cmdline")),
        );
        if let Some(ms) = kexec_latency_ms(gen1) {
            r.check_detail("gen1 starting up -> kexec", true, format!("{ms:.0} ms"));
        }
        Ok(r)
    }

    fn volume_down_hold(&self) -> Result<Report> {
        let scenario = Scenario::VolumeDownHold;
        let disk = self.good_disk(scenario, &["b-first", "a-second"])?;
        // Unix socket: fixed TCP ports collide across worktrees, and the
        // socket path limit rules out anything under target/.
        let socket =
            env::temp_dir().join(format!("pocketboot-qemu-boot-policy-{}.qmp", process::id()));
        let mut q = self.boot(scenario, &disk, "", Some(&socket))?;
        let mut qmp = Qmp::connect(&socket)?;
        let mut r = Report::default();
        // The guest drops input sent before virtio-input is up, so keep pressing.
        let deadline = Instant::now() + TIMEOUT;
        while parse_decision(&q.text()).is_none() && Instant::now() < deadline && q.running() {
            if let Err(err) = qmp.key("volumedown", true) {
                return q.qmp_failed(r, err);
            }
            thread::sleep(KEY_REPEAT);
        }
        r.check(
            "decision is menu/volume-down",
            q.decides(1, Duration::from_secs(1), "menu", Some("volume-down")),
        );
        r.check(
            "watcher logged break-in",
            q.text().contains("volume-down break-in detected"),
        );
        thread::sleep(Duration::from_secs(2));
        if let Err(err) = qmp.key("volumedown", false) {
            return q.qmp_failed(r, err);
        }
        thread::sleep(Duration::from_secs(3));
        r.check("no kexec", !q.text().contains("Starting new kernel"));
        r.check(
            "UI started",
            q.wait_for("starting frankenSlint UI", Duration::from_secs(10)),
        );
        Ok(r)
    }

    fn empty_disk(&self) -> Result<Report> {
        let scenario = Scenario::EmptyDisk;
        let disk = self.disk_path(scenario);
        sized_file(&disk, DISK_SIZE)?;
        let mut q = self.boot(scenario, &disk, "", None)?;
        let mut r = Report::default();
        r.check(
            "decision is menu/no-bootable-entry",
            q.decides(1, TIMEOUT, "menu", Some("no-bootable-entry")),
        );
        thread::sleep(Duration::from_secs(5));
        r.check("PID 1 still alive", q.init_alive());
        Ok(r)
    }

    fn broken_default(&self) -> Result<Report> {
        let scenario = Scenario::BrokenDefault;
        let disk = self.make_disk(
            scenario,
            &[
                ("/not-a-kernel".to_string(), Payload::Bytes(vec![0; 65536])),
                (
                    "/loader/entries/broken.conf".to_string(),
                    Payload::Bytes(bls("broken kernel", "/not-a-kernel", CONSOLE).into_bytes()),
                ),
            ],
        )?;
        let mut q = self.boot(scenario, &disk, "", None)?;
        let mut r = Report::default();
        r.check(
            "autoboot attempted",
            q.decides(1, TIMEOUT, "autoboot", None),
        );
        let error = q.wait(TIMEOUT, None, |text| {
            quoted_after(text, BOOT_FAILED).map(str::to_string)
        });
        r.check_detail(
            "falls back to menu/boot-failed",
            error.is_some(),
            error.unwrap_or_default(),
        );
        thread::sleep(Duration::from_secs(5));
        r.check("PID 1 still alive", q.init_alive());
        r.check("no kexec", !q.text().contains("Starting new kernel"));
        Ok(r)
    }

    fn cmdline_menu(&self) -> Result<Report> {
        let scenario = Scenario::CmdlineMenu;
        let disk = self.good_disk(scenario, &["pocketboot-gen2"])?;
        let mut q = self.boot(scenario, &disk, "pocketboot.menu", None)?;
        let mut r = Report::default();
        r.check(
            "decision is menu/cmdline",
            q.decides(1, TIMEOUT, "menu", Some("cmdline")),
        );
        r.check(
            "discovery still completes",
            q.wait_for("boot discovery complete count=1", TIMEOUT),
        );
        thread::sleep(Duration::from_secs(3));
        r.check("no kexec", !q.text().contains("Starting new kernel"));
        r.check(
            "no break-in watcher",
            !q.text().contains("break-in watcher spawned"),
        );
        Ok(r)
    }

    fn disk_path(&self, scenario: Scenario) -> PathBuf {
        self.work.join(format!("{}.raw", scenario.name()))
    }

    fn good_disk(&self, scenario: Scenario, entries: &[&str]) -> Result<PathBuf> {
        let mut files = vec![("/pocketboot-Image".to_string(), Payload::Host(&self.image))];
        for entry in entries {
            let options = format!("{CONSOLE} pocketboot.menu pocketboot.test={entry}");
            files.push((
                format!("/loader/entries/{entry}.conf"),
                Payload::Bytes(bls(entry, "/pocketboot-Image", &options).into_bytes()),
            ));
        }
        self.make_disk(scenario, &files)
    }

    /// GPT disk with one FAT ESP at LBA 2048 holding `files`.
    fn make_disk(&self, scenario: Scenario, files: &[(String, Payload)]) -> Result<PathBuf> {
        let disk = self.disk_path(scenario);
        let esp = self.work.join(format!("{}.esp", scenario.name()));
        sized_file(&esp, ESP_SIZE)?;
        run_tool(
            "mkfs.vfat",
            [OsStr::new("-n"), OsStr::new("PBESP"), esp.as_os_str()],
        )?;
        run_tool(
            "mmd",
            [
                OsStr::new("-i"),
                esp.as_os_str(),
                OsStr::new("::/loader"),
                OsStr::new("::/loader/entries"),
            ],
        )?;
        let payload = self.work.join("payload.tmp");
        for (dest, src) in files {
            let src = match src {
                Payload::Host(path) => *path,
                Payload::Bytes(bytes) => {
                    fs::write(&payload, bytes)
                        .map_err(|err| format!("write {}: {err}", payload.display()))?;
                    &payload
                }
            };
            let dest = format!("::{dest}");
            run_tool(
                "mcopy",
                [
                    OsStr::new("-i"),
                    esp.as_os_str(),
                    src.as_os_str(),
                    OsStr::new(&dest),
                ],
            )?;
        }

        sized_file(&disk, DISK_SIZE)?;
        run_tool(
            "sgdisk",
            [
                OsStr::new("--clear"),
                OsStr::new("--new=1:2048:+48M"),
                OsStr::new("--typecode=1:ef00"),
                disk.as_os_str(),
            ],
        )?;
        let mut src = File::open(&esp).map_err(|err| format!("open {}: {err}", esp.display()))?;
        let mut dst = OpenOptions::new()
            .write(true)
            .open(&disk)
            .map_err(|err| format!("open {}: {err}", disk.display()))?;
        dst.seek(SeekFrom::Start(ESP_OFFSET))
            .and_then(|_| io::copy(&mut src, &mut dst))
            .map_err(|err| format!("copy ESP into {}: {err}", disk.display()))?;
        remove_if_exists(&esp)?;
        remove_if_exists(&payload)?;
        Ok(disk)
    }

    fn boot(
        &self,
        scenario: Scenario,
        disk: &Path,
        append: &str,
        qmp_socket: Option<&Path>,
    ) -> Result<Qemu> {
        let name = scenario.name();
        let log = self.work.join(format!("{name}.log"));
        // QEMU truncates the serial file only once it opens it; a stale log
        // would otherwise satisfy the first wait.
        remove_if_exists(&log)?;
        let stderr_path = self.work.join(format!("{name}.stderr.log"));
        let stderr = File::create(&stderr_path)
            .map_err(|err| format!("create {}: {err}", stderr_path.display()))?;
        let mut serial = OsString::from("file:");
        serial.push(&log);
        let mut drive = OsString::from("if=none,id=pb,format=raw,file=");
        drive.push(disk);
        drive.push(",snapshot=on");

        let mut command = Command::new(&self.qemu);
        if let Some(socket) = qmp_socket {
            remove_if_exists(socket)?;
            let mut qmp = OsString::from("unix:");
            qmp.push(socket);
            qmp.push(",server=on,wait=off");
            command.arg("-qmp").arg(qmp);
        }
        let child = command
            .args(["-machine", "virt", "-cpu", "max", "-smp", "2", "-m", "512M"])
            .args(["-display", "none", "-monitor", "none", "-serial"])
            .arg(serial)
            .args(["-no-reboot", "-global", "virtio-mmio.force-legacy=false"])
            .arg("-kernel")
            .arg(&self.image)
            .arg("-append")
            .arg(format!("{CONSOLE} panic=1 {append}").trim())
            .arg("-drive")
            .arg(drive)
            .args(["-device", "virtio-blk-device,drive=pb"])
            .args(["-device", "virtio-keyboard-device"])
            .args(["-device", "virtio-gpu-device"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("spawn {}: {err}", self.qemu.to_string_lossy()))?;
        Ok(Qemu {
            child,
            log,
            qmp_socket: qmp_socket.map(Path::to_path_buf),
        })
    }
}

enum Payload<'a> {
    Host(&'a Path),
    Bytes(Vec<u8>),
}

fn bls(title: &str, linux: &str, options: &str) -> String {
    format!("title {title}\nlinux {linux}\noptions {options}\n")
}

fn run_tool<'a>(program: &str, args: impl IntoIterator<Item = &'a OsStr>) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args).stdout(Stdio::null());
    run_command(command, program)
}

fn sized_file(path: &Path, len: u64) -> Result<()> {
    let file = File::create(path).map_err(|err| format!("create {}: {err}", path.display()))?;
    file.set_len(len)
        .map_err(|err| format!("size {}: {err}", path.display()))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            Err(format!("remove {}: {err}", path.display()))
        }
        _ => Ok(()),
    }
}

/// A running guest; killed when dropped so early returns never leak QEMU.
struct Qemu {
    child: Child,
    log: PathBuf,
    qmp_socket: Option<PathBuf>,
}

impl Qemu {
    fn text(&self) -> String {
        fs::read(&self.log)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn init_alive(&mut self) -> bool {
        self.running() && !self.text().contains("Attempted to kill init")
    }

    /// A QMP failure is a guest outcome once QEMU has exited (panic=1 with
    /// -no-reboot), so report it as a failed check instead of aborting.
    fn qmp_failed(&mut self, mut report: Report, err: String) -> Result<Report> {
        if self.running() {
            return Err(err);
        }
        let status = self
            .child
            .try_wait()
            .ok()
            .flatten()
            .map_or_else(|| "unknown".to_string(), |status| status.to_string());
        report.check_detail("QEMU still running", false, format!("{err}; qemu {status}"));
        Ok(report)
    }

    /// Poll the serial log (or one kernel generation of it) until `find`
    /// matches, the timeout lapses or QEMU exits.
    fn wait<T>(
        &mut self,
        timeout: Duration,
        boot: Option<usize>,
        find: impl Fn(&str) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let text = self.text();
            let text = boot.map_or(text.as_str(), |n| generation(&text, n));
            if let Some(found) = find(text) {
                return Some(found);
            }
            if !self.running() {
                break;
            }
            thread::sleep(POLL);
        }
        None
    }

    fn wait_for(&mut self, marker: &str, timeout: Duration) -> bool {
        self.wait(timeout, None, |text| text.contains(marker).then_some(()))
            .is_some()
    }

    fn decides(
        &mut self,
        boot: usize,
        timeout: Duration,
        decision: &str,
        reason: Option<&str>,
    ) -> bool {
        self.wait(timeout, Some(boot), parse_decision)
            .is_some_and(|found| found.decision == decision && found.reason.as_deref() == reason)
    }
}

impl Drop for Qemu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(socket) = &self.qmp_socket {
            let _ = fs::remove_file(socket);
        }
    }
}

struct Qmp {
    stream: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(socket: &Path) -> Result<Self> {
        let deadline = Instant::now() + QMP_CONNECT_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(err) if Instant::now() > deadline => {
                    return Err(format!("connect QMP {}: {err}", socket.display()));
                }
                Err(_) => thread::sleep(QMP_CONNECT_BACKOFF),
            }
        };
        stream
            .set_read_timeout(Some(QMP_IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(QMP_IO_TIMEOUT)))
            .map_err(|err| format!("configure QMP socket: {err}"))?;
        let mut qmp = Self {
            stream: BufReader::new(stream),
        };
        qmp.read()?;
        qmp.command("qmp_capabilities", None)?;
        Ok(qmp)
    }

    fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let read = self
            .stream
            .read_line(&mut line)
            .map_err(|err| match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    "QMP read timed out".to_string()
                }
                _ => format!("read QMP: {err}"),
            })?;
        if read == 0 {
            return Err("QMP connection closed".to_string());
        }
        serde_json::from_str(&line).map_err(|err| format!("decode QMP message {line:?}: {err}"))
    }

    fn command(&mut self, name: &str, arguments: Option<Value>) -> Result<Value> {
        let mut message = json!({ "execute": name });
        if let Some(arguments) = arguments {
            message["arguments"] = arguments;
        }
        let line = format!("{message}\n");
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .map_err(|err| format!("write QMP {name}: {err}"))?;
        loop {
            let reply = self.read()?;
            if let Some(error) = reply.get("error") {
                return Err(format!("QMP {name}: {error}"));
            }
            if reply.get("return").is_some() {
                return Ok(reply);
            }
        }
    }

    fn key(&mut self, qcode: &str, down: bool) -> Result<()> {
        let event = json!({
            "type": "key",
            "data": { "down": down, "key": { "type": "qcode", "data": qcode } },
        });
        self.command("input-send-event", Some(json!({ "events": [event] })))
            .map(drop)
    }
}

#[derive(Default)]
struct Report {
    checks: Vec<Check>,
}

struct Check {
    label: String,
    ok: bool,
    detail: String,
}

impl Report {
    fn check(&mut self, label: impl Into<String>, ok: bool) {
        self.check_detail(label, ok, String::new());
    }

    fn check_detail(&mut self, label: impl Into<String>, ok: bool, detail: String) {
        self.checks.push(Check {
            label: label.into(),
            ok,
            detail,
        });
    }

    fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    decision: String,
    reason: Option<String>,
}

/// First `POCKETBOOT_BOOT_DECISION decision="word"[ reason=word]` marker.
fn parse_decision(text: &str) -> Option<Decision> {
    text.match_indices(DECISION).find_map(|(at, _)| {
        let rest = &text[at + DECISION.len()..];
        let decision = take_while(rest, is_word);
        let rest = rest[decision.len()..].strip_prefix('"')?;
        if decision.is_empty() {
            return None;
        }
        let reason = rest
            .strip_prefix(" reason=")
            .map(|rest| take_while(rest, |ch| is_word(ch) || ch == '-'))
            .filter(|reason| !reason.is_empty());
        Some(Decision {
            decision: decision.to_string(),
            reason: reason.map(str::to_string),
        })
    })
}

/// Text of kernel generation `n` (1 is the first boot, 2 the kexec'd one).
fn generation(text: &str, n: usize) -> &str {
    text.split(GEN_START).nth(n).unwrap_or("")
}

fn kexec_latency_ms(gen1: &str) -> Option<f64> {
    let start = log_timestamp(gen1, |rest| rest.contains("starting up"))?;
    let kexec = log_timestamp(gen1, |rest| {
        rest.starts_with(" kexec_core: Starting new kernel")
    })?;
    Some((kexec - start) * 1000.0)
}

/// First `[ seconds]` stamp whose remainder of the line satisfies `accept`.
fn log_timestamp(text: &str, accept: impl Fn(&str) -> bool) -> Option<f64> {
    text.lines().find_map(|line| {
        line.match_indices('[').find_map(|(at, _)| {
            let rest = line[at + 1..].trim_start();
            let stamp = take_while(rest, |ch| ch.is_ascii_digit() || ch == '.');
            let tail = rest[stamp.len()..].strip_prefix(']')?;
            if stamp.is_empty() || !accept(tail) {
                return None;
            }
            stamp.parse().ok()
        })
    })
}

fn quoted_after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.match_indices(prefix).find_map(|(at, _)| {
        let rest = &text[at + prefix.len()..];
        rest.find('"').map(|end| &rest[..end])
    })
}

fn take_while(text: &str, keep: impl Fn(char) -> bool) -> &str {
    let end = text.find(|ch: char| !keep(ch)).unwrap_or(text.len());
    &text[..end]
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN1: &str = "\
[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x000f0510]\r
[    0.404459]  INFO pocketboot: starting up\r
[    0.419029]  INFO pocketboot::breakin: volume-down break-in watcher spawned thread=\"pocketboot-breakin\"\r
[    0.872053]  INFO pocketboot: POCKETBOOT_BOOT_DECISION decision=\"autoboot\" id=pocketboot-gen2.conf elapsed_ms=476\r
[    1.042261] kexec_core: Starting new kernel\r
[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x000f0510]\r
[    0.857021]  INFO pocketboot: starting up\r
[    0.879069]  INFO pocketboot: POCKETBOOT_BOOT_DECISION decision=\"menu\" reason=cmdline elapsed_ms=35\r
";

    fn decision(decision: &str, reason: Option<&str>) -> Option<Decision> {
        Some(Decision {
            decision: decision.to_string(),
            reason: reason.map(str::to_string),
        })
    }

    #[test]
    fn decision_without_reason_is_autoboot() {
        assert_eq!(parse_decision(GEN1), decision("autoboot", None));
    }

    #[test]
    fn decision_reason_allows_hyphens() {
        let line = "~ # [    0.549639]  INFO pocketboot: POCKETBOOT_BOOT_DECISION \
                    decision=\"menu\" reason=volume-down elapsed_ms=61";
        assert_eq!(parse_decision(line), decision("menu", Some("volume-down")));
        let line = "POCKETBOOT_BOOT_DECISION decision=\"menu\" reason=boot-failed \
                    entry=\"broken kernel\" error=\"kernel is not a raw arm64 Image\"";
        assert_eq!(parse_decision(line), decision("menu", Some("boot-failed")));
    }

    #[test]
    fn decision_skips_malformed_markers() {
        assert_eq!(parse_decision("no marker here"), None);
        let text = "POCKETBOOT_BOOT_DECISION decision=\"\"\n\
                    POCKETBOOT_BOOT_DECISION decision=\"half\n\
                    POCKETBOOT_BOOT_DECISION decision=\"menu\" reason= elapsed_ms=1";
        assert_eq!(parse_decision(text), decision("menu", None));
    }

    #[test]
    fn generations_split_on_kernel_banner() {
        assert_eq!(generation(GEN1, 0), "[    0.000000] ");
        assert_eq!(
            parse_decision(generation(GEN1, 1)),
            decision("autoboot", None)
        );
        assert!(generation(GEN1, 1).contains("Starting new kernel"));
        assert_eq!(
            parse_decision(generation(GEN1, 2)),
            decision("menu", Some("cmdline"))
        );
        assert_eq!(generation(GEN1, 3), "");
        assert_eq!(generation("no banner", 1), "");
    }

    #[test]
    fn kexec_latency_uses_first_generation_stamps() {
        let ms = kexec_latency_ms(generation(GEN1, 1)).unwrap();
        assert!((ms - 637.802).abs() < 1e-6, "{ms}");
        assert_eq!(format!("{ms:.0} ms"), "638 ms");
    }

    #[test]
    fn kexec_latency_needs_both_markers() {
        assert_eq!(kexec_latency_ms(generation(GEN1, 2)), None);
        assert_eq!(
            kexec_latency_ms("[    1.0] kexec_core: Starting new kernel\n"),
            None
        );
        assert_eq!(
            kexec_latency_ms("[ 1.0] foo kexec_core: Starting new kernel\n[ 0.5] starting up\n"),
            None
        );
    }

    #[test]
    fn boot_failed_error_is_captured() {
        let line = "WARN pocketboot: POCKETBOOT_BOOT_DECISION decision=\"menu\" \
                    reason=boot-failed entry=\"broken kernel\" \
                    error=\"kernel is not a raw arm64 Image\" elapsed_ms=513";
        assert_eq!(
            quoted_after(line, BOOT_FAILED),
            Some("kernel is not a raw arm64 Image")
        );
        assert_eq!(
            quoted_after("decision=\"menu\" reason=cmdline", BOOT_FAILED),
            None
        );
    }

    #[test]
    fn bls_entry_format() {
        assert_eq!(
            bls("broken kernel", "/not-a-kernel", "quiet"),
            "title broken kernel\nlinux /not-a-kernel\noptions quiet\n"
        );
    }
}
