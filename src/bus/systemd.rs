//! A bus that systemd runs: on the socket of its socket activation, and handed over to its next
//! instance through its file descriptor store.
//!
//! # A restart through the store
//!
//! With [zoter], an instance of busd that stops hands its state and the connections of its clients
//! over to the next instance, through the file descriptor store that systemd keeps for the service
//! across its restarts. The clients notice nothing but a pause: the next instance serves them on
//! the same connections, with the same names, match rules and calls that wait for replies, and
//! passes on what was on its way through the bus.
//!
//! A hand-over takes file descriptors (FDs) of many kinds, which these docs name so:
//!
//! - The **listening socket** is the socket that the bus accepts its clients on. That of socket
//!   activation is systemd's, and one that the bus binds itself is its own.
//! - The **client socket** of a client is the end of its connection that the bus has.
//! - The **client-sent FDs** of a client are those that it sent with its messages, or ahead of
//!   them, which the bus read and didn't take yet.
//! - The **outgoing FDs** of a client are those of the messages that the bus is sending it and
//!   didn't write to it yet.
//! - The **connection FDs** of a client are its client socket, its client-sent FDs and its outgoing
//!   FDs, in that order, as zbus gives them with the record of its connection
//!   ([`zoter::Save::fds`]).
//! - The **process FD** of a client is the pidfd of its process, which the next instance opens to
//!   look the credentials of the client up.
//!
//! The bus hands over under socket activation alone, as [Socket activation](#socket-activation)
//! tells. On `SIGTERM`, it pauses its clients, finishes what it took, and hands their connections
//! over ([`Bus::hand_over`]). It hands its state and, as the items of the hand-over, the
//! connections of the clients, each under the number of its client, to zoter, which stores them
//! under `busd/` and waits for systemd to have dealt with them, and it exits
//! ([`Bus::hand_over_to_systemd`]). systemd starts the next instance with the sockets of socket
//! activation and what was stored, which it takes over from ([`Bus::for_systemd`]):
#![doc = simple_mermaid::mermaid!("systemd/restart.mmd")]
//!
//! zoter lays the hand-over out in the store: a memfd, under `busd/state`, which holds the state
//! of the bus and the record of the connection of each client, read in place, and sockets, under
//! `busd/fds`, whose queues hold the connection FDs, some 70000 in each, as many as that takes.
//! The state of the bus refers to each client by its number, which zoter gives back with its
//! connection.
//!
//! The next instance takes the connection FDs out once it raised its soft limit of open FDs to the
//! hard one, so they have to fit under that, with the process FD of each client, which zoter sees
//! to: the clients that don't fit get disconnected as the bus goes. A client-sent FD that is of the
//! same open file description as another, as one that a client sent twice, goes as it is, unlike in
//! the store, which keeps one FD of each.
//!
//! # Socket activation
//!
//! The hand-over takes socket activation, which makes the listening socket systemd's: systemd
//! passes it to every instance, under the name of its socket unit, and keeps it open while no
//! instance runs, with the clients that connect meanwhile in its backlog. The bus listens on it
//! whatever its address says.
//!
//! Without socket activation, the bus binds a listening socket of its own, which nothing keeps for
//! the next instance, so it doesn't hand over: it disconnects its clients as it goes, and removes
//! the file of its listening socket, as busd does without systemd. Nor does the next instance take
//! over without socket activation, as when its socket unit went meanwhile: it drops what was handed
//! over, which disconnects the clients, and binds a listening socket of its own.
//!
//! [zoter]: https://github.com/z-galaxy/zoter

use std::{os::fd::OwnedFd, time::Duration};

use anyhow::{bail, Context, Result};
use zbus::connection;
use zoter::{Passed, Restored, Store};
use zruntime::SharedRuntime;

use super::{handover::State, Bus, ClientHandover, Handover};
use crate::{debug, error, info, peer::SEND_TIMEOUT, warn};

