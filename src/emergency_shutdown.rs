// ============================================================
// src/emergency_shutdown.rs — watchdog de saída de emergência
// ============================================================
// Quando o runtime tokio fica preso em RPC (rate-limit, half-open),
// sinais de shutdown assíncronos não são processados. Esta flag é setada
// por 'q' / Esc / Ctrl+C na TUI e por um listener Ctrl+C independente;
// um watchdog thread força `process::exit(130)` após uma janela de graça.

use crossterm::{
    event::DisableMouseCapture,
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub static EMERGENCY_SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn request_emergency_shutdown() {
    EMERGENCY_SHUTDOWN.store(true, Ordering::Relaxed);
}

/// Tenta restaurar o terminal antes de matar o processo. Chamamos mesmo se a
/// TUI não estiver ativa: as operações do crossterm são idempotentes e
/// seguras quando o terminal já está em modo cooked.
fn emergency_terminal_cleanup() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

pub fn spawn_emergency_watchdog(grace: Duration) {
    std::thread::spawn(move || {
        while !EMERGENCY_SHUTDOWN.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        // A flag is armed as soon as shutdown is requested. Give the normal
        // runtime the configured grace period before forcing termination.
        std::thread::sleep(grace);
        eprintln!(
            "🛑 Saída de emergência: runtime tokio não respondeu em {}s.",
            grace.as_secs()
        );
        emergency_terminal_cleanup();
        std::process::exit(130);
    });
}
