//! The hand-over of a bus to its next instance, which serves its clients on from where it stopped,
//! on the same connections.

use std::{
    collections::HashMap,
    os::fd::{AsFd, OwnedFd},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use zbus::{
    address::Transport,
    names::{OwnedUniqueName, OwnedWellKnownName},
    wire::{serialized, to_bytes, LE},
    Address, OwnedGuid, Type,
};

use super::Bus;
use crate::{info, match_rules::MatchRules};

impl Bus {
    /// Stop the bus where its next instance can take over from it, and hand it over.
    ///
    /// The bus pauses: it stops taking the messages of each client, once it is done with those
    /// that it took. It forwards those, answers the calls to it that it got, and sends what it has
    /// to send on its own, such as the signals of the names that change owners, until nothing is
    /// on its way through it. The connection of each client then goes to the next instance, with
    /// the messages that zbus read of the client and that the bus didn't take, what it read of a
    /// message that the client is in the middle of sending, if any, and the file descriptors that
    /// came with those, for the next instance to go on from there. What the clients send meanwhile
    /// waits for the next instance too, as do the clients that connect, in the socket that the bus
    /// listens on. A client that goes meanwhile, by the time that the bus is done with what it
    /// took, has what it sent before it went taken, and the bus cleans up after it, as after any
    /// client that goes, as its connection may have ended on an error, or been closed by the bus,
    /// as a send to the client failed, neither of which can be handed over. So does a client that
    /// the bus disconnects meanwhile, as it doesn't take what the bus sends it in time. One that
    /// goes after that, as it closes its end, is handed over with what it sent, for the next
    /// instance to pass that on, and clean up after it. The clients still in the middle of their
    /// handshakes have [`STOP_TIMEOUT`] to finish them, and get disconnected if they don't.
    ///
    /// The bus lets go of its clients without disconnecting them: their connections end only once
    /// all of their sockets are closed, those of the [`Handover`] included. A client whose
    /// connection fails to be handed over gets disconnected, and is left out of the [`Handover`]:
    /// the next instance cleans up after it as after a client that went.
    pub async fn hand_over(self) -> Result<Handover> {
        // The socket that the bus listens on is duplicated ahead of those of the clients, which a
        // lack of file descriptors only fails one at a time.
        let listener = if self.bound {
            Some(self.listener.as_fd().try_clone_to_owned()?)
        } else {
            None
        };
        let peers = self.inner.peers.clone();
        peers.pause(STOP_TIMEOUT).await?;
        peers.settle().await?;
        let (snapshot, clients) = peers.snapshot().await?;
        let state = State {
            guid: self.inner.guid.clone(),
            last_id: self.inner.next_id as u64,
            peers: snapshot.peers,
            monitors: snapshot.monitors,
            names: snapshot.names,
        };
        peers.retire().await;
        info!(
            "Handing over {} clients and {} names.",
            clients.len(),
            state.names.len()
        );

        Ok(Handover {
            state: state.encode()?,
            listener,
            clients,
        })
    }

    /// Create a bus that takes over from the instance of the bus that handed `handover` over.
    ///
    /// The bus listens on `listener`, if given one, such as the socket that systemd's socket
    /// activation passes, or else on the socket of `handover`. It serves the clients of `handover`
    /// as their bus did, with their unique names, names, match rules and calls that wait for their
    /// replies, from where their bus left them. Those whose sockets are missing went meanwhile, so
    /// the bus cleans up after them as after any client that goes, as it does after those that
    /// lack file descriptors that came with the messages that their bus didn't take.
    ///
    /// The settings of the bus, such as [`Bus::set_max_objects_per_user`], are those of a new bus,
    /// and apply to the clients that it took over too, once set on the returned bus. The objects
    /// that it took over count, even beyond a maximum that is lower than the previous one.
    pub async fn take_over(handover: Handover, listener: Option<OwnedFd>) -> Result<Self> {
        let state = State::decode(&handover.state)?;
        let (listener, bound) = match (listener, handover.listener) {
            (Some(listener), _) => (listener, false),
            (None, Some(listener)) => (listener, true),
            (None, None) => bail!("Nothing to listen on: no socket was handed over or given."),
        };
        let (listener, unix) = Self::listener(listener)?;
        let address = Address::new(Transport::Unix(unix)).set_guid(state.guid.clone())?;
        let guid = state.guid;
        let mut bus = Self::new(listener, address, guid, bound).await?;
        bus.inner.next_id = usize::try_from(state.last_id)?;
        let clients: HashMap<_, _> = handover
            .clients
            .into_iter()
            .map(|client| (client.id, client))
            .collect();
        let snapshot = Snapshot {
            peers: state.peers,
            monitors: state.monitors,
            names: state.names,
        };
        bus.inner
            .peers
            .restore(snapshot, clients, bus.inner.send_timeout)
            .await?;

        Ok(bus)
    }
}

/// What a bus hands over to the instance of the bus that takes over from it.
///
/// [`Bus::hand_over`] stops a bus where nothing is on its way through it, and
/// [`Bus::take_over`] creates the next instance out of what it handed over, which serves the same
/// clients on the same connections, with the same names, match rules and calls that wait for
/// replies. The clients notice nothing but a pause.
#[derive(Debug)]
pub struct Handover {
    /// The state of the bus, encoded: its GUID, and its peers, monitors and names.
    pub state: Vec<u8>,
    /// The socket that the bus listened on, unless it was given one, as by systemd's socket
    /// activation, which then gives the next instance the socket too.
    pub listener: Option<OwnedFd>,
    /// The clients, peers and monitors.
    pub clients: Vec<ClientHandover>,
}

/// What a bus hands over of a client, besides what the state of the bus holds of it: the file
/// descriptors of its connection.
#[derive(Debug)]
pub struct ClientHandover {
    /// The number of the client, which its unique name carries.
    pub id: usize,
    /// The socket of the client, followed by the file descriptors that came with the messages
    /// that the bus didn't take, in their order: the next instance passes them on with those.
    ///
    /// They are those of the hand-over of the connection, as zbus encodes it
    /// ([`zbus::connection::Handover::encode`]), in its order.
    pub fds: Vec<OwnedFd>,
}

/// How long a client in the middle of its handshake has to finish that as the bus hands over,
/// before the bus lets go of it, which disconnects it.
///
/// The bus doesn't wait for the other clients to be done sending their messages: it stops taking
/// them once it is done with those that it took, wherever the clients are in sending the next ones,
/// and the hand-over waits for those that it took to reach their recipients, each of which has as
/// long as it has to take a message, from when the hand-over starts.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// The peers, monitors and names of a bus, as it hands them over.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) peers: Vec<PeerState>,
    pub(crate) monitors: Vec<MonitorState>,
    pub(crate) names: Vec<NameState>,
}

