#[cfg(target_os = "linux")]
use zbus::MessageStream;
use zbus::{names::OwnedUniqueName, Connection};
use zruntime::{Event, EventListener};
#[cfg(target_os = "linux")]
use zruntime::{Shared, Task};

use crate::{match_rules::MatchRules, user::Charge};

use super::Recipient;

/// A client of the bus: a peer, or the monitor that a peer becomes. It holds the client's
/// connection and what the bus needs to serve the client and to hand it over. The bus itself, as a
/// peer of its own, also has one.
#[derive(Debug)]
pub struct Client {
    conn: Connection,
    recipient: Recipient,
    id: usize,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    /// The client as an object that the bus keeps for its user, as in dbus-broker. This holds for
    /// a peer and for the monitor that the peer becomes. `None` for the bus itself.
    _charge: Option<Charge>,
    /// Notified when the client leaves, when it becomes a monitor, and when the bus pauses to hand
    /// over, so that the task that serves the client checks why.
    canceled: Event,
    /// The client's message stream, after the task that served the client gave it back when the
    /// bus paused to hand over.
    #[cfg(target_os = "linux")]
    paused: Option<MessageStream>,
    /// The task that writes to the client what the previous instance of the bus left unwritten,
    /// while that task runs. See [`Client::flush`].
    #[cfg(target_os = "linux")]
    flushing: Option<Task<(), Shared>>,
}

impl Client {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The recipient through which the bus sends messages to the client. It stays the same when a
    /// peer becomes a monitor.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// The client's number. No other client that the bus ever has shares it, and it stays the
    /// same when a peer becomes a monitor. It is 0 for the bus itself, and 1 or more for the other
    /// clients, whose unique names contain it.
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
        self.canceled.listen()
    }

    /// Pause the client while the bus hands over.
    ///
    /// Pausing makes the task that serves the client finish the messages that it already took, and
    /// then give the client's message stream back. From then on, neither a send to the client nor
    /// the hand-over of its connection waits for the client to take what the bus sends it, because
    /// whatever it doesn't take goes along with the hand-over. Pausing also stops the write of what
    /// the previous instance of the bus left unwritten, if that write is still under way
    /// ([`Client::flush`]). The bus itself pauses in the same way as its clients.
    #[cfg(target_os = "linux")]
    pub(crate) fn pause(&mut self) {
        let prepared = self.conn.prepare_hand_over();
        debug_assert!(prepared, "the connection of a client can't be handed over");
        // The write waits for the client to take it, for up to the time that the client has to
        // take a message. Meanwhile it holds the turn to write, and the sends to the client and
        // the hand-over of its connection wait for that turn. Dropping the write's task releases
        // the turn and leaves what the write didn't write to those sends and to the hand-over.
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

    /// The client's message stream, if the task that served the client gave it back when the bus
    /// paused to hand over. That task may have served the client as a peer or as the monitor that
    /// the peer became.
    #[cfg(target_os = "linux")]
    pub(crate) fn paused(&mut self) -> &mut Option<MessageStream> {
        &mut self.paused
    }

    /// Create the client numbered `id` and named `unique_name`, on `conn`. The bus sends its
    /// messages through `recipient`. The client has `match_rules`, and `charge` counts the client
    /// as an object of its user.
    pub(super) fn new(
        conn: Connection,
        recipient: Recipient,
        id: usize,
        unique_name: OwnedUniqueName,
        match_rules: MatchRules,
        charge: Option<Charge>,
    ) -> Self {
        Self {
            conn,
            recipient,
            id,
            unique_name,
            match_rules,
            _charge: charge,
            canceled: Event::new(),
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
        self.canceled.notify(usize::MAX);
    }

    /// Make `flushing` the task that writes to the client what is left to write, as
    /// [`Client::flush`] does.
    #[cfg(target_os = "linux")]
    pub(super) fn set_flushing(&mut self, flushing: Task<(), Shared>) {
        self.flushing = Some(flushing);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.notify_canceled();
    }
}

/// The unique name of the client numbered `id`.
pub(crate) fn unique_name(id: usize) -> OwnedUniqueName {
    OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap()
}
