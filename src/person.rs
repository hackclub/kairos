use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;

/// The person at the keyboard. Typing into the harness's pty is invisible to the system, so while the person reads or
/// types, the pointer moves a pixel and back every half second, which the system counts as input the way it does a
/// person's.
#[derive(Clone)]
pub struct Person {
    on: bool,
    busy: Arc<AtomicBool>,
    log: Arc<Mutex<Log>>,
}

#[derive(Default)]
struct Log {
    inputs: Vec<f64>,
    busy: Vec<(f64, f64)>,
}

impl Person {
    /// Off (no window to work in), it never touches anything.
    pub fn new(on: bool) -> Result<Person> {
        let person = Person { on, busy: Arc::default(), log: Arc::default() };
        if on {
            nudge()?;
            let p = person.clone();
            thread::spawn(move || loop {
                if p.busy.load(Ordering::Relaxed) {
                    p.press();
                }
                thread::sleep(Duration::from_millis(500));
            });
        }
        Ok(person)
    }

    /// Reading or typing, as opposed to waiting on the agent with hands off.
    pub fn busy(&self, yes: bool) {
        if !self.on || self.busy.swap(yes, Ordering::Relaxed) == yes {
            return;
        }
        let now = crate::now();
        let mut log = self.log.lock().unwrap();
        if yes {
            log.busy.push((now, f64::MAX));
        } else if let Some(last) = log.busy.last_mut() {
            last.1 = now;
        }
        drop(log);
        if yes {
            self.press();
        }
    }

    /// One key press, or the like.
    pub fn press(&self) {
        if self.on && nudge().is_ok() {
            self.log.lock().unwrap().inputs.push(crate::now());
        }
    }

    pub fn inputs(&self) -> Vec<f64> {
        self.log.lock().unwrap().inputs.clone()
    }

    pub fn busy_times(&self) -> Vec<(f64, f64)> {
        let now = crate::now();
        self.log.lock().unwrap().busy.iter().map(|&(a, b)| (a, b.min(now))).collect()
    }
}

#[cfg(target_os = "linux")]
fn nudge() -> Result<()> {
    let ok = std::process::Command::new("xdotool")
        .args(["mousemove_relative", "--", "1", "0", "mousemove_relative", "--", "-1", "0"])
        .status()
        .map_err(|e| anyhow::anyhow!("xdotool: {e}"))?
        .success();
    anyhow::ensure!(ok, "xdotool could not move the pointer");
    Ok(())
}

#[cfg(target_os = "macos")]
fn nudge() -> Result<()> {
    use std::ffi::c_void;
    use std::ptr::null;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, at: CGPoint, button: u32) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }
    const MOUSE_MOVED: u32 = 5;
    const HID_TAP: u32 = 0;
    unsafe {
        let here = CGEventCreate(null());
        anyhow::ensure!(!here.is_null(), "CGEventCreate failed");
        let at = CGEventGetLocation(here);
        CFRelease(here);
        let moved = CGEventCreateMouseEvent(null(), MOUSE_MOVED, at, 0);
        anyhow::ensure!(!moved.is_null(), "CGEventCreateMouseEvent failed");
        CGEventPost(HID_TAP, moved);
        CFRelease(moved);
    }
    Ok(())
}

#[cfg(windows)]
fn nudge() -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
    let step = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
    };
    let moves = [step(1), step(-1)];
    let sent = unsafe { SendInput(moves.len() as u32, moves.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    anyhow::ensure!(sent == moves.len() as u32, "SendInput sent {sent} of {}", moves.len());
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn nudge() -> Result<()> {
    anyhow::bail!("the lab can't make input on this system")
}
