use std::{
    collections::{HashMap, VecDeque},
    iter,
};
use zbus::{
    fdo::{ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::{
        BusName, OwnedBusName, OwnedUniqueName, OwnedWellKnownName, UniqueName, WellKnownName,
    },
};

use crate::fdo;

#[derive(Debug)]
pub struct NameRegistry {
    names: HashMap<OwnedWellKnownName, NameEntry>,
    /// The current and peak name counts of each owner, kept up to date as names change owners, so
    /// that neither takes a pass over the names. An entry stays after its owner released its last
    /// name, to preserve the peak: `release_all` removes it, when the peer disconnects or becomes
    /// a monitor.
    owned: HashMap<OwnedUniqueName, Owned>,
    /// The well-known names that each peer owns or queues for, each once, by its unique name: what
    /// the policy knows the peer by. A peer without any has no entry. It is kept up to date as
    /// names change hands, so that telling the names of a peer takes no pass over the names.
    names_of: HashMap<OwnedUniqueName, Vec<OwnedWellKnownName>>,
    /// The most names that had an owner at a time.
    peak_names: usize,
    /// The most names that one owner had at a time.
    peak_names_per_owner: usize,
}

/// A registry without names, but for the one that the bus itself has: its unique name and its
/// well-known name are both `org.freedesktop.DBus`, which it has without asking for it, so that
/// name is in `names_of` and not in `names`.
impl Default for NameRegistry {
    fn default() -> Self {
        let bus = UniqueName::from_static_str_unchecked(fdo::BUS_NAME).into();
        let name = WellKnownName::from_static_str_unchecked(fdo::BUS_NAME).into();

        Self {
            names: HashMap::new(),
            owned: HashMap::new(),
            names_of: HashMap::from([(bus, vec![name])]),
            peak_names: 0,
            peak_names_per_owner: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NameEntry {
    owner: NameOwner,
    waiting_list: VecDeque<NameOwner>,
}

impl NameEntry {
    pub fn owner(&self) -> &NameOwner {
        &self.owner
    }

    pub fn waiting_list(&self) -> impl Iterator<Item = &NameOwner> {
        self.waiting_list.iter()
    }
}

#[derive(Clone, Debug)]
pub struct NameOwner {
    unique_name: OwnedUniqueName,
    allow_replacement: bool,
    do_not_queue: bool,
}

impl NameOwner {
    /// An owner of a name, or one that queues for it, which lets another take the name over from
    /// it if `allow_replacement`, and doesn't queue for the name once that happens if
    /// `do_not_queue`.
    pub fn new(unique_name: OwnedUniqueName, allow_replacement: bool, do_not_queue: bool) -> Self {
        Self {
            unique_name,
            allow_replacement,
            do_not_queue,
        }
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    /// Whether the owner lets another take the name over from it.
    pub fn allow_replacement(&self) -> bool {
        self.allow_replacement
    }

    /// Whether the owner doesn't queue for the name once another takes it over from it.
    pub fn do_not_queue(&self) -> bool {
        self.do_not_queue
    }
}

/// The names of one owner.
#[derive(Clone, Copy, Debug, Default)]
pub struct Owned {
    /// How many names the owner has.
    pub now: usize,
    /// The most names that the owner had at a time.
    pub peak: usize,
}

impl NameRegistry {
    pub async fn request_name(
        &mut self,
        name: WellKnownName<'_>,
        unique_name: UniqueName<'_>,
        flags: RequestNameFlags,
    ) -> (RequestNameReply, Option<NameOwnerChanged>) {
        match self.names.get_mut(&*name) {
            Some(entry) => {
                if entry.owner.unique_name == unique_name {
                    // The flags of an owner are those of its last request.
                    entry.owner.allow_replacement =
                        flags.contains(RequestNameFlags::ALLOW_REPLACEMENT);
                    entry.owner.do_not_queue = flags.contains(RequestNameFlags::DO_NOT_QUEUE);

                    (RequestNameReply::AlreadyOwner, None)
                } else if flags.contains(RequestNameFlags::REPLACE_EXISTING)
                    && entry.owner.allow_replacement
                {
                    let unique_name = OwnedUniqueName::from(unique_name.clone());
                    let new_owner = NameOwner::new(
                        unique_name.clone(),
                        flags.contains(RequestNameFlags::ALLOW_REPLACEMENT),
                        flags.contains(RequestNameFlags::DO_NOT_QUEUE),
                    );
                    let replaced = std::mem::replace(&mut entry.owner, new_owner);
                    let old_owner = replaced.unique_name.clone();
                    // It has the name now, so it queues for it no more: left in the queue, it
                    // would get the name again later, even once it is gone.
                    entry
                        .waiting_list
                        .retain(|waiting| waiting.unique_name != unique_name);
                    // The one that it took the name from is first in line to get it back, unless
                    // it asked not to queue.
                    if !replaced.do_not_queue {
                        entry.waiting_list.push_front(replaced);
                    }
                    self.note_lost(&old_owner);
                    self.note_gained(&unique_name);
                    self.index(&old_owner, &name);
                    self.index(&unique_name, &name);

                    (
                        RequestNameReply::PrimaryOwner,
                        Some(NameOwnerChanged {
                            name: BusName::from(name).into(),
                            old_owner: Some(old_owner),
                            new_owner: Some(unique_name),
                        }),
                    )
                } else if !flags.contains(RequestNameFlags::DO_NOT_QUEUE) {
                    let allow_replacement = flags.contains(RequestNameFlags::ALLOW_REPLACEMENT);
                    // A peer is in the queue once, however many times it asks: a second entry
                    // would outlast the release of the first, and get the name for a peer that
                    // let go of it, or that is gone.
                    let queued = entry
                        .waiting_list
                        .iter_mut()
                        .find(|waiting| waiting.unique_name == unique_name);
                    match queued {
                        Some(queued) => queued.allow_replacement = allow_replacement,
                        None => entry.waiting_list.push_back(NameOwner::new(
                            OwnedUniqueName::from(unique_name.clone()),
                            allow_replacement,
                            false,
                        )),
                    }
                    self.index(&unique_name, &name);

                    (RequestNameReply::InQueue, None)
                } else {
                    (RequestNameReply::Exists, None)
                }
            }
            None => {
                let unique_name = OwnedUniqueName::from(unique_name.clone());
                let name = OwnedWellKnownName::from(name);
                let owner = NameOwner::new(
                    unique_name.clone(),
                    flags.contains(RequestNameFlags::ALLOW_REPLACEMENT),
                    flags.contains(RequestNameFlags::DO_NOT_QUEUE),
                );

                self.names.insert(
                    name.clone(),
                    NameEntry {
                        owner,
                        waiting_list: VecDeque::new(),
                    },
                );
                self.note_gained(&unique_name);
                self.index(&unique_name, &name);

                (
                    RequestNameReply::PrimaryOwner,
                    Some(NameOwnerChanged {
                        name: BusName::from(name.into_inner()).into(),
                        old_owner: None,
                        new_owner: Some(unique_name),
                    }),
                )
            }
        }
    }

    pub async fn release_name(
        &mut self,
        name: WellKnownName<'_>,
        owner: UniqueName<'_>,
    ) -> (ReleaseNameReply, Option<NameOwnerChanged>) {
        match self.names.get_mut(name.as_str()) {
            Some(entry) => {
                if *entry.owner.unique_name == owner {
                    let owner = entry.owner.unique_name.clone();
                    let new_owner_name = match entry.waiting_list.pop_front() {
                        Some(next) => {
                            entry.owner = next;

                            Some(entry.owner.unique_name.clone())
                        }
                        None => {
                            self.names.remove(name.as_str());

                            None
                        }
                    };
                    // The loss first: the name may pass on to the one that lets go of it, which
                    // queued for it too, and it has the name once, not twice.
                    self.note_lost(&owner);
                    if let Some(new_owner) = &new_owner_name {
                        self.note_gained(new_owner);
                    }
                    // The one that the name passes on to queued for it, so it has the name already.
                    self.index(&owner, &name);

                    (
                        ReleaseNameReply::Released,
                        Some(NameOwnerChanged {
                            name: BusName::from(name).into(),
                            old_owner: Some(owner),
                            new_owner: new_owner_name,
                        }),
                    )
                } else {
                    for (i, waiting) in entry.waiting_list.iter().enumerate() {
                        if *waiting.unique_name == owner {
                            entry.waiting_list.remove(i);
                            self.index(&owner, &name);

                            return (ReleaseNameReply::Released, None);
                        }
                    }

                    (ReleaseNameReply::NonExistent, None)
                }
            }
            None => (ReleaseNameReply::NonExistent, None),
        }
    }

    pub async fn release_all(&mut self, owner: UniqueName<'_>) -> Vec<NameOwnerChanged> {
        // Find all names registered and queued by the given owner.
        let names: Vec<_> = self
            .names
            .iter()
            .filter_map(|(name, entry)| {
                if *entry.owner.unique_name == owner
                    || entry
                        .waiting_list
                        .iter()
                        .any(|waiting| *waiting.unique_name == owner)
                {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();
        // Now release our claim or waiting list tickets from all these names.
        let mut all_changed = vec![];
        for name in names {
            let (_, changed) = self.release_name(name.inner().clone(), owner.clone()).await;
            if let Some(changed) = changed {
                all_changed.push(changed);
            }
        }
        self.owned.remove(owner.as_str());

        all_changed
    }

    /// The well-known names that `peer` owns or queues for: what the policy knows it by.
    pub fn names_of(&self, peer: &str) -> &[OwnedWellKnownName] {
        self.names_of.get(peer).map_or(&[], Vec::as_slice)
    }

    /// The unique name of the owner of `name`, if it has one.
    pub fn lookup(&self, name: &WellKnownName<'_>) -> Option<&OwnedUniqueName> {
        self.names.get(name.as_str()).map(|e| &e.owner.unique_name)
    }

    pub fn all_names(&self) -> &HashMap<OwnedWellKnownName, NameEntry> {
        &self.names
    }

    /// Give `name` to `owner`, with `waiting_list` queuing for it, in that order, as another
    /// instance of the bus had it when it handed over.
    pub fn insert(
        &mut self,
        name: OwnedWellKnownName,
        owner: NameOwner,
        waiting_list: impl IntoIterator<Item = NameOwner>,
    ) {
        let owner_name = owner.unique_name.clone();
        let entry = NameEntry {
            owner,
            waiting_list: waiting_list.into_iter().collect(),
        };
        let peers: Vec<_> = iter::once(&entry.owner)
            .chain(&entry.waiting_list)
            .map(|peer| peer.unique_name.clone())
            .collect();
        self.names.insert(name.clone(), entry);
        self.note_gained(&owner_name);
        for peer in &peers {
            self.index(peer, &name);
        }
    }

    /// The most names that had an owner at a time.
    pub fn peak_names(&self) -> usize {
        self.peak_names
    }

    /// The most names that one owner had at a time.
    pub fn peak_names_per_owner(&self) -> usize {
        self.peak_names_per_owner
    }

    /// How many names `owner` has, and the most that it had at a time.
    pub fn owned_by(&self, owner: &str) -> Owned {
        self.owned.get(owner).copied().unwrap_or_default()
    }

    pub fn waiting_list(
        &self,
        name: WellKnownName<'_>,
    ) -> Option<impl Iterator<Item = &NameOwner>> {
        self.names.get(name.as_str()).map(|e| e.waiting_list.iter())
    }

    /// Note that `owner` gained a name that it did not have, for the peaks.
    fn note_gained(&mut self, owner: &OwnedUniqueName) {
        let owned = self.owned.entry(owner.clone()).or_default();
        owned.now += 1;
        owned.peak = owned.peak.max(owned.now);
        self.peak_names_per_owner = self.peak_names_per_owner.max(owned.now);
        self.peak_names = self.peak_names.max(self.names.len());
    }

    /// Note that `owner` lost a name that it had.
    fn note_lost(&mut self, owner: &OwnedUniqueName) {
        if let Some(owned) = self.owned.get_mut(owner) {
            owned.now = owned.now.saturating_sub(1);
        }
    }

    /// Note whether `peer` owns or queues for `name`, as that may have changed.
    ///
    /// Only `name` is looked at, as the other names of `peer` did not change. The entry of the bus
    /// itself is left as [`NameRegistry::default`] made it: the bus has its name without it being
    /// in `names`, and asks for no other.
    fn index(&mut self, peer: &UniqueName<'_>, name: &WellKnownName<'_>) {
        if peer == fdo::BUS_NAME {
            return;
        }
        let has = self.names.get(name.as_str()).is_some_and(|entry| {
            *entry.owner.unique_name == *peer
                || entry
                    .waiting_list
                    .iter()
                    .any(|waiting| *waiting.unique_name == *peer)
        });
        let Some(names) = self.names_of.get_mut(peer.as_str()) else {
            if has {
                let names = vec![name.to_owned().into()];
                self.names_of.insert(peer.to_owned().into(), names);
            }

            return;
        };
        let noted = names.iter().position(|noted| noted == name);
        match (has, noted) {
            (true, None) => names.push(name.to_owned().into()),
            (false, Some(i)) => {
                names.swap_remove(i);
                if names.is_empty() {
                    self.names_of.remove(peer.as_str());
                }
            }
            (true, Some(_)) | (false, None) => (),
        }
    }
}

#[derive(Debug)]
pub struct NameOwnerChanged {
    pub name: OwnedBusName,
    pub old_owner: Option<OwnedUniqueName>,
    pub new_owner: Option<OwnedUniqueName>,
}

#[cfg(test)]
mod tests;
