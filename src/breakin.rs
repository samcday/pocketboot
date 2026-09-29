use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirEntryExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    boot_state::PonKeys,
    input::{
        EV_KEY, EV_SYN, INPUT, InputEvent, KEY_BITMAP_BYTES, KEY_VOLUMEDOWN, SYN_DROPPED,
        eviocgkey, ioctl_read, sysfs_event_has_key, test_bit,
    },
};

const THREAD_NAME: &str = "pocketboot-breakin";
const POLL_TIMEOUT_MS: libc::c_int = 100;
const RESCAN_INTERVAL: Duration = Duration::from_millis(250);
const PMIC_PON_SOURCE: &str = "pmic-pon";

/// Watches for volume-down during autoboot, via evdev and the PMIC PON
/// registers, until it fires once or is stopped.
pub(crate) struct Watcher {
    shared: Arc<Shared>,
    wake: Arc<File>,
    thread: Option<thread::JoinHandle<Vec<File>>>,
}

struct Shared {
    stop: AtomicBool,
    triggered: AtomicBool,
}

impl Watcher {
    pub(crate) fn spawn(
        started: Instant,
        pon_keys: Option<PonKeys>,
        on_trigger: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        let wake = Arc::new(eventfd()?);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            triggered: AtomicBool::new(false),
        });
        let thread = {
            let shared = shared.clone();
            let wake = wake.clone();
            thread::Builder::new()
                .name(THREAD_NAME.to_string())
                .spawn(move || run(&shared, &wake, started, pon_keys, on_trigger))?
        };
        tracing::info!(thread = THREAD_NAME, "volume-down break-in watcher spawned");

        Ok(Self {
            shared,
            wake,
            thread: Some(thread),
        })
    }

    pub(crate) fn triggered(&self) -> bool {
        self.shared.triggered.load(Ordering::SeqCst)
    }

    /// Stops the watcher after one last drain of its devices and the PMIC.
    /// The evdev files come back still open: each close waits for an RCU
    /// grace period, which the autoboot path skips by holding them over kexec.
    pub(crate) fn finish(mut self) -> (bool, Vec<File>) {
        self.stop();
        let files = match self.thread.take().map(thread::JoinHandle::join) {
            Some(Ok(files)) => files,
            Some(Err(_)) => {
                tracing::warn!(thread = THREAD_NAME, "break-in watcher thread panicked");
                Vec::new()
            }
            None => Vec::new(),
        };
        (self.triggered(), files)
    }

    fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Err(err) = (&*self.wake).write(&1u64.to_ne_bytes()) {
            tracing::debug!(error = %err, "failed to wake break-in watcher");
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn eventfd() -> io::Result<File> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn run(
    shared: &Shared,
    wake: &File,
    started: Instant,
    pon_keys: Option<PonKeys>,
    on_trigger: impl FnOnce(),
) -> Vec<File> {
    let mut watch = Watch {
        pon_keys,
        devices: Vec::new(),
        seen: Vec::new(),
        next_scan: Instant::now(),
    };
    match watch.run(shared, wake) {
        Some(source) => {
            shared.triggered.store(true, Ordering::SeqCst);
            tracing::info!(
                source = %source,
                elapsed_ms = started.elapsed().as_millis(),
                "volume-down break-in detected"
            );
            on_trigger();
        }
        None => tracing::debug!(thread = THREAD_NAME, "break-in watcher stopped"),
    }
    watch
        .devices
        .into_iter()
        .map(|device| device.file)
        .collect()
}

struct Watch {
    pon_keys: Option<PonKeys>,
    devices: Vec<Device>,
    // Keyed by inode too: a re-registered device can reuse the eventN name.
    seen: Vec<(PathBuf, u64)>,
    next_scan: Instant,
}

impl Watch {
    fn run(&mut self, shared: &Shared, wake: &File) -> Option<String> {
        loop {
            if Instant::now() >= self.next_scan
                && let Some(source) = self.rescan()
            {
                return Some(source);
            }
            if let Some(source) = self.check_pon() {
                return Some(source);
            }

            let mut fds = Vec::with_capacity(self.devices.len() + 1);
            fds.push(pollfd(wake.as_raw_fd()));
            fds.extend(
                self.devices
                    .iter()
                    .map(|device| pollfd(device.file.as_raw_fd())),
            );
            let rc =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    tracing::debug!(error = %err, "break-in watcher poll failed");
                    thread::sleep(Duration::from_millis(POLL_TIMEOUT_MS as u64));
                }
                continue;
            }

            if shared.stop.load(Ordering::SeqCst) {
                return self.drain_all().or_else(|| self.check_pon());
            }

            for index in (0..self.devices.len()).rev() {
                let revents = fds[index + 1].revents;
                if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    self.drop_device(index, "hangup");
                } else if revents & libc::POLLIN != 0
                    && let Some(source) = self.drain_device(index)
                {
                    return Some(source);
                }
            }
        }
    }

    fn rescan(&mut self) -> Option<String> {
        self.next_scan = Instant::now() + RESCAN_INTERVAL;
        let Ok(entries) = fs::read_dir(INPUT) else {
            return None;
        };

        let mut present = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| name.starts_with("event")) else {
                continue;
            };
            let path = entry.path();
            let node = (path.clone(), entry.ino());
            present.push(node.clone());
            if self.seen.contains(&node) {
                continue;
            }
            self.seen.push(node);

            // Never open nodes that cannot report volume-down: opening runs
            // the driver's open() callback, which powers up some touchscreens.
            if !sysfs_event_has_key(name, KEY_VOLUMEDOWN) {
                continue;
            }
            match Device::open(&path) {
                Ok((device, true)) => return Some(device.path.display().to_string()),
                Ok((device, false)) => {
                    tracing::debug!(path = %path.display(), "watching input device for volume-down");
                    self.devices.push(device);
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "ignoring input device");
                }
            }
        }

        self.seen.retain(|node| present.contains(node));
        None
    }

    fn check_pon(&mut self) -> Option<String> {
        let pon_keys = self.pon_keys.as_ref()?;
        match pon_keys.read() {
            Ok(state) if state.volume_down => Some(PMIC_PON_SOURCE.to_string()),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(
                    registers = %pon_keys.registers.display(),
                    error = %err,
                    "PMIC PON key read failed; no longer polling it for break-in"
                );
                self.pon_keys = None;
                None
            }
        }
    }

    fn drain_all(&mut self) -> Option<String> {
        for index in (0..self.devices.len()).rev() {
            if let Some(source) = self.drain_device(index) {
                return Some(source);
            }
        }
        None
    }

    fn drain_device(&mut self, index: usize) -> Option<String> {
        match self.devices[index].drain() {
            Ok(true) => Some(self.devices[index].path.display().to_string()),
            Ok(false) => None,
            Err(err) => {
                self.drop_device(index, &err.to_string());
                None
            }
        }
    }

    fn drop_device(&mut self, index: usize, reason: &str) {
        let device = self.devices.swap_remove(index);
        tracing::debug!(path = %device.path.display(), reason, "dropping break-in input device");
    }
}

