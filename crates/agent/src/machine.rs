//! What the computer itself is doing: whether it runs in a virtual machine or a remote session, screen lock and sleep.
//! All read by polling, so kairosd needs no window or run loop for them. Sleep is the gap between a clock that runs
//! while the computer sleeps and one that doesn't, so a change to the system clock never looks like it.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use kairos_core::event::{Event, Kind};

use crate::{now, Msg};

const POLL: Duration = Duration::from_secs(10);
/// Less than this between the two clocks isn't worth a line.
const MIN_SLEEP_SECS: f64 = 30.0;
/// Readings in a row before a change counts, so a UAC prompt or a failed reading doesn't make one.
const HOLD: u32 = 3;

pub fn spawn(tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut locked = lock_reader();
        let (mut screen, mut session) = (Steady::default(), Steady::default());
        let vm = hypervisor().map(str::to_string);
        if tx.send(Msg::Event(Event::new(now(), None, Kind::Machine { vm: vm.clone(), remote: remote() }))).is_err() {
            return;
        }
        let mut last = clocks();
        loop {
            thread::sleep(POLL);
            let t = now();
            let next = clocks();
            if let (Some((awake0, total0)), Some((awake, total))) = (last, next) {
                let slept = (total - total0) - (awake - awake0);
                if slept >= MIN_SLEEP_SECS && tx.send(Msg::Event(Event::new(t, None, Kind::Slept { secs: slept.round() }))).is_err() {
                    return;
                }
            }
            last = next;
            if let Some((now_locked, since)) = screen.observe(locked(), t) {
                if tx.send(Msg::Event(Event::new(since, None, Kind::Screen { locked: now_locked }))).is_err() {
                    return;
                }
            }
            if let Some((now_remote, since)) = session.observe(remote(), t) {
                if tx.send(Msg::Event(Event::new(since, None, Kind::Machine { vm: vm.clone(), remote: Some(now_remote) }))).is_err() {
                    return;
                }
            }
        }
    });
}

#[derive(Default)]
struct Steady {
    shown: Option<bool>,
    seen: Option<(bool, f64, u32)>,
}

impl Steady {
    /// A new state and when it was first read, once it has held for HOLD readings. The state kairosd starts in isn't a
    /// change, and a reading that failed changes nothing.
    fn observe(&mut self, reading: Option<bool>, at: f64) -> Option<(bool, f64)> {
        let r = reading?;
        match &mut self.seen {
            Some((s, _, n)) if *s == r => *n += 1,
            _ => self.seen = Some((r, at, 1)),
        }
        let (state, since, n) = self.seen?;
        if n < HOLD || self.shown == Some(state) {
            return None;
        }
        let first = self.shown.replace(state).is_none();
        (!first).then_some((state, since))
    }
}

/// The hypervisor's name, from a known list, "none" on real hardware, or None where it can't be read.
#[cfg(target_arch = "x86_64")]
fn hypervisor() -> Option<&'static str> {
    use std::arch::x86_64::__cpuid;
    #[allow(unused_unsafe)]
    // SAFETY: cpuid is on every x86_64 processor, and leaves a processor doesn't have read as zeros.
    let (cpu, top) = unsafe { (__cpuid(1), __cpuid(0x4000_0000)) };
    if cpu.ecx & (1 << 31) == 0 {
        return Some("none");
    }
    let mut id = [0u8; 12];
    for (i, r) in [top.ebx, top.ecx, top.edx].into_iter().enumerate() {
        id[i * 4..i * 4 + 4].copy_from_slice(&r.to_le_bytes());
    }
    Some(match &id {
        b"Microsoft Hv" => {
            // Windows with virtualization-based security runs as Hyper-V's root partition on real hardware. Only the
            // root may create partitions.
            #[allow(unused_unsafe)]
            // SAFETY: as above, and the leaf is in the range the hypervisor reports.
            let root = top.eax >= 0x4000_0003 && unsafe { __cpuid(0x4000_0003) }.ebx & 1 != 0;
            if root { "none" } else { "hyperv" }
        }
        b"VMwareVMware" => "vmware",
        b"KVMKVMKVM\0\0\0" | b"Linux KVM Hv" => "kvm",
        b"VBoxVBoxVBox" => "virtualbox",
        b"XenVMMXenVMM" => "xen",
        b"prl hyperv  " | b" lrpepyh  vr" => "parallels",
        b"TCGTCGTCGTCG" => "qemu",
        b"bhyve bhyve " => "bhyve",
        _ => "other",
    })
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn hypervisor() -> Option<&'static str> {
    let (mut v, mut len) = (0i32, std::mem::size_of::<i32>());
    // SAFETY: the sysctl writes one int into v.
    let ok = unsafe { libc::sysctlbyname(c"kern.hv_vmm_present".as_ptr(), (&mut v as *mut i32).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    ok.then_some(if v != 0 { "other" } else { "none" })
}

#[cfg(not(any(target_arch = "x86_64", all(target_os = "macos", target_arch = "aarch64"))))]
fn hypervisor() -> Option<&'static str> {
    None
}

#[cfg(windows)]
fn remote() -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    // SAFETY: reads a system metric.
    Some(unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0)
}

#[cfg(not(windows))]
fn remote() -> Option<bool> {
    None
}

