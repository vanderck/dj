//! Keeping the computer awake while dj runs: no screen dimming, display sleep or idle sleep in
//! the middle of a set. On macOS with the system's `caffeinate`, tied to dj's process (it ends
//! with dj, even if dj crashes). Elsewhere it does nothing.

use std::process::Child;

#[derive(Default)]
pub struct KeepAwake {
    child: Option<Child>,
}

/// `caffeinate`'s arguments: keep the display on (-d), the system from idle sleep (-i) and, on
/// mains power, from sleeping at all (-s), for as long as process `pid` runs (-w).
pub fn caffeinate_args(pid: u32) -> Vec<String> {
    vec!["-dis".into(), "-w".into(), pid.to_string()]
}

impl KeepAwake {
    /// Keep it awake or not (does nothing when already so).
    pub fn set(&mut self, on: bool) {
        // A `caffeinate` that ended by itself (killed from outside) is started again.
        if let Some(c) = self.child.as_mut() {
            if !matches!(c.try_wait(), Ok(None)) {
                self.child = None;
            }
        }
        match (on, self.child.is_some()) {
            (true, false) => self.child = start(),
            (false, true) => self.stop(),
            _ => {}
        }
    }

    pub fn is_on(&self) -> bool {
        self.child.is_some()
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
fn start() -> Option<Child> {
    std::process::Command::new("/usr/bin/caffeinate")
        .args(caffeinate_args(std::process::id()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("keep awake: {e}"))
        .ok()
}

#[cfg(not(target_os = "macos"))]
fn start() -> Option<Child> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caffeinate_keeps_display_and_system_awake_while_dj_runs() {
        assert_eq!(caffeinate_args(4242), vec!["-dis", "-w", "4242"]);
        // Off the Mac it's a no-op, and turning it off is always fine.
        let mut k = KeepAwake::default();
        k.set(true);
        assert_eq!(k.is_on(), cfg!(target_os = "macos"));
        k.set(false);
        assert!(!k.is_on());
    }
}
