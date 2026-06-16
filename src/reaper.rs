use std::{
    io,
    sync::{
        Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

static STARTED: AtomicBool = AtomicBool::new(false);
static CHILD_WAIT_LOCK: Mutex<()> = Mutex::new(());

const REAP_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn spawn() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    match thread::Builder::new()
        .name("pocketboot-reaper".to_string())
        .spawn(run)
    {
        Ok(_thread) => tracing::info!(thread = "pocketboot-reaper", "PID 1 reaper thread spawned"),
        Err(err) => tracing::error!(error = ?err, "failed to spawn PID 1 reaper thread"),
    }
}

pub(crate) fn child_guard() -> ChildGuard {
    let lock = CHILD_WAIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ChildGuard { _lock: lock }
}

pub(crate) struct ChildGuard {
    _lock: MutexGuard<'static, ()>,
}

fn run() {
    loop {
        match CHILD_WAIT_LOCK.try_lock() {
            Ok(_lock) => match reap_available_children() {
                Ok(reaped) if reaped > 0 => tracing::debug!(reaped, "reaped child processes"),
                Ok(_) => {}
                Err(err) => tracing::warn!(error = ?err, "PID 1 child reaping failed"),
            },
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Poisoned(poisoned)) => {
                let _lock = poisoned.into_inner();
                match reap_available_children() {
                    Ok(reaped) if reaped > 0 => tracing::debug!(reaped, "reaped child processes"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(error = ?err, "PID 1 child reaping failed"),
                }
            }
        }
        thread::sleep(REAP_INTERVAL);
    }
}

fn reap_available_children() -> io::Result<usize> {
    let mut reaped = 0;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            reaped += 1;
            tracing::debug!(pid, status, "reaped child process");
            continue;
        }
        if pid == 0 {
            return Ok(reaped);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ECHILD) {
            return Ok(reaped);
        }
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}
