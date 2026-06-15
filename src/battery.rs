use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

const POWER_SUPPLY: &str = "/sys/class/power_supply";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) type Updates = mpsc::Receiver<Option<Snapshot>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) percent: u8,
    pub(crate) status: Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Unknown,
    Charging,
    Discharging,
    NotCharging,
    Full,
}

pub(crate) fn spawn() -> io::Result<Updates> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("pocketboot-battery".to_string())
        .spawn(move || watch(tx))?;
    tracing::info!(
        thread = "pocketboot-battery",
        "battery watcher thread spawned"
    );
    Ok(rx)
}

fn watch(tx: mpsc::Sender<Option<Snapshot>>) {
    let mut last = None;
    loop {
        let current = read_snapshot();
        if last != Some(current) {
            match current {
                Some(snapshot) => tracing::info!(
                    percent = snapshot.percent,
                    status = ?snapshot.status,
                    "battery state updated"
                ),
                None if last.is_some() => tracing::info!("battery state unavailable"),
                None => {}
            }

            if tx.send(current).is_err() {
                tracing::debug!("battery update receiver disconnected");
                return;
            }
            last = Some(current);
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn read_snapshot() -> Option<Snapshot> {
    let entries = fs::read_dir(POWER_SUPPLY).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_battery(&path) || !is_present(&path) {
            continue;
        }

        let Some(percent) = read_percent(&path) else {
            tracing::debug!(path = %path.display(), "battery has no readable charge level");
            continue;
        };

        return Some(Snapshot {
            percent,
            status: read_status(&path),
        });
    }
    None
}

fn is_battery(path: &Path) -> bool {
    read_trimmed(path.join("type"))
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("Battery"))
}

fn is_present(path: &Path) -> bool {
    read_trimmed(path.join("present"))
        .as_deref()
        .map(|present| present != "0")
        .unwrap_or(true)
}

fn read_percent(path: &Path) -> Option<u8> {
    read_i64(path.join("capacity"))
        .map(percent_from_i64)
        .or_else(|| read_ratio_percent(path, "charge_now", &["charge_full", "charge_full_design"]))
        .or_else(|| read_ratio_percent(path, "energy_now", &["energy_full", "energy_full_design"]))
}

fn read_ratio_percent(path: &Path, now_name: &str, full_names: &[&str]) -> Option<u8> {
    let now = read_i64(path.join(now_name))?;
    let full = full_names
        .iter()
        .find_map(|name| read_i64(path.join(name)))?;
    if now < 0 || full <= 0 {
        return None;
    }
    let percent = (now.saturating_mul(100).saturating_add(full / 2)) / full;
    Some(percent_from_i64(percent))
}

fn percent_from_i64(value: i64) -> u8 {
    value.clamp(0, 100) as u8
}

fn read_status(path: &Path) -> Status {
    match read_trimmed(path.join("status"))
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "charging" => Status::Charging,
        "discharging" => Status::Discharging,
        "not charging" => Status::NotCharging,
        "full" => Status::Full,
        _ => Status::Unknown,
    }
}

fn read_i64(path: PathBuf) -> Option<i64> {
    read_trimmed(path)?.parse().ok()
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}
