use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug)]
pub enum Interrupt {
    Interrupt,
    Terminate,
}

impl Interrupt {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}

/// Signal subscriptions belong to the CLI, not to a process-global polling flag.
#[cfg(unix)]
pub struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    pub async fn recv(&mut self) -> Result<Interrupt> {
        tokio::select! {
            signal = self.interrupt.recv() => {
                signal.context("interrupt signal stream closed")?;
                Ok(Interrupt::Interrupt)
            }
            signal = self.terminate.recv() => {
                signal.context("termination signal stream closed")?;
                Ok(Interrupt::Terminate)
            }
        }
    }
}

#[cfg(windows)]
pub struct Signals {
    interrupt: tokio::signal::windows::CtrlC,
    terminate: tokio::signal::windows::CtrlBreak,
}

#[cfg(windows)]
impl Signals {
    pub fn new() -> Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
            terminate: tokio::signal::windows::ctrl_break()?,
        })
    }

    pub async fn recv(&mut self) -> Result<Interrupt> {
        tokio::select! {
            signal = self.interrupt.recv() => {
                signal.context("interrupt signal stream closed")?;
                Ok(Interrupt::Interrupt)
            }
            signal = self.terminate.recv() => {
                signal.context("termination signal stream closed")?;
                Ok(Interrupt::Terminate)
            }
        }
    }
}
