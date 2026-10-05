//! Interruption handling: `Ctrl-C` and `SIGTERM` end the harness with the conventional exit codes.

use std::{fmt, io};

/// A signal that asks the harness to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// `SIGINT` / `Ctrl-C`.
    Interrupt,
    /// `SIGTERM`, e.g. a cancelled CI job.
    Terminate,
}

impl ShutdownSignal {
    /// The shell convention `128 + signal number`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_bench::signals::ShutdownSignal;
    ///
    /// assert_eq!(ShutdownSignal::Interrupt.exit_code(), 130);
    /// assert_eq!(ShutdownSignal::Terminate.exit_code(), 143);
    /// ```
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        })
    }
}

/// Installed signal handlers; install before starting work so no signal is missed.
#[derive(Debug)]
pub struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    /// Registers the handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS refuses the registration.
    #[cfg(unix)]
    pub fn install() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Registers the handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS refuses the registration.
    #[cfg(not(unix))]
    pub const fn install() -> io::Result<Self> {
        Ok(Self {})
    }

    /// Resolves when the first stop signal arrives.
    #[cfg(unix)]
    pub async fn recv(&mut self) -> ShutdownSignal {
        tokio::select! {
            _ = self.interrupt.recv() => ShutdownSignal::Interrupt,
            _ = self.terminate.recv() => ShutdownSignal::Terminate,
        }
    }

    /// Resolves when the first stop signal arrives.
    #[cfg(not(unix))]
    pub async fn recv(&mut self) -> ShutdownSignal {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
        ShutdownSignal::Interrupt
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    /// One test, because signals are process-wide and concurrent tests would see each other's.
    #[tokio::test]
    async fn signals_map_to_the_conventional_exit_codes() {
        use rustix::process::{Signal, getpid, kill_process};

        let mut signals = ShutdownSignals::install().unwrap();
        kill_process(getpid(), Signal::INT).unwrap();
        let interrupt = signals.recv().await;
        assert_eq!(interrupt, ShutdownSignal::Interrupt);
        assert_eq!(interrupt.exit_code(), 130);

        kill_process(getpid(), Signal::TERM).unwrap();
        let terminate = signals.recv().await;
        assert_eq!(terminate, ShutdownSignal::Terminate);
        assert_eq!(terminate.exit_code(), 143);
    }
}
