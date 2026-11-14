//! The hand-over of a bus to its next instance, which serves its clients on from where it stopped,
//! on the same connections.

use std::{
    collections::HashMap,
    os::fd::{AsFd, OwnedFd},
    sync::Arc,
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
use crate::{info, match_rules::MatchRules, peers::Peers, policy::Policy};

impl Bus {
    /// Stop the bus where its next instance can take over from it, and hand it over.
    ///
    /// The bus pauses: it stops taking the messages of each client, once it is done with those
    /// that it took, and forwards those. Then it takes no more of the calls to it: it answers
    /// those that it has under way, and does what it has left to do on its own, such as sending
    /// the signals of the names that change owners, without waiting for its clients to take what
    /// it sends them. What is on its way between the bus and its clients by then, the calls to the
    /// bus that it didn't take and what it sent and didn't pass on, goes to the next instance as
    /// it is, on the connection on which the bus serves the D-Bus API, which the bus hands over
    /// too: the next instance answers those calls, and passes the rest on, once, in order. The
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
        // Ahead of the connections of the clients, which a bus that fails to hand its own over
        // disconnects as it goes.
        let own = peers.hand_over_us().await?;
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
            own: Some(own),
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
    /// The bus gives each client `send_timeout` to take a message, as [`Bus::set_send_timeout`]
    /// sets it, the clients of `handover` included, from the start: the bus writes each of those
    /// what the previous instance left unwritten as soon as it serves them, within that time. It
    /// goes by `policy` from the start too, as [`Bus::set_policy`] sets it, so that the policy has
    /// its say on the first message of each client of `handover`. Its other settings, such as
    /// [`Bus::set_max_objects_per_user`], are those of a new bus, and apply to the clients that it
    /// took over too, once set on the returned bus. The objects that it took over count, even
    /// beyond a maximum that is lower than the previous one.
    ///
    /// The bus serves the D-Bus API on the connection that `handover` has of the instance before
    /// it, so it answers the calls to that instance that it didn't take, and passes on what that
    /// instance sent and didn't pass on, before anything else, once its clients, their names and
    /// their match rules are back.
    pub async fn take_over(
        handover: Handover,
        listener: Option<OwnedFd>,
        send_timeout: Duration,
        policy: Arc<Policy>,
    ) -> Result<Self> {
        let state = handover.state;
        let (listener, bound) = match (listener, handover.listener) {
            (Some(listener), _) => (listener, false),
            (None, Some(listener)) => (listener, true),
            (None, None) => bail!("Nothing to listen on: no socket was handed over or given."),
        };
        let (listener, unix) = Self::listener(listener)?;
        let address = Address::new(Transport::Unix(unix)).set_guid(state.guid.clone())?;
        let guid = state.guid;
        let last_id = usize::try_from(state.last_id)?;
        let peers = Peers::new();
        // Before the bus has the clients of the hand-over, which it serves as soon as it has them.
        peers.set_policy(policy).await;
        let us = Self::us(&peers, &guid, handover.own).await?;
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
        let service_conn = peers
            .restore(snapshot, clients, send_timeout, Some(us))
            .await?
            .context("The bus was not added to its peers")?;
        let mut bus = Self::with_peers(listener, address, guid, bound, peers, service_conn);
        bus.inner.next_id = last_id;
        bus.set_send_timeout(send_timeout);

        Ok(bus)
    }
}

/// What a bus hands over to the instance of the bus that takes over from it.
///
/// [`Bus::hand_over`] stops a bus once it is done with what it took, and [`Bus::take_over`]
/// creates the next instance out of what it handed over, which serves the same clients on the same
/// connections, with the same names, match rules and calls that wait for replies, and goes on
/// with what was on its way between the bus and its clients. The clients notice nothing but a
/// pause.
#[derive(Debug)]
pub struct Handover {
    /// The state of the bus: its GUID, and its peers, monitors and names.
    pub(crate) state: State,
    /// The socket that the bus listened on, unless it was given one, as by systemd's socket
    /// activation, which then gives the next instance the socket too.
    pub listener: Option<OwnedFd>,
    /// The clients, peers and monitors.
    pub clients: Vec<ClientHandover>,
    /// The connection on which the bus served the D-Bus API: none in what an instance that handed
    /// none over stored, which the next instance serves the API on a new one for.
    pub(crate) own: Option<OwnHandover>,
}

/// What a bus hands over of the connection on which it serves the D-Bus API as a peer of its own:
/// the two ends of it, which a channel in the process of the bus joins.
///
/// The bus answers the calls of its clients on one end, and its peer, which the bus forwards the
/// messages of as those of any peer, has the other. What is on its way between the two as the bus
/// hands over goes with them: the calls to the bus that it didn't take, and what the bus sent,
/// replies and signals, and didn't pass on to its clients.
#[derive(Debug)]
pub struct OwnHandover {
    /// The end on which the bus answers the calls to it.
    pub(crate) service: connection::Handover,
    /// The end of the peer of the bus.
    pub(crate) peer: connection::Handover,
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
    /// The state, encoded in the D-Bus wire format, after its version, as the bus passes it on to
    /// its next instance in another process.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let data = to_bytes(serialized::Context::new(LE, 0), &(VERSION, self))?;

        Ok(data.bytes().to_vec())
    }

    /// The state that `bytes` encode, as [`State::encode`] encodes it, and whether the bus that
    /// encoded it handed the connection on which it served the D-Bus API over along with it: one
    /// of `VERSION_WITHOUT_OWN` didn't, as it made sure that nothing was on its way on that
    /// connection.
    pub(crate) fn decode(bytes: &[u8]) -> Result<(Self, bool)> {
        let data = serialized::Data::new(bytes, serialized::Context::new(LE, 0));
        let (version, _) = data
            .deserialize::<u32>()
            .context("The state that was handed over has no version")?;
        let with_own = match version {
            VERSION => true,
            VERSION_WITHOUT_OWN => false,
            _ => bail!("The state that was handed over is of version {version}, not {VERSION}."),
        };
        let ((_, state), _) = data
            .deserialize::<(u32, Self)>()
            .context("Failed to decode the state that was handed over")?;

        Ok((state, with_own))
    }
}

