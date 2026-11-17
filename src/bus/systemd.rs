//! A bus that systemd runs: on the socket of its socket activation, and handed over to its next
//! instance through its file descriptor store.
//!
//! # A restart through the store
//!
//! With [zoter], an instance of busd that stops hands its state and the connections of its clients
//! over to the next instance, through the file descriptor store that systemd keeps for the service
//! across its restarts. The clients notice nothing but a pause: the next instance serves them on
//! the same connections, with the same names, match rules and calls that wait for replies, answers
//! the calls to the bus that were on their way to it, and passes on what was on its way from it.
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
//! The bus hands over under socket activation, as [Socket activation](#socket-activation) tells.
//! On `SIGTERM`, it pauses its clients and itself, finishes what it took, and hands their
//! connections and its own over ([`Bus::hand_over`]). It hands its state and, as the items of the
//! hand-over, its own connection, and the connections of the clients, each under the number of its
//! client, to zoter, which stores them under `busd/` and waits for systemd to
//! have dealt with them, and it exits ([`Bus::hand_over_to_systemd`]). systemd starts the next
//! instance with the sockets of socket activation and what was stored, which it takes over from
//! ([`Bus::for_systemd`]):
#![doc = simple_mermaid::mermaid!("systemd/restart.mmd")]
//!
//! zoter lays the hand-over out in the store: a memfd, under `busd/state`, which holds the state
//! of the bus and the record of each connection, read in place, and sockets, under `busd/fds`,
//! whose queues hold the connection FDs, some 70000 in each, as many as that takes. The state of
//! the bus refers to each client by its number, which zoter gives back with its connection.
//!
//! The bus serves the D-Bus API on a connection to itself, over a channel in its process, whose two
//! ends it hands over too, with what was on its way between the bus and its clients: the calls to
//! the bus that it didn't take, and what it sent and didn't pass on. The channel stays behind: the
//! two ends go together as one item of the hand-over, ahead of the clients, with what the channel
//! held for each, and the FDs of their messages for connection FDs, under the number of the peer
//! of the bus, which no client has. The next instance joins the two with a channel of its own. It
//! takes nothing over without them, rather than leave the clients whose calls were on their way
//! waiting for replies that would never come. It does take over from the state of an earlier
//! version of busd, which came without that connection, as that version made sure that nothing was
//! on its way before it stopped, while an earlier version takes nothing over from this one.
//!
//! zoter stores a hand-over whole, or fails to, and the next instance takes it over whole, or fails
//! to: zoter leaves no client out of it. The next instance takes the connection FDs out once it
//! raised its soft limit of open FDs to the hard one, so they have to fit under that, with the
//! process FD of each client, which it opens as it serves the client on. If the connection FDs
//! don't fit, it takes nothing over, which disconnects all of the clients. If they do, a client
//! whose process FD it then can't open gets disconnected, and the others are served on. A
//! client-sent FD that is of the same open file description as another, as one that a client sent
//! twice, goes as it is, unlike in the store, which keeps one FD of each.
//!
//! # What the units need
//!
//! The hand-over is systemd's work as much as the bus's, so it takes units that have systemd do
//! its part, as the README of busd shows: a socket unit, for
//! [socket activation](#socket-activation), and a service unit that sets
//! `FileDescriptorStoreMax=` to 2 at least, under systemd 254 or later, and has `Requires=` on the
//! socket unit, as systemd starts a service without its socket where the socket unit isn't active
//! (systemd.socket(5): "the service may be started without the socket"). zoter gives the bus the
//! store of such a service, and fails to otherwise ([`zoter::Store::from_env`]).
//!
//! The bus tells systemd that it has started as it takes over through the store, which a unit of
//! `Type=notify` has systemd wait for, before it restores the connections that it took over. Its
//! clients wait for it in the socket of socket activation meanwhile. So busd needs both the store
//! and the socket under a unit that systemd takes notifications from, and fails to start without
//! either: zoter's error says what the unit lacks. Where systemd takes no notifications from the
//! bus, as when it doesn't run it at all, the bus runs without a store, and never hands over: it
//! disconnects its clients as it stops. It listens on the socket of socket activation if systemd
//! passed one, and on a socket of its own otherwise.
//!
//! # Socket activation
//!
//! The hand-over takes socket activation, which makes the listening socket systemd's: systemd
//! passes it to every instance, under the name of its socket unit, and keeps it open while no
//! instance runs, with the clients that connect meanwhile in its backlog. The bus listens on it
//! whatever its address says.
//!
//! A bus with a store never binds a listening socket of its own, which nothing would keep for the
//! next instance: it fails to start without the socket of socket activation, as when its socket
//! unit went before a restart, and leaves what the previous instance handed over in the store. The
//! bus without a store binds one at its address if systemd passed no socket, and removes the file
//! of it as it stops, as busd does without systemd.
//!
//! [zoter]: https://github.com/z-galaxy/zoter

