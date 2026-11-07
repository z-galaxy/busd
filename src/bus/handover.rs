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
    connection,
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
    /// on its way through it, without waiting for its clients to take what it sends them. The
    /// connection of each client then goes to the next instance, with the messages that zbus read
    /// of the client and that the bus didn't take, what it read of a message that the client is in
    /// the middle of sending, if any, what the bus sent the client and the client didn't take, and
    /// the file descriptors that came with those, for the next instance to go on from there: it
    /// writes the client what the client didn't take first. What the clients send meanwhile waits
    /// for the next instance too, as do the clients that connect, in the socket that the bus
    /// listens on. A client that goes meanwhile, by the time that the bus is done with what it
    /// took, has what it sent before it went taken, and the bus cleans up after it, as after any
    /// client that goes, as its connection may have ended on an error, or been closed by the bus,
    /// as a send to the client failed, neither of which can be handed over. One that goes after
    /// that, as it closes its end, is handed over with what it sent, for the next instance to pass
    /// that on, and clean up after it. The clients still in the middle of their handshakes have
    /// [`STOP_TIMEOUT`] to finish them, and get disconnected if they don't.
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
            state,
            listener,
            clients,
        })
    }

    /// Create a bus that takes over from the instance of the bus that handed `handover` over.
    ///
    /// The bus listens on `listener`, if given one, such as the socket that systemd's socket
    /// activation passes, or else on the socket of `handover`. It serves the clients of `handover`
    /// as their bus did, with their unique names, names, match rules and calls that wait for their
    /// replies, from where their bus left them. Those whose connections `handover` lacks went
    /// meanwhile, so the bus cleans up after them as after any client that goes.
    ///
    /// The settings of the bus, such as [`Bus::set_max_objects_per_user`], are those of a new bus,
    /// and apply to the clients that it took over too, once set on the returned bus. The objects
    /// that it took over count, even beyond a maximum that is lower than the previous one.
    pub async fn take_over(handover: Handover, listener: Option<OwnedFd>) -> Result<Self> {
        let state = handover.state;
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
    /// The state of the bus: its GUID, and its peers, monitors and names.
    pub(crate) state: State,
    /// The socket that the bus listened on, unless it was given one, as by systemd's socket
    /// activation, which then gives the next instance the socket too.
    pub listener: Option<OwnedFd>,
    /// The clients, peers and monitors.
    pub clients: Vec<ClientHandover>,
}

/// What a bus hands over of a client, besides what the state of the bus holds of it: its
/// connection.
#[derive(Debug)]
pub struct ClientHandover {
    /// The number of the client, which its unique name carries.
    pub id: usize,
    /// The hand-over of the connection of the client: its socket, with what zbus read of the
    /// client and the bus didn't take, and the file descriptors that came with that, which the
    /// next instance passes on.
    pub connection: connection::Handover,
}

/// How long a client in the middle of its handshake has to finish that as the bus hands over,
/// before the bus lets go of it, which disconnects it.
///
/// The bus doesn't wait for the other clients to be done sending their messages: it stops taking
/// them once it is done with those that it took, wherever the clients are in sending the next ones.
/// Nor does it wait for its clients to take what it sends them: what a client doesn't take goes on
/// with the hand-over of its connection.
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
pub(crate) struct State {
    /// The GUID of the bus, which its clients know it by.
    guid: OwnedGuid,
    /// The number of the last client that connected.
    last_id: u64,
    peers: Vec<PeerState>,
    monitors: Vec<MonitorState>,
    names: Vec<NameState>,
}

impl State {
    /// The records of the hand-overs of the connections of the clients, each with the number of
    /// its client: those of the peers, then those of the monitors.
    pub(crate) fn connections_mut(&mut self) -> impl Iterator<Item = (u64, &mut Vec<u8>)> {
        let peers = self
            .peers
            .iter_mut()
            .map(|peer| (peer.id, &mut peer.connection));
        let monitors = self
            .monitors
            .iter_mut()
            .map(|monitor| (monitor.id, &mut monitor.connection));

        peers.chain(monitors)
    }

    /// The state, encoded in the D-Bus wire format, after the version of its encoding, as the bus
    /// passes it on to its next instance in another process.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let data = to_bytes(serialized::Context::new(LE, 0), &(VERSION, self))?;

        Ok(data.bytes().to_vec())
    }

    /// The state that `bytes` encode, as [`State::encode`] encodes it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
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
    /// The record of the hand-over of the connection of the peer, as zbus saves it
    /// ([`connection::Handover::save`]), for a next instance in another process, which gets the
    /// file descriptors of the connection apart. It is empty otherwise, as for a peer that the bus
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
    /// The record of the hand-over of the connection of the monitor, as for a peer: see
    /// [`PeerState`].
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

        let encoded = state.encode().unwrap();
        let decoded = State::decode(&encoded).unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{state:?}"));

        let mut other = encoded.clone();
        other[0] = 1;
        let error = State::decode(&other).unwrap_err();
        assert!(error.to_string().contains("version 1"), "{error}");
    }

    /// A bus doesn't take over from a state that it can't decode, rather than panic: garbage, the
    /// state of a version that it doesn't know, and one of the right version that is cut short, or
    /// whose body is spoiled.
    #[test]
    fn undecodable_states_are_refused() {
        let state = State {
            guid: zbus::Guid::generate().into(),
            last_id: 1,
            peers: Vec::new(),
            monitors: Vec::new(),
            names: Vec::new(),
        };
        let encoded = state.encode().unwrap();
        let spoils: [fn(&mut Vec<u8>); 4] = [
            |state| *state = b"not a state".to_vec(),
            |state| state[0] += 1,
            |state| state.truncate(4 + 6),
            |state| state[4..16].fill(0xff),
        ];
        for spoil in spoils {
            let mut spoiled = encoded.clone();
            spoil(&mut spoiled);
            assert!(State::decode(&spoiled).is_err(), "{spoiled:?}");
        }
    }
}
