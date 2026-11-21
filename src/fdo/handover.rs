//! What the bus takes over from busd 0.8, which served the D-Bus API on a connection to itself,
//! over a channel in its process, and handed that connection over too, both of its ends, with what
//! was on its way between the bus and its clients: the calls to the bus that it didn't take, and
//! what it sent and didn't pass on.

use std::time::Duration;

use futures_util::{future::join4, StreamExt};
use zbus::{
    connection::{self, socket::ChannelHandover},
    message,
    names::{OwnedUniqueName, UniqueName},
    Connection, Message, MessageStream,
};
use zruntime::{mpmc, SharedRuntime};

use super::{Driver, BUS_NAME};
use crate::{peer::checked, peers::Peers, warn};

/// What was on its way between an instance of busd 0.8 and its clients, on the connection on which
/// that bus served the D-Bus API, as it handed the connection over: what that bus sent and didn't
/// pass on, and the calls to it that it didn't take, each in order.
///
/// The bus that takes over passes on what that instance sent before anything else that it sends,
/// and answers the calls that that instance didn't take, in order, before any other call: they were
/// on their way before any call that a client makes once the bus took over.
#[derive(Debug, Default)]
pub(crate) struct OnItsWay {
    /// What the bus sent and didn't pass on, each with the bus for its sender, as the bus passes
    /// on what it sends.
    pub(crate) sent: Vec<Message>,
    /// The calls to the bus that it didn't take.
    pub(crate) calls: Vec<Message>,
}

impl OnItsWay {
    /// What was on its way on `channel`, the two ends of the connection that an instance of busd
    /// 0.8 handed over, which go on over a channel of their own until the bus has taken all of it.
    /// Fails if either end fails to go on.
    ///
    /// Each end has what the other one wrote and it didn't read, and keeps what it didn't write
    /// itself, which it writes ahead of what it sends. So each end sends a message of its own to
    /// the other one, which comes to the other one after what this one didn't write, and tells it
    /// that it has all of that. The two ends write and read at once, as the channel holds no more
    /// than a few messages. Nothing but the two ends takes part in that, so it takes no time unless
    /// something is wrong, as when an end fails to send its message: the bus gives up on it after
    /// [`DRAIN_TIMEOUT`] then, and drops all that was on its way, rather than wait for ever.
    pub(crate) async fn take_over(channel: ChannelHandover) -> zbus::Result<Self> {
        // In the order that busd 0.8 put them together in.
        let (service, peer) = channel.into_ends();
        let service = connection::Builder::take_over(service)
            .build_message_stream()
            .await?;
        let peer = connection::Builder::take_over(peer)
            .build_message_stream()
            .await?;
        let runtime = SharedRuntime::current()?;
        let Ok((sent, calls)) = runtime.timeout(DRAIN_TIMEOUT, drain(service, peer)).await else {
            warn!(
                "Dropping what was on its way to and from the bus as it was handed over, as it \
                 couldn't be taken"
            );

            return Ok(Self::default());
        };
        let bus: OwnedUniqueName = UniqueName::from_static_str_unchecked(BUS_NAME).into();

        Ok(Self {
            sent: sent
                .into_iter()
                .filter_map(|msg| from_bus(msg, &bus))
                .collect(),
            calls: calls
                .into_iter()
                .filter(|msg| msg.message_type() == message::Type::MethodCall)
                .collect(),
        })
    }
}

/// The calls to the bus that an instance of busd 0.8 didn't take, which the bus answers once it
/// passed on what that instance sent: see [`Calls::take_over`](super::Calls::take_over).
#[derive(Debug)]
pub(crate) struct TakenOver {
    /// The calls, in order.
    pub(super) calls: Vec<Message>,
    /// Closed, or given a message, once the bus passed on what that instance sent, and cleaned up
    /// after the clients that went meanwhile: the bus answers the calls only then.
    pub(super) held: mpmc::Receiver<()>,
}

impl Driver {
    /// Answer the calls of `taken_over`, in order, once it lets the bus go on.
    pub(super) async fn take_over(&mut self, peers: &Peers, taken_over: TakenOver) {
        let TakenOver { calls, held } = taken_over;
        // Either way, the bus goes on.
        let _ = held.recv().await;
        for call in &calls {
            self.answer(peers, call).await;
        }
    }
}

/// What is on its way between `service`, the end on which the bus answered the calls to it, and
/// `peer`, the end of the peer of the bus: what the bus sent and didn't pass on, and the calls to
/// the bus that it didn't take, each in order, as [`OnItsWay::take_over`] takes them.
async fn drain(
    mut service: MessageStream,
    mut peer: MessageStream,
) -> (Vec<Message>, Vec<Message>) {
    let (service_conn, peer_conn) = (Connection::from(&service), Connection::from(&peer));
    let (Ok(to_peer), Ok(to_service)) = (end(), end()) else {
        warn!("Failed to build the messages that end what the bus was handed over with");

        return (Vec::new(), Vec::new());
    };
    let (to_peer_sent, to_service_sent, sent, calls) = join4(
        service_conn.send(&to_peer),
        peer_conn.send(&to_service),
        until(&mut peer, &to_peer),
        until(&mut service, &to_service),
    )
    .await;
    for (written, what) in [
        (to_peer_sent, "what the bus sent"),
        (to_service_sent, "the calls to the bus"),
    ] {
        if let Err(e) = written {
            warn!("Failed to pass {what} on, which it was handed over with: {e}");
        }
    }

    (sent, calls)
}

/// What `stream` yields, up to `end`, which the other end sent after all that is on its way.
async fn until(stream: &mut MessageStream, end: &Message) -> Vec<Message> {
    let mut msgs = Vec::new();
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(msg) if msg.data().bytes() == end.data().bytes() => return msgs,
            Ok(msg) => msgs.push(msg),
            Err(e) => warn!("Dropping a message that the bus was handed over with: {e}"),
        }
    }
    warn!("The connection that the bus was handed over with ended early");

    msgs
}

/// `msg`, which the previous instance of the bus sent, `bus`, and didn't pass on, as the bus passes
/// its own messages on: with `bus` for its sender, which the errors of busd 0.8 lacked, as the
/// object server of zbus sends them without one, unless the bus can't pass it on, as when it is a
/// call, which the bus never makes, or of another sender.
fn from_bus(msg: Message, bus: &OwnedUniqueName) -> Option<Message> {
    if msg.message_type() == message::Type::MethodCall {
        warn!("Dropping a call that the previous instance of the bus sent, which it never does");

        return None;
    }

    match checked(msg, bus) {
        Ok(msg) => Some(msg),
        Err(e) => {
            warn!("Dropping what the previous instance of the bus sent: {e}");

            None
        }
    }
}

/// A message that an end sends the other one after what is on its way, as
/// [`OnItsWay::take_over`] says. No message on the way has its bytes: none of those is a signal on
/// [`PATH`], which only busd has, and a serial is only ever given to one message.
fn end() -> zbus::Result<Message> {
    Message::signal(PATH, INTERFACE, "End").build(&())
}

/// How long the bus takes what is on its way between the two ends of the connection of busd 0.8 at
/// most: nothing but the two ends takes part in that, so it takes no time unless something is
/// wrong, as when an end fails to send the message that ends what it holds.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The path of the message that [`end`] builds, which only busd has.
const PATH: &str = "/org/busd/Handover";

/// The interface of the message that [`end`] builds, which only busd has.
const INTERFACE: &str = "org.busd.Handover";
