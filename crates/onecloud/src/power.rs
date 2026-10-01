//! Staying out of the way: low CPU and I/O priority, and pausing on a low
//! battery.

use std::{fs, path::Path};

/// Lower this process's CPU priority (nice 10) and move its I/O to the idle
/// class, so syncing never competes with what the user is doing.
pub fn be_nice() {
    // SAFETY: plain syscalls on the calling process, no pointers involved
    unsafe {
        _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        // ioprio_set(IOPRIO_WHO_PROCESS, self, IOPRIO_CLASS_IDLE << 13)
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        _ = libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
}

/// Battery state, if the machine has a battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub discharging: bool,
}

/// Read the first battery under `/sys/class/power_supply`.
#[must_use]
pub fn battery() -> Option<Battery> {
    battery_in(Path::new("/sys/class/power_supply"))
}

fn battery_in(root: &Path) -> Option<Battery> {
    let read = |dir: &Path, f: &str| {
        fs::read_to_string(dir.join(f))
            .ok()
            .map(|s| s.trim().to_string())
    };
    fs::read_dir(root).ok()?.flatten().find_map(|e| {
        let dir = e.path();
        if read(&dir, "type")? != "Battery" {
            return None;
        }
        Some(Battery {
            percent: read(&dir, "capacity")?.parse().ok()?,
            discharging: read(&dir, "status")? == "Discharging",
        })
    })
}

/// Whether to hold off syncing: on battery and below `threshold` percent.
/// A threshold of 0 never pauses.
#[must_use]
pub fn should_pause(battery: Option<Battery>, threshold: u8) -> bool {
    battery.is_some_and(|b| b.discharging && b.percent < threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sysfs_and_decides() {
        let root = tempfile::tempdir().unwrap();
        let ac = root.path().join("AC");
        let bat = root.path().join("BAT0");
        fs::create_dir_all(&ac).unwrap();
        fs::create_dir_all(&bat).unwrap();
        fs::write(ac.join("type"), "Mains\n").unwrap();
        fs::write(bat.join("type"), "Battery\n").unwrap();
        fs::write(bat.join("capacity"), "15\n").unwrap();
        fs::write(bat.join("status"), "Discharging\n").unwrap();
        let b = battery_in(root.path());
        assert_eq!(
            b,
            Some(Battery {
                percent: 15,
                discharging: true
            })
        );
        assert!(should_pause(b, 20));
        assert!(!should_pause(b, 10));
        assert!(!should_pause(b, 0));
        fs::write(bat.join("status"), "Charging\n").unwrap();
        assert!(!should_pause(battery_in(root.path()), 20));
        assert!(!should_pause(None, 20)); // desktops never pause
    }
}
