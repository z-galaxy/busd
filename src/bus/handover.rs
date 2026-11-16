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
    /// The bus pauses: it stops taking the messages of each client, once it is done with those that
    /// it took, and forwards those. Then it finishes its own chores, such as its clean-up after the
    /// clients that went meanwhile, and stops taking the calls to itself: it answers those that it
    /// has under way, and sends the signals that follow their replies, such as those of the names
    /// that change owners, without waiting for its clients to take what it sends them.
    ///
    /// The bus hands over the connection on which it serves the D-Bus API as well, with whatever is
    /// on its way between it and its clients by then: the calls to the bus that it didn't take, and
    /// what it sent and didn't pass on. The next instance answers those calls and passes the rest
    /// on, each once and in order.
    ///
    /// The connection of each client then goes to the next instance, with the messages that zbus
    /// read of the client and that the bus didn't take, what it read of a message that the client
    /// is in the middle of sending, if any, what the bus sent the client and the client didn't
    /// take, and the file descriptors that came with those, for the next instance to go on from
    /// there: it writes the client what the client didn't take first. What the clients send
    /// meanwhile waits for the next instance too, as do the clients that connect, in the socket
    /// that the bus listens on. A client that goes meanwhile, by the time that the bus is done with
    /// what it took, has what it sent before it went taken, and the bus cleans up after it, as
    /// after any client that goes, as its connection may have ended on an error, or been closed by
    /// the bus, as a send to the client failed, neither of which can be handed over. One that goes
    /// after that, as it closes its end, is handed over with what it sent, for the next instance to
    /// pass that on, and clean up after it. The clients still in the middle of their handshakes
    /// have [`STOP_TIMEOUT`] to finish them, and get disconnected if they don't.
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
        peers.settle().await;
        // The bus's own connection goes first: if that fails, the bus still has its clients, and
        // disconnects them as it goes.
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
    /// The bus serves the D-Bus API on the connection of the previous instance, if `handover` has
    /// it. Once its clients, their names and their match rules are back, it answers the calls
    /// that the previous instance didn't take, and passes on what that instance sent and didn't
    /// pass on, before it does anything else.
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
        let service_conn = peers.restore(snapshot, clients, send_timeout, us).await?;
        let mut bus = Self::with_peers(listener, address, guid, bound, peers, service_conn);
        bus.inner.next_id = last_id;
        bus.set_send_timeout(send_timeout);

        Ok(bus)
    }
}

/// What a bus hands over to the instance of the bus that takes over from it.
///
/// [`Bus::hand_over`] stops a bus once it is done with what it took. [`Bus::take_over`] creates
/// the next instance out of what it handed over. That instance serves the same clients on the same
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
    /// The connection on which the previous instance served the D-Bus API. `None` in a hand-over
    /// of a version that came without it: the next instance then serves the API on a new
    /// connection.
    pub(crate) own: Option<OwnHandover>,
}

/// The hand-over of the connection on which a bus serves the D-Bus API: both of its ends.
///
/// The bus serves the API as a peer of its own, over a channel in its process. It answers the
/// calls to it on one end. Its peer has the other end, and the bus forwards the messages of that
/// peer as it does those of any client. Whatever is on its way between the two ends as the bus
/// hands over goes with them: the calls to the bus that it didn't take, and the replies and
/// signals that it sent and didn't pass on to its clients.
#[derive(Debug)]
pub struct OwnHandover {
    /// The end on which the bus answers the calls to it.
    pub(crate) service: connection::Handover,
    /// The other end, which the peer of the bus has.
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
    /// The state in the D-Bus wire format, after its version, for a next instance in another
    /// process.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let data = to_bytes(serialized::Context::new(LE, 0), &(VERSION, self))?;

        Ok(data.bytes().to_vec())
    }

    /// The state that `bytes` hold, as [`State::encode`] encoded it, and whether its bus handed
    /// over the connection on which it served the D-Bus API as well. A bus of
    /// `VERSION_WITHOUT_OWN` didn't: it made sure that nothing was on its way on that connection.
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

/// The version of the state. A bus takes over from a bus of the same version, or of the version
/// before it, and from no other.
const VERSION: u32 = 2;
/// The version of the state before the bus handed over the connection on which it serves the
/// D-Bus API. Its state is encoded in the same way. This bus takes over from a bus of that
/// version, which made sure that nothing was on its way on that connection. A bus of that version
/// doesn't take over from this one: it would lose what is on its way there.
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

    /// A bus and the instance that takes over from it share only the bytes of the state, and the
    /// two may run different versions of busd. A state of this version encodes to these bytes and
    /// decodes from them, however the types that hold it change. Changing the encoding requires a
    /// new version.
    #[test]
    fn encoding_of_the_state_is_pinned() {
        let name = |id: u32| OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();
        // At most one rule per peer, because a peer's rules are in no particular order.
        let rules = |rule: &str| {
            let mut rules = MatchRules::default();
            rules.add(OwnedMatchRule::try_from(rule).unwrap());

            rules
        };
        let state = State {
            guid: zbus::Guid::from_static_str("0123456789abcdef0123456789abcdef")
                .unwrap()
                .into(),
            last_id: 3,
            peers: vec![
                PeerState {
                    id: 1,
                    greeted: true,
                    match_rules: rules("type='signal',sender='org.zbus.Test'"),
                    pending_calls: vec![(2, name(2), 7)],
                },
                PeerState {
                    id: 2,
                    greeted: false,
                    match_rules: MatchRules::default(),
                    pending_calls: Vec::new(),
                },
            ],
            monitors: vec![MonitorState {
                id: 3,
                match_rules: rules("type='method_call'"),
            }],
            names: vec![NameState {
                name: OwnedWellKnownName::try_from("org.zbus.Test").unwrap(),
                owners: vec![(name(1), false, true), (name(2), true, false)],
            }],
        };

        let encoded = state.encode().unwrap();
        let hex: String = encoded.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, ENCODED);

        let (decoded, with_own) = State::decode(&encoded).unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{state:?}"));
        assert!(with_own);
    }

    /// A bus takes over from a state of the version before this one. That version came without
    /// the connection on which the bus serves the D-Bus API, as its bus made sure that nothing was
    /// on its way there.
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

    /// The state of `encoding_of_the_state_is_pinned`, encoded by version 2, in hexadecimal.
    const ENCODED: &str = concat!(
        "02000000000000002000000030313233343536373839616263646566303132333435363738396162",
        "63646566000000000300000000000000700000000000000001000000000000000100000029000000",
        "24000000747970653d277369676e616c272c73656e6465723d276f72672e7a6275732e5465737427",
        "00000000180000000200000000000000070000003a627573642e3200070000000200000000000000",
        "00000000000000000000000000000000230000000000000003000000000000001700000012000000",
        "747970653d276d6574686f645f63616c6c270000440000000d0000006f72672e7a6275732e546573",
        "740000002c000000070000003a627573642e3100000000000100000000000000070000003a627573",
        "642e32000100000000000000",
    );
}