use std::{iter, os::fd::OwnedFd, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use zbus::connection::{self, socket::ChannelHandover};
use zoter::{Passed, Restored, Save, Store};
use zruntime::SharedRuntime;

use super::{handover::State, Bus, ClientHandover, Handover, OwnHandover};
use crate::{
    debug, error, info,
    peer::{BUS_ID, SEND_TIMEOUT},
    policy::Policy,
    warn,
};

impl Bus {
    /// Create a bus that systemd runs, out of the FDs that systemd passed, `passed`, and the store
    /// of the service, `store`, if systemd keeps one.
    ///
    /// With a store, the bus takes over from the instance that handed over through it, as
    /// [`Bus::hand_over_to_systemd`] does, if one did, and tells systemd that it has started as it
    /// does, while its clients wait in the socket of the socket activation. The bus listens on
    /// that socket, whatever `address` says: the store of [`zoter::Store::from_env`] comes with
    /// one, and this fails if systemd passed none. busd listens on one socket: where systemd
    /// passed more than one listening socket of socket activation, it listens on the first, and
    /// closes the others with a warning, which leaves the clients that connect to those without an
    /// answer. Taking over may fail, as when the previous instance encoded its state otherwise, or
    /// when what it handed over doesn't fit under the limit of open FDs of this one: the clients
    /// that were handed over get disconnected then, and the bus starts as if nothing was handed
    /// over. What was stored is removed from the store first, so that whatever comes of it, the
    /// instances that come after don't find it again.
    ///
    /// Without a store, the bus takes nothing over, and doesn't tell systemd anything. It listens
    /// on the first socket of the socket activation, if systemd passed one, as above, and otherwise
    /// at `address`, as [`Bus::for_address`] does.
    ///
    /// The bus goes by `policy`, as [`Bus::set_policy`] sets it, and the clients that it takes over
    /// do from the start, as with [`Bus::take_over`].
    pub async fn for_systemd(
        mut passed: Passed,
        store: Option<&mut Store>,
        address: Option<&str>,
        policy: Arc<Policy>,
    ) -> Result<Self> {
        let has_store = store.is_some();
        let restored = store.and_then(|store| {
            store.take_over(&mut passed).unwrap_or_else(|e| {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its clients: {e}"
                );

                None
            })
        });
        let mut bus = match (Self::take_activation(passed), restored) {
            (None, _) if has_store => bail!(
                "A bus with a file descriptor store needs a socket of socket activation to \
                 listen on, and systemd passed none."
            ),
            (None, _) => Self::for_address(address).await?,
            (Some(activation), None) => Self::for_listener(activation).await?,
            (Some(activation), Some(restored)) => {
                Self::out_of_restored(restored, activation, policy.clone()).await?
            }
        };
        // A bus that took over goes by the policy already, and any other has no clients yet.
        bus.set_policy(policy).await;

        Ok(bus)
    }

    /// Hand the bus over to its next instance, through `store`, the store of systemd. The bus
    /// listens on the listening socket of the socket activation, which systemd keeps for the next
    /// instance. A bus that listens on a socket of its own instead has nothing to hand over to: it
    /// cleans up as [`Bus::cleanup`] does, only logging a failure to, and fails.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: its state, the connection on which it
    /// serves the D-Bus API, both of its ends, and the connections of its clients, each under the
    /// number of its client. It stores all of them or fails: it fails if the bus takes longer than
    /// 30 s to hand over, if the two ends of its own connection can't go together, or if storing
    /// fails, as when the FDs of the hand-over don't fit in the store, which disconnects the
    /// clients as the bus goes. The next instance needs to be able to open all of the FDs that the
    /// bus stored, and the process FD of each client, under its limit of open FDs: it takes nothing
    /// over if it can't open those that were stored, which disconnects the clients.
    pub async fn hand_over_to_systemd(self, store: Store) -> Result<()> {
        if self.listener.is_bound() {
            if let Err(e) = self.cleanup().await {
                error!("Failed to clean up: {e}");
            }

            bail!(
                "The bus listens on a socket of its own, rather than on one of socket activation, \
                 which systemd would keep for the next instance: a bus with a file descriptor \
                 store needs a socket of socket activation to listen on."
            );
        }
        info!("Handing over to the next instance of the bus..");
        let handover = SharedRuntime::current()?
            .timeout(HAND_OVER_TIMEOUT, self.hand_over())
            .await
            .context("The bus took too long to hand over")??;

        let OwnHandover { service, peer } = handover
            .own
            .context("The bus didn't hand over the connection on which it serves the D-Bus API")?;
        // The channel between its two ends is in this process alone, so the two go without it,
        // each with what the channel held for it.
        let own = ChannelHandover::new(service, peer)?;
        let state = handover.state.encode()?;
        let clients = handover
            .clients
            .iter()
            .map(|client| (client.id as u64, &client.connection as &dyn Save));
        let items = iter::once((OWN_ID, &own as &dyn Save)).chain(clients);
        let stored = store.hand_over(&state[..], items)?;
        debug!(
            "Stored the state of the bus, its own connection and {} file descriptors of {} \
             clients for the next instance",
            stored.fds(),
            stored.items() - 1
        );

        Ok(())
    }

    /// The listening socket of the socket activation, if systemd passed one, out of `passed`, from
    /// which zoter took what was stored: the first of what is left.
    ///
    /// What comes after it is listening sockets of socket activation too, which have the names of
    /// their units by default, such as `busd.socket`, or, for a bus without a store, what was
    /// stored. busd listens on one socket, so it closes those, with a warning that names them:
    /// systemd keeps such a socket open, so the clients that connect to it get no answer.
    fn take_activation(passed: Passed) -> Option<OwnedFd> {
        let mut passed = passed.into_vec().into_iter();
        let (kept, activation) = passed.next()?;
        let ignored: Vec<_> = passed.map(|(name, _)| name).collect();
        if !ignored.is_empty() {
            warn!(
                "Ignoring what systemd passed after `{kept}`, as the bus listens on one socket, \
                 the first that systemd passed: `{}`. The clients that connect to a socket among \
                 these get no answer, as systemd keeps it open: remove it from the socket units \
                 of the service.",
                ignored.join("`, `")
            );
        }

        Some(activation)
    }

    /// The next instance of the bus that handed `restored` over, on `activation`, the listening
    /// socket of the socket activation, whose clients go by `policy` from the start.
    ///
    /// If taking over fails, it is a bus on `activation`, as [`Bus::for_listener`] creates, which
    /// `policy` is yet to be set on.
    async fn out_of_restored(
        restored: Restored,
        activation: OwnedFd,
        policy: Arc<Policy>,
    ) -> Result<Self> {
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
        match Self::take_over(handover, Some(activation), SEND_TIMEOUT, policy).await {
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
    /// state can be decoded, and the connection on which that instance served the D-Bus API came
    /// with it, whole.
    ///
    /// Each client gets its connection back from the record of its connection, read in place, and
    /// from its connection FDs. One whose connection can't be restored is gone: the next instance
    /// cleans up after it as after any client that goes. If the state can't be decoded, or the
    /// connection of the bus is missing or can't be restored, everything that was handed over
    /// closes, which disconnects the clients.
    fn from_restored(mut restored: Restored) -> Result<Self> {
        let (state, with_own) = State::decode(restored.state())?;
        let mut own = None;
        let mut clients = Vec::new();
        for item in restored.take_items() {
            if item.id == OWN_ID {
                let channel = item.restore::<ChannelHandover>().context(
                    "Failed to restore the connection on which the bus serves the D-Bus API",
                )?;
                own = Some(OwnHandover::from(channel));

                continue;
            }
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

        // A bus that lost what was on its way between the instance before it and its clients
        // would leave those clients waiting for it, so it takes nothing over then.
        if with_own && own.is_none() {
            bail!("The connection on which the bus serves the D-Bus API wasn't handed over");
        }

        // The listening socket is that of the socket activation, which systemd passes on its own.
        Ok(Self {
            state,
            listener: None,
            clients,
            own,
        })
    }
}

impl From<ChannelHandover> for OwnHandover {
    /// The connection on which a bus serves the D-Bus API, out of its two ends as they got to the
    /// next instance in another process, which a new channel joins, with what the channel before
    /// held for each.
    fn from(channel: ChannelHandover) -> Self {
        let (service, peer) = channel.into_ends();

        Self { service, peer }
    }
}

/// The key that busd hands over under, in the store of systemd.
pub const STORE_KEY: &str = "busd";
/// The id of the item of a hand-over that is the connection on which the bus serves the D-Bus
/// API, both of its ends: the number of the peer of the bus, which no client has, as the clients go
/// under their numbers.
const OWN_ID: u64 = BUS_ID as u64;
/// How long the bus may take to hand over, before busd gives up on it, and its clients get
/// disconnected as it goes.
///
/// The bus doesn't wait for its clients to take what it sends them as it hands over, but it gives
/// the hand-over of the connection of a client, which nothing should hold up, as long as the client
/// has to take a message, `SEND_TIMEOUT`, 10 s, before it gives up on it, as it gives a client to
/// take what the instance before it left unwritten. This is longer, with room for the rest of the
/// hand-over, and well within the 90 s that systemd gives a service to stop by default.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(30);
