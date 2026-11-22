use std::rc::Rc;

use zbus::names::OwnedUniqueName;
use zruntime::{Event, EventListener};
#[cfg(target_os = "linux")]
use zruntime::{Local, Task};

use crate::{connection::Connection, match_rules::MatchRules, user::Charge};
#[cfg(target_os = "linux")]
use crate::{
    connection::{Handover, Reader},
    warn,
};

use super::Recipient;

/// A client of the bus: a peer, or the monitor that a peer becomes. It holds the client's
/// connection and what the bus needs to serve the client and to hand it over.
#[derive(Debug)]
pub struct Client {
    connection: Rc<Connection>,
    recipient: Recipient,
    id: usize,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    /// The client as an object that the bus keeps for its user, as in dbus-broker. This holds for
    /// a peer and for the monitor that the peer becomes.
    _charge: Charge,
    /// Notified when the client leaves, when it becomes a monitor, and when the bus pauses to hand
    /// over, so that the task that serves the client checks why.
    canceled: Canceled,
    /// The reader of the client's messages, after the task that served the client gave it back
    /// when the bus paused to hand over.
    #[cfg(target_os = "linux")]
    paused: Option<Reader>,
    /// The task that writes to the client what the previous instance of the bus left unwritten,
    /// while that task runs. See [`Client::flush`].
    #[cfg(target_os = "linux")]
    flushing: Option<Task<(), Local>>,
}

impl Client {
    /// The bus's end of the client's connection, for writing to the client.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The recipient through which the bus sends messages to the client. It stays the same when a
    /// peer becomes a monitor.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// The client's number. No other client that the bus ever has shares it, and it stays the
    /// same when a peer becomes a monitor. It is 1 or more, since 0 is the number of the bus
    /// itself. The unique name of the client contains it.
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    pub fn match_rules(&self) -> &MatchRules {
        &self.match_rules
    }

    pub fn listen_cancellation(&self) -> EventListener {
        self.canceled.0.listen()
    }

    /// Pause the client while the bus hands over.
    ///
    /// Pausing makes the task that serves the client finish the messages that it already took, and
    /// then give the reader of the client's messages back. From then on, neither a send to the
    /// client nor the hand-over of its connection waits for the client to take what the bus sends
    /// it, because whatever it doesn't take goes along with the hand-over. Pausing also stops the
    /// write of what the previous instance of the bus left unwritten, if that write is still under
    /// way ([`Client::flush`]).
    #[cfg(target_os = "linux")]
    pub(crate) fn pause(&mut self) {
        // Every write that waits for the client stops waiting, including the write of what the
        // previous instance left unwritten. Each write leaves what it didn't write to the
        // hand-over.
        self.connection.pause();
        self.flushing = None;
        self.notify_canceled();
    }

    /// Write to the client what the previous instance of the bus left unwritten, if anything, in a
    /// task of its own, as [`Recipient::flush`] does. The write stops when the bus pauses the
    /// client to hand over in turn ([`Client::pause`]), whether the client is a peer or the monitor
    /// that the peer became, or when the bus lets go of the client.
    #[cfg(target_os = "linux")]
    pub(crate) fn flush(&mut self) {
        self.flushing = self.recipient.flush();
    }

    /// The reader of the client's messages, if the task that served the client gave it back when
    /// the bus paused to hand over. That task may have served the client as a peer or as the
    /// monitor that the peer became.
    #[cfg(target_os = "linux")]
    pub(crate) fn paused(&mut self) -> &mut Option<Reader> {
        &mut self.paused
    }

    /// The hand-over of the client's connection, with the unread bytes of the client's reader.
    ///
    /// This takes the client. The task that served the client gave the reader back when the bus
    /// paused. The unread bytes are those that the reader read and the bus didn't take. There is
    /// no hand-over if the task didn't give the reader back, if the bus is disconnecting the
    /// client, or if the connection fails to be handed over, which disconnects the client.
    ///
    /// Nothing but the client holds its connection once the bus is paused and settled, since
    /// nothing sends to the client then. A connection that something else still holds can't go to
    /// the next instance: the bus closes it, which disconnects the client.
    #[cfg(target_os = "linux")]
    pub(crate) fn hand_over(self) -> Option<Handover> {
        let (connection, reader) = {
            // The client's recipient shares the connection, and so does the task that writes what
            // the previous instance left unwritten, while that task runs. Both go at the end of
            // this scope.
            let Self {
                connection,
                recipient: _recipient,
                flushing: _flushing,
                paused,
                ..
            } = self;

            (connection, paused?)
        };
        let connection = match Rc::try_unwrap(connection) {
            Ok(connection) => connection,
            Err(connection) => {
                warn!(
                    "Disconnecting `{}`, as its connection is still in use as the bus hands it \
                     over",
                    connection.unique_name()
                );
                connection.close();

                return None;
            }
        };

        connection.hand_over(reader)
    }

    /// Create the client numbered `id` on `connection`. The client goes by the unique name of the
    /// connection, and the bus sends messages to the client through the connection. The client has
    /// `match_rules`, and `charge` counts the client as an object of its user.
    pub(super) fn new(
        connection: Rc<Connection>,
        id: usize,
        match_rules: MatchRules,
        charge: Charge,
    ) -> Self {
        Self {
            recipient: Recipient::client(connection.clone()),
            unique_name: connection.unique_name().clone(),
            connection,
            id,
            match_rules,
            _charge: charge,
            canceled: Canceled::default(),
            #[cfg(target_os = "linux")]
            paused: None,
            #[cfg(target_os = "linux")]
            flushing: None,
        }
    }

    /// The client's match rules, as a mutable reference.
    pub(super) fn match_rules_mut(&mut self) -> &mut MatchRules {
        &mut self.match_rules
    }

    /// Notify [`Client::listen_cancellation`]'s listeners when the client leaves, becomes a monitor
    /// or pauses, so that the task that serves the client checks why.
    pub(super) fn notify_canceled(&self) {
        self.canceled.0.notify(usize::MAX);
    }
}

/// The unique name of the client numbered `id`.
pub(crate) fn unique_name(id: usize) -> OwnedUniqueName {
    OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap()
}

/// The cancellation of the task that serves a client.
///
/// It is also notified when the client goes.
#[derive(Debug, Default)]
struct Canceled(Event);

impl Drop for Canceled {
    fn drop(&mut self) {
        self.0.notify(usize::MAX);
    }
}