struct Device {
    file: File,
    path: PathBuf,
}

impl Device {
    fn open(path: &Path) -> io::Result<(Self, bool)> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let held = volume_down_held(&file)?;
        Ok((
            Self {
                file,
                path: path.to_path_buf(),
            },
            held,
        ))
    }

    fn drain(&mut self) -> io::Result<bool> {
        let mut buffer = [0u8; mem::size_of::<InputEvent>() * 32];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => return Ok(false),
                Ok(bytes) => {
                    let whole_events = bytes / mem::size_of::<InputEvent>();
                    for chunk in buffer[..whole_events * mem::size_of::<InputEvent>()]
                        .chunks_exact(mem::size_of::<InputEvent>())
                    {
                        let event =
                            unsafe { ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
                        if event_requests_break_in(&event) {
                            return Ok(true);
                        }
                        if event_is_syn_dropped(&event) && volume_down_held(&self.file)? {
                            return Ok(true);
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

fn pollfd(fd: libc::c_int) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

fn volume_down_held(file: &File) -> io::Result<bool> {
    let mut keys = [0u8; KEY_BITMAP_BYTES];
    ioctl_read(file.as_raw_fd(), eviocgkey(keys.len()), &mut keys)?;
    Ok(test_bit(&keys, KEY_VOLUMEDOWN))
}

// Any volume-down event counts: pm8941-pwrkey reports a key held since before
// it probed only as a synthetic press+release once the key is let go.
fn event_requests_break_in(event: &InputEvent) -> bool {
    event.type_ == EV_KEY && event.code == KEY_VOLUMEDOWN && matches!(event.value, 0..=2)
}

fn event_is_syn_dropped(event: &InputEvent) -> bool {
    event.type_ == EV_SYN && event.code == SYN_DROPPED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{EV_ABS, KEY_POWER, KEY_VOLUMEUP, SYN_REPORT};

    fn event(type_: u16, code: u16, value: i32) -> InputEvent {
        InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }

    #[test]
    fn volume_down_press_repeat_and_release_request_break_in() {
        for value in [0, 1, 2] {
            assert!(event_requests_break_in(&event(
                EV_KEY,
                KEY_VOLUMEDOWN,
                value
            )));
        }
    }

    #[test]
    fn other_events_do_not_request_break_in() {
        assert!(!event_requests_break_in(&event(EV_KEY, KEY_VOLUMEUP, 1)));
        assert!(!event_requests_break_in(&event(EV_KEY, KEY_POWER, 1)));
        assert!(!event_requests_break_in(&event(EV_SYN, SYN_REPORT, 0)));
        assert!(!event_requests_break_in(&event(EV_ABS, KEY_VOLUMEDOWN, 1)));
    }

    fn pipe_device(events: &[InputEvent]) -> Device {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
            0
        );
        let (reader, mut writer) =
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        for event in events {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    ptr::from_ref(event).cast::<u8>(),
                    mem::size_of::<InputEvent>(),
                )
            };
            writer.write_all(bytes).unwrap();
        }
        Device {
            file: reader,
            path: PathBuf::from("pipe"),
        }
    }

    #[test]
    fn drain_finds_volume_down_among_queued_events() {
        let mut device = pipe_device(&[
            event(EV_KEY, KEY_VOLUMEUP, 1),
            event(EV_SYN, SYN_REPORT, 0),
            event(EV_KEY, KEY_VOLUMEDOWN, 1),
            event(EV_SYN, SYN_REPORT, 0),
        ]);

        assert!(device.drain().unwrap());
    }

    #[test]
    fn drain_without_volume_down_stops_at_would_block() {
        let mut device = pipe_device(&[
            event(EV_KEY, KEY_POWER, 1),
            event(EV_SYN, SYN_REPORT, 0),
            event(EV_KEY, KEY_POWER, 0),
            event(EV_SYN, SYN_REPORT, 0),
        ]);

        assert!(!device.drain().unwrap());
        assert!(!device.drain().unwrap());
    }

    #[test]
    fn detects_syn_dropped() {
        assert!(event_is_syn_dropped(&event(EV_SYN, SYN_DROPPED, 0)));
        assert!(!event_is_syn_dropped(&event(EV_SYN, SYN_REPORT, 0)));
        assert!(!event_is_syn_dropped(&event(EV_KEY, SYN_DROPPED, 0)));
    }
}