/// A peer, as the bus hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct PeerState {
    /// The number of the peer, which its unique name carries.
    pub(crate) id: u64,
    /// Whether the peer called `Hello`.
    pub(crate) greeted: bool,
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
    pub(crate) match_rules: MatchRules,
}

/// A well-known name, as the bus hands it over: its owner first, then those that queue for it, in
/// order, each with whether it lets another take the name over from it, and whether it doesn't
/// queue for the name once that happens.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct NameState {
    pub(crate) name: OwnedWellKnownName,
    pub(crate) owners: Vec<(OwnedUniqueName, bool, bool)>,
}

/// The version of the state: an instance of the bus of another version doesn't take over from
/// this one, nor this one from it, but for the version before this one.
const VERSION: u32 = 2;
/// The version of the state before the bus handed over the connection on which it serves the D-Bus
/// API, which is encoded as this one is. A bus of that version doesn't take over from this one, as
/// it would lose what is on its way on that connection, while this one takes over from it, as that
/// bus made sure that nothing was on its way there.
const VERSION_WITHOUT_OWN: u32 = 1;

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
                match_rules,
                pending_calls: vec![(2, name(2), 7)],
            }],
            monitors: vec![MonitorState {
                id: 3,
                match_rules: MatchRules::default(),
            }],
            names: vec![NameState {
                name: OwnedWellKnownName::try_from("org.zbus.Test").unwrap(),
                owners: vec![(name(1), false, true), (name(2), true, false)],
            }],
        };

        let encoded = state.encode().unwrap();
        let (decoded, with_own) = State::decode(&encoded).unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{state:?}"));
        assert!(with_own);

        let mut other = encoded.clone();
        let version = VERSION + 1;
        other[..4].copy_from_slice(&version.to_le_bytes());
        let error = State::decode(&other).unwrap_err();
        let expected = format!("version {version}");
        assert!(error.to_string().contains(&expected), "{error}");
    }

    /// A bus takes over from the state of a bus of the version before this one, which came
    /// without the connection on which the bus serves the D-Bus API, as that bus made sure that
    /// nothing was on its way there.
    #[test]
    fn state_of_the_version_before_comes_without_the_connection_of_the_bus() {
        let state = State {
            guid: zbus::Guid::generate().into(),
            last_id: 7,
            peers: Vec::new(),
            monitors: Vec::new(),
            names: Vec::new(),
        };
        let mut encoded = state.encode().unwrap();
        encoded[..4].copy_from_slice(&VERSION_WITHOUT_OWN.to_le_bytes());

        let (decoded, with_own) = State::decode(&encoded).unwrap();

        assert_eq!(format!("{decoded:?}"), format!("{state:?}"));
        assert!(!with_own);
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
