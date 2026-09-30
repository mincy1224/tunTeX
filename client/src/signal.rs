//! Interrupt handling.
//!
//! When the caller interrupts a build, the remote job should stop too -- there
//! is no point burning a CPU on a compile nobody is waiting for.
//!
//! This is best-effort by design.  On Windows a caller that force-terminates
//! this process gives it no chance to send anything, which is exactly why the
//! server enforces its own timeout regardless.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Result;
use crate::remote::Remote;

/// Exit status used when the user interrupts the build.
pub const EXIT_INTERRUPTED: i32 = 130;

/// Ask the server to stop a job.  Returns whether a job was actually cancelled.
pub fn cancel_remote(base_url: &str, token: Option<&str>, request_id: &str) -> Result<bool> {
    let remote = Remote::new(base_url, token.map(str::to_string), 30)?;
    remote.cancel(request_id)
}

/// Installs the Ctrl+C handler.
///
/// Only one handler may be installed per process, so this is called once from
/// `run`.  Tests exercise [`cancel_remote`] directly instead.
pub fn install_handler(base_url: String, token: Option<String>, request_id: String) -> Result<()> {
    let already_handled = AtomicBool::new(false);

    ctrlc::set_handler(move || {
        // A second Ctrl+C should fall through to the default behaviour and kill
        // the process immediately, rather than being swallowed.
        if already_handled.swap(true, Ordering::SeqCst) {
            eprintln!("tuntex: interrupted again, exiting");
            std::process::exit(EXIT_INTERRUPTED);
        }

        eprintln!("tuntex: interrupted, cancelling the remote build ({request_id})");

        match cancel_remote(&base_url, token.as_deref(), &request_id) {
            Ok(true) => eprintln!("tuntex: remote build cancelled"),
            Ok(false) => eprintln!("tuntex: the remote build had already finished"),
            Err(error) => eprintln!(
                "tuntex: could not cancel the remote build: {}\n\
                 tuntex: the server will stop it when its timeout expires",
                error.message()
            ),
        }

        std::process::exit(EXIT_INTERRUPTED);
    })
    .map_err(|error| {
        crate::error::Error::software(format!("could not install the interrupt handler: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_without_a_server_is_an_error_not_a_panic() {
        let error = cancel_remote(
            "http://127.0.0.1:1",
            None,
            "00000000-0000-4000-8000-000000000000",
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 69);
    }

    #[test]
    fn the_interrupt_exit_code_is_the_conventional_one() {
        assert_eq!(EXIT_INTERRUPTED, 130);
    }
}