impl Bus {
    /// Create a bus that systemd runs, out of the FDs that systemd passed, `passed`, and the store
    /// of the service, `store`, if any.
    ///
    /// The bus listens on the listening socket of the socket activation, if systemd passed one,
    /// whatever `address` says, and takes over there from the instance that handed over through
    /// the store, as [`Bus::hand_over_to_systemd`] does, if one did. Taking over may fail, as when
    /// the previous instance encoded its state otherwise: the clients that were handed over get
    /// disconnected then, and the bus starts as if nothing was handed over. Without socket
    /// activation, the bus listens at `address`, as [`Bus::for_address`] does, and drops what was
    /// handed over, if anything, which disconnects the clients. busd listens on one socket, so
    /// this fails if systemd passed more than one listening socket of socket activation. What was
    /// stored is removed from the store first, so that whatever comes of it, the instances that
    /// come after don't find it again.
    pub async fn for_systemd(
        mut passed: Passed,
        store: Option<&mut Store>,
        address: Option<&str>,
    ) -> Result<Self> {
        let restored = match store {
            Some(store) => store.take_over(&mut passed).unwrap_or_else(|e| {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its clients: {e}"
                );

                None
            }),
            // Without a store to remove it from, what was stored is left alone, rather than taken
            // for sockets of socket activation.
            None => {
                drop(take_stored(&mut passed));

                None
            }
        };
        let Some(activation) = Self::take_activation(passed)? else {
            if restored.is_some() {
                warn!(
                    "Dropping what the previous instance of the bus handed over, which disconnects \
                     its clients, as systemd passed no socket of socket activation to take over on"
                );
            }

            return Self::for_address(address).await;
        };
        let Some(restored) = restored else {
            return Self::for_listener(activation).await;
        };

        Self::out_of_restored(restored, activation).await
    }

    /// Hand the bus over to its next instance, through `store`, the store of systemd, if the bus
    /// listens on the listening socket of the socket activation, which systemd keeps for the next
    /// instance, and the store takes a hand-over. Otherwise, the bus says why it doesn't hand over,
    /// and cleans up as [`Bus::cleanup`] does, only logging a failure to, as busd does without
    /// systemd.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: its state, and the connections of its
    /// clients, each under the number of its client, with one FD more for each in the next
    /// instance, its process FD. The clients that don't fit, under the limit of open FDs of the
    /// next instance or in the store, get disconnected as the bus goes, with a warning. It fails if
    /// the bus takes longer than 30 s to hand over, or if storing fails, which disconnects the
    /// clients as the bus goes.
    pub async fn hand_over_to_systemd(self, store: Store) -> Result<()> {
        let Some(mut to_store) = self.start_hand_over(store) else {
            if let Err(e) = self.cleanup().await {
                error!("Failed to clean up: {e}");
            }

            return Ok(());
        };
        info!("Handing over to the next instance of the bus..");
        let handover = SharedRuntime::current()?
            .timeout(HAND_OVER_TIMEOUT, self.hand_over())
            .await
            .context("The bus took too long to hand over")??;

        to_store.set_extra_fds_per_item(1);
        let state = handover.state.encode()?;
        let clients = handover
            .clients
            .iter()
            .map(|client| (client.id as u64, &client.connection));
        let stored = to_store.store(state, clients)?;
        debug!(
            "Stored the state of the bus and {} file descriptors of {} clients for the next \
             instance",
            stored.fds(),
            stored.items()
        );

        Ok(())
    }

    /// The hand-over of the bus to its next instance through `store`, started, if the bus can hand
    /// over: the store takes a hand-over, and the bus listens on the listening socket of the socket
    /// activation, which systemd keeps for the next instance, rather than on one of its own. It
    /// says why the bus can't otherwise.
    fn start_hand_over(&self, store: Store) -> Option<zoter::Handover> {
        let max = store.max();
        let handover = match store.handover() {
            Ok(handover) => handover,
            Err(e) => {
                // Most services that systemd runs keep no store, unlike one that is too small.
                match max {
                    Some(_) => warn!("Not handing over to the next instance of the bus: {e}"),
                    None => info!("Not handing over to the next instance of the bus: {e}"),
                }

                return None;
            }
        };
        if self.bound {
            warn!(
                "Not handing over to the next instance of the bus, as it listens on a socket of \
                 its own rather than on one of socket activation, which systemd would keep for \
                 the next instance: add a socket unit for the service."
            );

            return None;
        }

        Some(handover)
    }

    /// The listening socket of the socket activation, if systemd passed one, out of `passed`, from
    /// which zoter took what was stored.
    ///
    /// The others are listening sockets of socket activation too, which have the names of their
    /// units by default, such as `busd.socket`. busd listens on one, so this fails if there is
    /// more than one.
    fn take_activation(passed: Passed) -> Result<Option<OwnedFd>> {
        let mut sockets = passed.into_vec().into_iter().map(|(_, fd)| fd);
        let activation = sockets.next();
        if sockets.next().is_some() {
            bail!("systemd passed more than one socket to listen on: busd listens on one.");
        }

        Ok(activation)
    }

    /// The next instance of the bus that handed `restored` over, on `activation`, the listening
    /// socket of the socket activation.
    ///
    /// If taking over fails, it is a bus on `activation`, as [`Bus::for_listener`] creates.
    async fn out_of_restored(restored: Restored, activation: OwnedFd) -> Result<Self> {
        let handover = match Handover::from_restored(restored) {
            Ok(handover) => handover,
            Err(e) => {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its clients: {e:#}"
                );

                return Self::for_listener(activation).await;
            }
        };
        let clients = handover.clients.len();
        let fallback = activation.try_clone()?;
        match Self::take_over(handover, Some(activation), SEND_TIMEOUT).await {
            Ok(bus) => {
                info!("Took {clients} clients over from the previous instance of the bus.");

                Ok(bus)
            }
            Err(e) => {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its clients: {e:#}"
                );

                Self::for_listener(fallback).await
            }
        }
    }
}