#[cfg(unix)]
fn clock(id: libc::clockid_t) -> Option<f64> {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes one timespec.
    (unsafe { libc::clock_gettime(id, &mut t) } == 0).then(|| t.tv_sec as f64 + t.tv_nsec as f64 / 1e9)
}

/// Seconds the computer has been awake and seconds it has been on, from the same start.
#[cfg(target_os = "linux")]
fn clocks() -> Option<(f64, f64)> {
    Some((clock(libc::CLOCK_MONOTONIC)?, clock(libc::CLOCK_BOOTTIME)?))
}

#[cfg(target_os = "macos")]
fn clocks() -> Option<(f64, f64)> {
    Some((clock(libc::CLOCK_UPTIME_RAW)?, clock(libc::CLOCK_MONOTONIC)?))
}

#[cfg(windows)]
fn clocks() -> Option<(f64, f64)> {
    use windows_sys::Win32::System::WindowsProgramming::{QueryInterruptTime, QueryUnbiasedInterruptTime};
    let (mut awake, mut total) = (0u64, 0u64);
    // SAFETY: each writes one u64, in 100 ns units.
    let ok = unsafe { QueryUnbiasedInterruptTime(&mut awake) } != 0;
    unsafe { QueryInterruptTime(&mut total) };
    ok.then(|| (awake as f64 / 1e7, total as f64 / 1e7))
}

/// logind's LockedHint for the person's session. Desktops that don't set it never read as locked.
#[cfg(target_os = "linux")]
fn lock_reader() -> impl FnMut() -> Option<bool> {
    let session = || {
        let conn = zbus::blocking::Connection::system().ok()?;
        zbus::blocking::proxy::Builder::<zbus::blocking::Proxy>::new(&conn)
            .destination("org.freedesktop.login1")
            .ok()?
            .path("/org/freedesktop/login1/session/auto")
            .ok()?
            .interface("org.freedesktop.login1.Session")
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    };
    let proxy = session();
    move || proxy.as_ref()?.get_property::<bool>("LockedHint").ok()
}

#[cfg(target_os = "macos")]
fn lock_reader() -> impl FnMut() -> Option<bool> {
    use std::ffi::{c_char, c_void};

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSessionCopyCurrentDictionary() -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(b: *const c_void) -> u8;
        fn CFRelease(cf: *const c_void);
    }
    const UTF8: u32 = 0x0800_0100;
    // SAFETY: a static NUL-terminated string. The key is kept for the life of the thread.
    let key = unsafe { CFStringCreateWithCString(std::ptr::null(), c"CGSSessionScreenIsLocked".as_ptr(), UTF8) } as usize;
    move || {
        let key = key as *const c_void;
        if key.is_null() {
            return None;
        }
        // SAFETY: the dictionary is ours to release; the value belongs to it and is read before the release.
        unsafe {
            let dict = CGSessionCopyCurrentDictionary();
            if dict.is_null() {
                return None;
            }
            let v = CFDictionaryGetValue(dict, key);
            let locked = !v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID() && CFBooleanGetValue(v) != 0;
            CFRelease(dict);
            Some(locked)
        }
    }
}

/// Windows shows the lock screen (and UAC prompts) on a desktop other than Default, which a user's process can't open.
#[cfg(windows)]
fn lock_reader() -> impl FnMut() -> Option<bool> {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME};
    || {
        // SAFETY: the desktop handle is closed before returning; the name buffer is ours.
        unsafe {
            let desk = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
            if desk.is_null() {
                return Some(true);
            }
            let mut name = [0u16; 64];
            let mut needed = 0u32;
            let ok = GetUserObjectInformationW(desk, UOI_NAME, name.as_mut_ptr().cast(), std::mem::size_of_val(&name) as u32, &mut needed) != 0;
            CloseDesktop(desk);
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            ok.then(|| String::from_utf16_lossy(&name[..len]) != "Default")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_counts_once_it_holds_and_the_starting_state_is_not_a_change() {
        let mut s = Steady::default();
        let mut read = |r: Option<bool>, at: f64| s.observe(r, at);
        assert_eq!((read(Some(false), 0.0), read(Some(false), 10.0), read(Some(false), 20.0)), (None, None, None), "how it started");
        assert_eq!((read(Some(true), 30.0), read(Some(false), 40.0)), (None, None), "a short UAC prompt");
        assert_eq!((read(Some(true), 50.0), read(None, 60.0), read(Some(true), 70.0)), (None, None, None), "a failed reading changes nothing");
        assert_eq!(read(Some(true), 80.0), Some((true, 50.0)), "locked since it was first read");
        assert_eq!((read(Some(true), 90.0), read(Some(false), 100.0), read(Some(false), 110.0)), (None, None, None));
        assert_eq!(read(Some(false), 120.0), Some((false, 100.0)));
    }

    #[test]
    fn the_clocks_read_and_the_awake_one_never_runs_ahead() {
        let (awake, total) = clocks().unwrap();
        assert!(awake > 0.0 && total + 1.0 >= awake, "{awake} {total}");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_hypervisor_reads_as_a_known_name() {
        let vm = hypervisor().unwrap();
        assert!(["none", "hyperv", "vmware", "kvm", "virtualbox", "xen", "parallels", "qemu", "bhyve", "other"].contains(&vm), "{vm}");
    }
}
