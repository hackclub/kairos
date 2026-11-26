use std::fs::File;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

pub const ENTER: &str = "\r";
pub const DOWN: &str = "\x1b[B";
pub const ESC: &str = "\x1b";
const ROWS: NonZeroU16 = NonZeroU16::new(40).unwrap();
const COLS: NonZeroU16 = NonZeroU16::new(120).unwrap();

/// Reads what is typed into this process's own window and drops it. The agents get their keys through their ptys;
/// this is what makes the window's terminal read typing, the way it does for a person.
pub fn drain_stdin() {
    if cfg!(unix) {
        thread::spawn(|| {
            let mut buf = [0u8; 256];
            while let Ok(1..) = std::io::stdin().read(&mut buf) {}
        });
    }
}

pub fn size() -> PtySize {
    PtySize { rows: ROWS.get(), cols: COLS.get(), pixel_width: 0, pixel_height: 0 }
}

/// What a real terminal sends back when a program asks about it (cursor position, colours, device type). Codex asks
/// all of these at startup, and a person's terminal always answers.
fn answers(out: &[u8], (row, col): (u16, u16)) -> String {
    let asks = |q: &[u8]| out.windows(q.len()).any(|w| w == q);
    let mut reply = String::new();
    if asks(b"\x1b[6n") {
        reply += &format!("\x1b[{};{}R", row + 1, col + 1);
    }
    if asks(b"\x1b]10;?") {
        reply += "\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
    }
    if asks(b"\x1b]11;?") {
        reply += "\x1b]11;rgb:0000/0000/0000\x1b\\";
    }
    if asks(b"\x1b[c") || asks(b"\x1b[0c") {
        reply += "\x1b[?1;2c";
    }
    reply
}

enum Proc {
    Local(Box<dyn Child + Send + Sync>),
    /// In a pty another lab process holds in its own terminal window, over a socket that closes when the program ends.
    Remote(Arc<AtomicBool>, TcpStream),
}

/// A program running in a pty, with the screen a person would see.
pub struct Term {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    child: Proc,
}

impl Term {
    /// `raw_path` gets every byte the program wrote and a `.screens` file next to it gets each new screen with its time,
    /// for when the last screen alone does not explain a run.
    pub fn spawn(cmd: CommandBuilder, raw_path: &Path) -> Result<Term> {
        let pair = native_pty_system().openpty(size()).map_err(|e| anyhow!("{e}"))?;
        let child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("{e}"))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| anyhow!("{e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("{e}"))?;
        // The master has to outlive the reader, or the child gets a hangup.
        Term::wire(reader, writer, Box::new(pair.master), Proc::Local(child), raw_path)
    }

    /// A program the lab started in another terminal window, through `stream`.
    pub fn remote(stream: TcpStream, raw_path: &Path) -> Result<Term> {
        let ended = Arc::new(AtomicBool::new(false));
        let reader = Ended { stream: stream.try_clone()?, ended: ended.clone() };
        Term::wire(Box::new(reader), Box::new(stream.try_clone()?), Box::new(()), Proc::Remote(ended, stream), raw_path)
    }

    fn wire(
        mut reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        keep: Box<dyn std::any::Any + Send>,
        child: Proc,
        raw_path: &Path,
    ) -> Result<Term> {
        let mut raw = File::create(raw_path)?;
        let writer = Arc::new(Mutex::new(writer));
        let answer = writer.clone();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let seen = parser.clone();
        thread::spawn(move || {
            let _keep = keep;
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = raw.write_all(&buf[..n]);
                let cursor = {
                    let mut parser = seen.lock().unwrap();
                    parser.process(&buf[..n]);
                    parser.screen().cursor_position()
                };
                let reply = answers(&buf[..n], cursor);
                if !reply.is_empty() {
                    let mut w = answer.lock().unwrap();
                    let _ = w.write_all(reply.as_bytes());
                    let _ = w.flush();
                }
            }
        });
        let (shown, mut log, started) = (parser.clone(), File::create(raw_path.with_extension("screens"))?, Instant::now());
        thread::spawn(move || {
            let mut last = String::new();
            loop {
                thread::sleep(Duration::from_millis(250));
                let now = shown.lock().unwrap().screen().contents();
                if now != last {
                    let lines: Vec<&str> = now.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
                    if writeln!(log, "=== {:.1} s\n{}", started.elapsed().as_secs_f32(), lines.join("\n")).is_err() {
                        return;
                    }
                    last = now;
                }
            }
        });
        Ok(Term { writer, parser, child })
    }

    pub fn screen(&self) -> String {
        let text = self.parser.lock().unwrap().screen().contents();
        text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
    }

    pub fn send(&mut self, keys: &str) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(keys.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// One key at a time, the way a person types, so the harness never takes it for a paste.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        let mut buf = [0u8; 4];
        for c in text.chars() {
            self.send(c.encode_utf8(&mut buf))?;
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    /// What a terminal sends for a paste: wrapped in markers when the program asked for them, with lines ended by Enter.
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let marked = self.parser.lock().unwrap().screen().bracketed_paste();
        self.send(&if marked { format!("\x1b[200~{text}\x1b[201~") } else { text })
    }

    pub fn exited(&mut self) -> bool {
        match &mut self.child {
            Proc::Local(child) => matches!(child.try_wait(), Ok(Some(_))),
            Proc::Remote(ended, _) => ended.load(Ordering::Relaxed),
        }
    }

    pub fn kill(&mut self) {
        match &mut self.child {
            Proc::Local(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            // The other window ends its program when the socket closes.
            Proc::Remote(ended, stream) => {
                let _ = stream.shutdown(Shutdown::Both);
                let until = Instant::now() + Duration::from_secs(5);
                while !ended.load(Ordering::Relaxed) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

struct Ended {
    stream: TcpStream,
    ended: Arc<AtomicBool>,
}

impl Read for Ended {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.stream.read(buf);
        if !matches!(n, Ok(1..)) {
            self.ended.store(true, Ordering::Relaxed);
        }
        n
    }
}
