//! 電源の状態 (04 §4 の電池の設定、07 §4.2 の `memory.on_battery`)。C を含む依存を使わずに OS から得る
//! (ADR-0015)。分からないときは `Unknown` (電源に繋がっているものとして扱う)。

/// 電源の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    /// 電源に繋がっている。
    Ac,
    /// 電池で動いている。
    Battery,
    /// 分からない (電池の無い機械、取れない OS)。
    Unknown,
}

/// OS に問い合わせる。
pub fn system() -> Power {
    imp::power()
}

#[cfg(windows)]
mod imp {
    use super::Power;

    /// `SYSTEM_POWER_STATUS` (winbase.h)。
    #[repr(C)]
    #[derive(Default)]
    struct SystemPowerStatus {
        ac_line_status: u8,
        battery_flag: u8,
        battery_life_percent: u8,
        system_status_flag: u8,
        battery_life_time: u32,
        battery_full_life_time: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }

    pub fn power() -> Power {
        let mut s = SystemPowerStatus::default();
        // SAFETY: `s` は呼び出しの間だけ有効な、正しい配置の構造体。関数はそれを書くだけ。
        let ok = unsafe { GetSystemPowerStatus(&mut s) };
        if ok == 0 {
            return Power::Unknown;
        }
        // battery_flag の 128 は「電池が無い」。
        match s.ac_line_status {
            0 if s.battery_flag & 128 == 0 => Power::Battery,
            1 => Power::Ac,
            _ => Power::Unknown,
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Power;

    /// `/sys/class/power_supply` の電源 (Mains・USB) が 1 つでも繋がっていれば Ac。そうでなく、機械の
    /// 電池 (周辺機器の電池 `scope=Device` を除く) があれば Battery。どちらも無ければ Unknown
    /// (電池の無い機械で、繋がっていない USB-C の口だけが見えるときに、電池と誤らないため)。
    pub fn power() -> Power {
        let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else {
            return Power::Unknown;
        };
        let read = |p: &std::path::Path, f: &str| {
            std::fs::read_to_string(p.join(f))
                .map(|s| s.trim().to_owned())
                .unwrap_or_default()
        };
        let (mut online, mut battery) = (false, false);
        for e in dir.filter_map(Result::ok) {
            let p = e.path();
            if read(&p, "scope") == "Device" {
                continue;
            }
            match read(&p, "type").as_str() {
                "Mains" | "USB" => online |= read(&p, "online") == "1",
                "Battery" => battery = true,
                _ => {}
            }
        }
        if online {
            Power::Ac
        } else if battery {
            Power::Battery
        } else {
            Power::Unknown
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::Power;

    pub fn power() -> Power {
        Power::Unknown
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_system_answers_without_panicking() {
        let _ = super::system();
    }
}
