use std::{
    collections::{hash_map::Entry, HashMap},
    mem,
    num::NonZeroU32,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use zbus::names::OwnedUniqueName;

use crate::user::{Charge, User};

/// The method calls to a peer that wait for its reply: the caller and the serial of each.
///
/// The bus records each call that expects a reply as it passes the call on to its callee, as
/// dbus-broker does, and only passes on a reply, or an error, that takes the record of a call away:
/// one from the callee, to the caller, whose reply serial is the serial of the call. So a peer
/// can't pass a reply of its own off as the reply to a call that another peer made of a third one,
/// which a caller that tells replies apart by their serial alone would take it for. The records
/// also let the bus answer the calls that wait on a peer that goes, with an error.
///
/// Each record is an object that the bus keeps for the user of the callee, on behalf of the user of
/// the caller, as with dbus-broker, for as long as it is kept, so that a caller can't have the bus
/// keep records without end, of calls to a peer that never answers. The bus keeps no records of
/// the calls to itself, which it answers itself, as they come in, as dbus-broker keeps none.
///
/// The records of a peer are shared between the peer and the task that serves it, as the peer
/// goes, when it becomes a monitor, before its task does.
#[derive(Debug)]
pub struct PendingCalls {
    calls: Mutex<Calls>,
    /// The user of the peer, which the records are objects of.
    user: Arc<User>,
}

impl PendingCalls {
    /// No calls yet, to a peer of `user`.
    pub fn new(user: Arc<User>) -> Self {
        Self {
            calls: Mutex::default(),
            user,
        }
    }

    /// Record the call of `caller`, whose number is `caller_id`, with `serial`, unless one with
    /// that serial waits already, or the user of `caller` has as many of the objects of the user of
    /// the callee as it may.
    pub fn add(
        &self,
        caller_id: usize,
        caller: &Arc<Caller>,
        serial: NonZeroU32,
    ) -> Result<(), Refusal> {
        match self.calls().entry((caller_id, serial)) {
            Entry::Occupied(_) => Err(Refusal::SerialPending),
            Entry::Vacant(call) => {
                let charge = Charge::new(&self.user, &caller.user).ok_or(Refusal::QuotaExceeded)?;
                call.insert(Call {
                    caller: caller.clone(),
                    _charge: charge,
                });

                Ok(())
            }
        }
    }

    /// Take the record of the call of the caller whose number is `caller_id`, with `serial`, away,
    /// if there is one, in which case it's `true`.
    pub fn remove(&self, caller_id: usize, serial: NonZeroU32) -> bool {
        self.calls().remove(&(caller_id, serial)).is_some()
    }

    /// Forget the calls of the caller whose number is `caller_id`, which goes.
    pub fn forget(&self, caller_id: usize) {
        self.calls().retain(|(id, _), _| *id != caller_id);
    }

    /// Record the call of `caller`, whose number is `caller_id`, with `serial`, which another
    /// instance of the bus handed over, whatever the quota of the user of the callee: that instance
    /// kept all of the records that it handed over at once.
    #[cfg(target_os = "linux")]
    pub(crate) fn add_handed_over(
        &self,
        caller_id: usize,
        caller: &Arc<Caller>,
        serial: NonZeroU32,
    ) {
        let charge = Charge::taken_over(&self.user, &caller.user);
        self.calls().insert(
            (caller_id, serial),
            Call {
                caller: caller.clone(),
                _charge: charge,
            },
        );
    }

    /// The records: the number of the caller of each call, its unique name, and the serial of the
    /// call.
    pub fn records(&self) -> impl Iterator<Item = (usize, OwnedUniqueName, NonZeroU32)> {
        let records: Vec<_> = self
            .calls()
            .iter()
            .map(|((caller_id, serial), call)| {
                (*caller_id, call.caller.unique_name.clone(), *serial)
            })
            .collect();

        records.into_iter()
    }

    /// Take all of the records away: the caller and the serial of each call.
    pub fn take(&self) -> impl Iterator<Item = (OwnedUniqueName, NonZeroU32)> {
        mem::take(&mut *self.calls())
            .into_iter()
            .map(|((_, serial), call)| (call.caller.unique_name.clone(), serial))
    }

    fn calls(&self) -> MutexGuard<'_, Calls> {
        // Nothing can panic while the lock is held.
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A peer, as the caller of the calls that wait for replies, on whichever callees.
#[derive(Debug)]
pub struct Caller {
    unique_name: OwnedUniqueName,
    user: Arc<User>,
}

impl Caller {
    /// The peer of `unique_name`, of `user`.
    pub fn new(unique_name: OwnedUniqueName, user: Arc<User>) -> Arc<Self> {
        Arc::new(Self { unique_name, user })
    }

    /// The user of the peer.
    pub fn user(&self) -> &User {
        &self.user
    }
}

/// Why the bus can't record a call as one that waits for its reply, and refuses the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A call of the same caller with the same serial waits on the callee already.
    SerialPending,
    /// The user of the caller has as many of the objects of the user of the callee as it may
    /// already.
    QuotaExceeded,
}

/// The record of each call, by the number of the caller, as [`Peer::id`] has it, and the serial of
/// the call.
///
/// The records are hashed with foldhash rather than with the SipHash of the standard library, which
/// takes several times as long to hash their keys. A caller picks the serials of its calls, but
/// foldhash is seeded at random, so that it can't pick serials whose hashes collide.
///
/// [`Peer::id`]: super::Peer::id
type Calls = HashMap<(usize, NonZeroU32), Call, foldhash::fast::RandomState>;

/// The record of a call that waits for its reply.
#[derive(Debug)]
struct Call {
    caller: Arc<Caller>,
    /// The record, as an object that the bus keeps for the user of the callee.
    _charge: Charge,
}
