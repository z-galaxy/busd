//! A bus that systemd runs, on the socket of its socket activation.

use std::os::fd::OwnedFd;

use anyhow::{bail, Result};

use super::Bus;

impl Bus {
    /// Create a bus that systemd runs, out of the file descriptors that systemd passed as it
    /// started the process, `fds`, as [`take_listen_fds`](crate::systemd::take_listen_fds) gives
    /// them.
    ///
    /// The bus listens on the socket of the socket activation, if systemd passed one, as
    /// [`Bus::for_listener`] does, or else at `address`, as [`Bus::for_address`] does. busd listens
    /// on one socket, so this fails if systemd passed more than one.
    pub async fn for_systemd(fds: Vec<OwnedFd>, address: Option<&str>) -> Result<Self> {
        let mut fds = fds.into_iter();
        let listener = fds.next();
        if fds.next().is_some() {
            bail!("systemd passed more than one socket to listen on: busd listens on one.");
        }

        match listener {
            Some(listener) => Self::for_listener(listener).await,
            None => Self::for_address(address).await,
        }
    }
}