/// The state of a bus, as it hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
struct State {
    /// The GUID of the bus, which its clients know it by.
    guid: OwnedGuid,
    /// The number of the last client that connected.
    last_id: u64,
    peers: Vec<PeerState>,
    monitors: Vec<MonitorState>,
    names: Vec<NameState>,
}

impl State {
    /// The state, encoded in the D-Bus wire format, after the version of its encoding.
    fn encode(&self) -> Result<Vec<u8>> {
        let data = to_bytes(serialized::Context::new(LE, 0), &(VERSION, self))?;

        Ok(data.bytes().to_vec())
    }

    /// The state that `bytes` encode, as [`State::encode`] encodes it.
    fn decode(bytes: &[u8]) -> Result<Self> {
        let data = serialized::Data::new(bytes, serialized::Context::new(LE, 0));
        let (version, _) = data
            .deserialize::<u32>()
            .context("The state that was handed over has no version")?;
        if version != VERSION {
            bail!("The state that was handed over is of version {version}, not {VERSION}.");
        }
        let ((_, state), _) = data
            .deserialize::<(u32, Self)>()
            .context("Failed to decode the state that was handed over")?;

        Ok(state)
    }
}

/// A peer, as the bus hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct PeerState {
    /// The number of the peer, which its unique name carries.
    pub(crate) id: u64,
    /// Whether the peer called `Hello`.
    pub(crate) greeted: bool,
    /// The hand-over of the connection of the peer, as zbus encodes it, but for its file
    /// descriptors, which go apart: see [`ClientHandover`]. It is empty for a peer that the bus
    /// left out of the hand-over.
    pub(crate) connection: Vec<u8>,
    pub(crate) match_rules: MatchRules,
    /// The calls to the peer that wait for its reply: the number of the caller, its unique name,
    /// and the serial of the call.
    pub(crate) pending_calls: Vec<(u64, OwnedUniqueName, u32)>,
}

/// A monitor, as the bus hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct MonitorState {
    /// The number of the peer that became the monitor.
    pub(crate) id: u64,
    /// The hand-over of the connection of the monitor, as for a peer: see [`PeerState`].
    pub(crate) connection: Vec<u8>,
    pub(crate) match_rules: MatchRules,
}

/// A well-known name, as the bus hands it over: its owner first, then those that queue for it, in
/// order, each with whether it lets another take the name over from it.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct NameState {
    pub(crate) name: OwnedWellKnownName,
    pub(crate) owners: Vec<(OwnedUniqueName, bool)>,
}

/// The version of the encoding of the state: an instance of the bus that encodes its state
/// otherwise doesn't take over from this one, nor this one from it.
const VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use zbus::{names::OwnedUniqueName, OwnedMatchRule};

    use super::*;

    #[test]
    fn state() {
        let name = |id: u32| OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();
        let mut match_rules = MatchRules::default();
        let rule = OwnedMatchRule::try_from("type='signal',sender='org.zbus.Test'").unwrap();
        match_rules.add(rule);
        let state = State {
            guid: zbus::Guid::generate().into(),
            last_id: 3,
            peers: vec![PeerState {
                id: 1,
                greeted: true,
                connection: b"\x01l\x01\0\x01".to_vec(),
                match_rules,
                pending_calls: vec![(2, name(2), 7)],
            }],
            monitors: vec![MonitorState {
                id: 3,
                connection: Vec::new(),
                match_rules: MatchRules::default(),
            }],
            names: vec![NameState {
                name: OwnedWellKnownName::try_from("org.zbus.Test").unwrap(),
                owners: vec![(name(1), false), (name(2), true)],
            }],
        };

        let decoded = State::decode(&state.encode().unwrap()).unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{state:?}"));

        let mut other = state.encode().unwrap();
        other[0] = 1;
        let error = State::decode(&other).unwrap_err();
        assert!(error.to_string().contains("version 1"), "{error}");
    }
}
