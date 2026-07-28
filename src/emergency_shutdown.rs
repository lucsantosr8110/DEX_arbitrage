// ============================================================
// src/emergency_shutdown.rs — watchdog de saída de emergência
// ============================================================
// Quando o runtime tokio fica preso em RPC (rate-limit, half-open),
// sinais de shutdown assíncronos não são processados. Esta flag é setada
// por 'q' / Esc / Ctrl+C na TUI e por um listener Ctrl+C independente;
// um watchdog thread força `process::exit(130)` após 5s.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub static EMERGENCY_SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn request_emergency_shutdown() {
    EMERGENCY_SHUTDOWN.store(true, Ordering::Relaxed);
}

pub fn spawn_emergency_watchdog() {
    std::thread::spawn(move || {
        while !EMERGENCY_SHUTDOWN.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(Duration::from_secs(5));
        eprintln!("🛑 Saída de emergência: runtime tokio não respondeu em 5s.");
        std::process::exit(130);
    });
}