impl Handover {
    /// What the previous instance of the bus handed over, out of what zoter took over, if its
    /// state can be decoded.
    ///
    /// Each client gets its connection back from the record of its connection, read in place, and
    /// from its connection FDs. One whose connection can't be restored, or whose connection FDs
    /// are missing, is gone: the next instance cleans up after it as after any client that goes.
    /// If the state can't be decoded, everything that was handed over closes, which disconnects
    /// the clients.
    fn from_restored(mut restored: Restored) -> Result<Self> {
        let state = State::decode(restored.state())?;
        let mut clients = Vec::new();
        for item in restored.take_items() {
            let Ok(id) = usize::try_from(item.id) else {
                continue;
            };
            match item.restore::<connection::Handover>() {
                Ok(connection) => clients.push(ClientHandover { id, connection }),
                Err(e) => {
                    warn!("Leaving `:busd.{id}` out, as its connection can't be restored: {e}")
                }
            }
        }
        // The connections hold what they need of their records, and the memfd is unmapped as
        // `restored` goes.

        // The listening socket is that of the socket activation, which systemd passes on its own.
        Ok(Self {
            state,
            listener: None,
            clients,
        })
    }
}

/// What was stored under the names of busd, out of `passed`, for a bus without a store to take it
/// over from.
fn take_stored(passed: &mut Passed) -> Vec<OwnedFd> {
    let names: Vec<_> = passed
        .iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with(STORE_PREFIX))
        .map(str::to_owned)
        .collect();

    names
        .iter()
        .flat_map(|name| passed.take_all(name))
        .collect()
}

/// The key that busd hands over under, in the store of systemd.
pub const STORE_KEY: &str = "busd";
/// What the names that zoter stores what busd hands over under start with.
const STORE_PREFIX: &str = "busd/";
/// How long the bus may take to hand over, before busd gives up on it, and its clients get
/// disconnected as it goes.
///
/// The bus doesn't wait for its clients to take what it sends them as it hands over, but it gives
/// the hand-over of the connection of a client, which nothing should hold up, as long as the client
/// has to take a message, `SEND_TIMEOUT`, 10 s, before it gives up on it, as it gives a client to
/// take what the instance before it left unwritten. This is longer, with room for the rest of the
/// hand-over, and well within the 90 s that systemd gives a service to stop by default.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    /// What was stored under the names of busd is taken out of what systemd passed, the names
    /// that another version of busd may store it under included, while the sockets of socket
    /// activation stay, even one whose unit has a name that starts like those of busd.
    #[test]
    fn stored_fds_are_told_apart() {
        let names = [
            "busd/state",
            "busd.socket",
            "busd/fds",
            "busd-session.socket",
            "busd/other",
        ];
        let mut passed = Passed::new(
            names
                .iter()
                .map(|name| (name.to_string(), null()))
                .collect(),
        );

        assert_eq!(take_stored(&mut passed).len(), 3);
        let left: Vec<_> = passed.iter().map(|(name, _)| name).collect();
        assert_eq!(left, ["busd.socket", "busd-session.socket"]);
        assert!(Bus::take_activation(passed).is_err());
    }

    /// An FD of no consequence.
    fn null() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }
}
