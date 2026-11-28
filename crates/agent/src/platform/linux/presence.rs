use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use super::backend::{running, Running};
use crate::platform::InputWatch;
use crate::{now, Msg};

pub const IDLE_AFTER: Duration = Duration::from_secs(60);

struct IdleState {
    threshold: Duration,
    idle: bool,
}

impl IdleState {
    fn observe(&mut self, idle_for: Duration) -> Option<Msg> {
        let now_idle = idle_for >= self.threshold;
        if now_idle == self.idle {
            return None;
        }
        self.idle = now_idle;
        Some(if now_idle { Msg::Idled } else { Msg::Resumed })
    }
}

/// One idle reading: how long since any input and, from a backend that can tell, since the last key press and the
/// last input a program made. A kind the first reading has is reported from then on; a later reading may miss it.
#[derive(Clone, Copy)]
pub struct Since {
    pub input: Duration,
    pub keys: Option<Duration>,
    pub injected: Option<Duration>,
}

#[derive(Default)]
struct Watches {
    input: InputWatch,
    keys: InputWatch,
    injected: InputWatch,
}

impl Watches {
    /// The new inputs a reading shows, as messages.
    fn observe(&mut self, at: f64, since: Since) -> [Option<Msg>; 3] {
        let input = self.input.observe(at, since.input.as_secs_f64()).map(Msg::Input);
        let keys = since.keys.and_then(|d| self.keys.observe(at, d.as_secs_f64())).map(Msg::Keys);
        let injected = since.injected.and_then(|d| self.injected.observe(at, d.as_secs_f64())).map(Msg::Injected);
        [input, keys, injected]
    }
}

pub fn poll(
    name: &'static str,
    threshold: Duration,
    tx: Sender<Msg>,
    mut idle_time: impl FnMut() -> Result<Duration> + Send + 'static,
) -> Result<Running> {
    poll_since(name, threshold, tx, move || idle_time().map(|input| Since { input, keys: None, injected: None }))
}

pub fn poll_since(
    name: &'static str,
    threshold: Duration,
    tx: Sender<Msg>,
    mut read: impl FnMut() -> Result<Since> + Send + 'static,
) -> Result<Running> {
    let mut state = IdleState { threshold, idle: false };
    let mut watches = Watches::default();
    let first = read()?;
    let at = now();
    let (handle, life) = running(name);
    let kinds = (first.keys.is_some() || first.injected.is_some())
        .then_some(Msg::InputKinds { keys: first.keys.is_some(), injected: first.injected.is_some() });
    let mut opening = [Msg::PresenceReady, Msg::InputReady].into_iter().chain(kinds).chain(watches.observe(at, first).into_iter().flatten());
    if opening.any(|m| tx.send(m).is_err()) || state.observe(first.input).is_some_and(|m| tx.send(m).is_err()) {
        return Ok(handle);
    }
    thread::spawn(move || {
        let _life = life;
        loop {
            thread::sleep(InputWatch::poll_every(state.idle));
            let Ok(since) = read() else { return };
            if watches.observe(now(), since).into_iter().flatten().any(|m| tx.send(m).is_err()) {
                return;
            }
            if state.observe(since.input).is_some_and(|m| tx.send(m).is_err()) {
                return;
            }
        }
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn next_state(rx: &mpsc::Receiver<Msg>) -> Msg {
        loop {
            match rx.recv_timeout(secs(5)).unwrap() {
                Msg::Input(_) => {}
                m => return m,
            }
        }
    }

    #[test]
    fn idles_at_the_threshold_and_resumes_on_input() {
        let mut s = IdleState { threshold: secs(60), idle: false };
        assert!(s.observe(secs(0)).is_none());
        assert!(s.observe(secs(59)).is_none());
        assert!(matches!(s.observe(secs(60)), Some(Msg::Idled)));
        assert!(s.observe(secs(300)).is_none());
        assert!(matches!(s.observe(secs(0)), Some(Msg::Resumed)));
        assert!(s.observe(secs(1)).is_none());
    }

    #[test]
    fn reports_the_time_of_each_new_input_from_the_idle_readings() {
        let (tx, rx) = mpsc::channel();
        let mut readings = vec![Ok(secs(0)), Ok(secs(1)), Ok(secs(0))];
        readings.reverse();
        poll("test", secs(60), tx, move || readings.pop().unwrap_or_else(|| Err(anyhow::anyhow!("gone")))).unwrap();
        let before = crate::now();
        let inputs: Vec<f64> = rx.iter().filter_map(|m| if let Msg::Input(t) = m { Some(t) } else { None }).collect();
        assert_eq!(inputs.len(), 2, "{inputs:?}");
        let two_polls = 2.0 * InputWatch::poll_every(false).as_secs_f64();
        assert!(inputs[0] <= before && inputs[1] > inputs[0] + two_polls - 0.05, "{inputs:?} {before}");
    }

    #[test]
    fn a_backend_that_tells_keys_and_injected_input_apart_announces_and_reports_them() {
        let (tx, rx) = mpsc::channel();
        let since = |input, keys, injected: Option<u64>| -> Result<Since> {
            Ok(Since { input: secs(input), keys: Some(secs(keys)), injected: injected.map(secs) })
        };
        let mut readings = vec![since(0, 0, Some(100)), since(0, 3, Some(0)), since(1, 1, None)];
        readings.reverse();
        poll_since("test", secs(60), tx, move || readings.pop().unwrap_or_else(|| Err(anyhow::anyhow!("gone")))).unwrap();
        let msgs: Vec<Msg> = rx.iter().collect();
        assert!(matches!(msgs[..3], [Msg::PresenceReady, Msg::InputReady, Msg::InputKinds { keys: true, injected: true }]));
        let count = |f: fn(&Msg) -> bool| msgs.iter().filter(|m| f(m)).count();
        assert_eq!(count(|m| matches!(m, Msg::Input(_))), 2);
        assert_eq!(count(|m| matches!(m, Msg::Keys(_))), 1, "the second reading shows no newer key press");
        assert_eq!(count(|m| matches!(m, Msg::Injected(_))), 2, "a reading without the kind skips it");
    }

    #[test]
    fn an_unreadable_source_is_an_error_not_presence() {
        let (tx, rx) = mpsc::channel();
        assert!(poll("test", secs(60), tx, || Err(anyhow::anyhow!("no"))).is_err());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn announces_presence_then_follows_the_idle_time_until_the_source_fails() {
        let (tx, rx) = mpsc::channel();
        let mut readings = vec![Ok(secs(0)), Ok(secs(5)), Ok(secs(0))];
        readings.reverse();
        let run = poll("test", secs(2), tx, move || readings.pop().unwrap_or_else(|| Err(anyhow::anyhow!("gone")))).unwrap();
        assert!(matches!(rx.recv().unwrap(), Msg::PresenceReady));
        assert!(matches!(rx.recv().unwrap(), Msg::InputReady));
        assert!(matches!(next_state(&rx), Msg::Idled));
        assert!(matches!(next_state(&rx), Msg::Resumed));
        while run.is_alive() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}
