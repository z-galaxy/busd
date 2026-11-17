use std::{
    collections::{hash_map::Entry, HashMap},
    ops::Range,
};

use futures_util::future::join_all;
use zbus::Message;

use crate::peer::Recipient;

/// The messages of a batch that each recipient gets.
///
/// The task that serves a peer keeps its outbox from one batch to the next, as it keeps the vector
/// of the batch, so that the outbox only allocates where a batch takes more room than those before
/// it did, rather than for every batch.
#[derive(Default)]
pub(super) struct Outbox {
    /// Each recipient, by its number, and its share, in the order of their first messages.
    sends: Vec<(usize, Recipient, Share)>,
    /// Where each recipient is in `sends`, by its number.
    ///
    /// The recipients of the first message come up once each, so that they need no lookup, and
    /// they only go in here once the recipients of another message are looked up among them. A
    /// batch of one message, as most are when a peer waits for each reply, needs none at all.
    index: HashMap<usize, usize, foldhash::fast::RandomState>,
}

impl Outbox {
    /// Add the message at `i` in `batch` to the share of `recipient`, whose number is `id`.
    ///
    /// A peer's number is its client's number, [`Client::id`]. A monitor keeps the number of the
    /// peer that it was, so no two recipients share a number.
    ///
    /// [`Client::id`]: crate::peer::Client::id
    pub(super) fn add(&mut self, batch: &[Message], id: usize, recipient: &Recipient, i: usize) {
        if i > 0 {
            for (at, (id, ..)) in self.sends.iter().enumerate().skip(self.index.len()) {
                self.index.insert(*id, at);
            }
            match self.index.entry(id) {
                Entry::Occupied(at) => {
                    self.sends[*at.get()].2.add(batch, i);

                    return;
                }
                Entry::Vacant(at) => {
                    at.insert(self.sends.len());
                }
            }
        }

        self.sends
            .push((id, recipient.clone(), Share::Run(i..i + 1)));
    }

    /// Send each recipient its share of `batch`, all of them at once, and empty the outbox for the
    /// next batch.
    ///
    /// Returns the messages that their recipients can't take, for their file descriptors, along
    /// with each recipient.
    ///
    /// The outbox lets go of the recipients, and the messages of their shares, as soon as they are
    /// sent, rather than with the next batch, which may be a long time coming: a recipient holds
    /// the connection of its client, which stays open for as long as it is held, and a message the
    /// file descriptors that it carries.
    pub(super) async fn send(&mut self, batch: &[Message]) -> Vec<(Recipient, Message)> {
        let dropped = match &self.sends[..] {
            [] => Vec::new(),
            [(_, recipient, share)] => {
                let dropped = recipient.send(share.of(batch)).await;

                with_recipient(recipient, dropped)
            }
            sends => {
                let all = sends
                    .iter()
                    .map(|(_, recipient, share)| recipient.send(share.of(batch)));
                let all = join_all(all).await;

                sends
                    .iter()
                    .zip(all)
                    .flat_map(|((_, recipient, _), dropped)| with_recipient(recipient, dropped))
                    .collect()
            }
        };
        self.sends.clear();
        self.index.clear();

        dropped
    }
}

/// `msgs`, each with `recipient`.
fn with_recipient(recipient: &Recipient, msgs: Vec<Message>) -> Vec<(Recipient, Message)> {
    msgs.into_iter()
        .map(|msg| (recipient.clone(), msg))
        .collect()
}

/// The messages of a batch that a recipient gets.
enum Share {
    /// The messages in this range of the batch, one after the other, as most shares are.
    Run(Range<usize>),
    /// Messages that aren't all next to each other in the batch.
    Picked(Vec<Message>),
}

impl Share {
    /// Add the message at `i` in `batch`, which comes after those in the share.
    fn add(&mut self, batch: &[Message], i: usize) {
        match self {
            Self::Run(run) if run.end == i => run.end += 1,
            Self::Run(run) => {
                let mut picked = batch[run.clone()].to_vec();
                picked.push(batch[i].clone());
                *self = Self::Picked(picked);
            }
            Self::Picked(picked) => picked.push(batch[i].clone()),
        }
    }

    /// The messages of the share, out of `batch`.
    fn of<'s>(&'s self, batch: &'s [Message]) -> &'s [Message] {
        match self {
            Self::Run(run) => &batch[run.clone()],
            Self::Picked(picked) => picked,
        }
    }
}
