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
}

impl NameOwner {
    /// An owner of a name, or one that queues for it, which lets another take the name over from
    /// it if `allow_replacement`.
    pub fn new(unique_name: OwnedUniqueName, allow_replacement: bool) -> Self {
        Self {
            unique_name,
            allow_replacement,
        }
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    /// Whether the owner lets another take the name over from it.
    pub fn allow_replacement(&self) -> bool {
        self.allow_replacement
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
                    (RequestNameReply::AlreadyOwner, None)
                } else if flags.contains(RequestNameFlags::REPLACE_EXISTING)
                    && entry.owner.allow_replacement
                {
                    let old_owner = entry.owner.unique_name.clone();
                    let unique_name = OwnedUniqueName::from(unique_name.clone());
                    entry.owner = NameOwner {
                        unique_name: unique_name.clone(),
                        allow_replacement: flags.contains(RequestNameFlags::ALLOW_REPLACEMENT),
                    };
                    // It has the name now, so it queues for it no more: left in the queue, it
                    // would get the name again later, even once it is gone.
                    entry
                        .waiting_list
                        .retain(|waiting| waiting.unique_name != unique_name);
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
                        None => entry.waiting_list.push_back(NameOwner {
                            unique_name: OwnedUniqueName::from(unique_name.clone()),
                            allow_replacement,
                        }),
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
                let owner = NameOwner {
                    unique_name: unique_name.clone(),
                    allow_replacement: flags.contains(RequestNameFlags::ALLOW_REPLACEMENT),
                };

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
mod tests {
    use super::*;

    /// The peaks follow the names that have an owner, and the names that one owner has, and only
    /// ever go up.
    #[test]
    fn peaks_of_names_taken_and_released() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (0, 0)
            );

            request(
                &mut registry,
                "org.busd.One",
                ":busd.1",
                RequestNameFlags::empty(),
            )
            .await;
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (1, 1)
            );
            request(
                &mut registry,
                "org.busd.Two",
                ":busd.1",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.Three",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (3, 2)
            );

            for name in ["org.busd.One", "org.busd.Two", "org.busd.Three"] {
                let owner = registry.lookup(&name.try_into().unwrap()).unwrap().clone();
                registry
                    .release_name(name.try_into().unwrap(), owner.inner().clone())
                    .await;
            }
            assert_eq!(registry.all_names().len(), 0);
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (3, 2)
            );
        });
    }

    /// A client that queues for a name does not own it, so it does not raise the peaks.
    #[test]
    fn peaks_ignore_queued_names() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            request(
                &mut registry,
                "org.busd.One",
                ":busd.1",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.Two",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.Two",
                ":busd.1",
                RequestNameFlags::empty(),
            )
            .await;

            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (2, 1)
            );
        });
    }

    /// A name that another client takes over counts for its new owner.
    #[test]
    fn peaks_of_names_replaced() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
            request(
                &mut registry,
                "org.busd.Two",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.Three",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (3, 2)
            );

            let (_, changed) = request(
                &mut registry,
                "org.busd.One",
                ":busd.2",
                RequestNameFlags::REPLACE_EXISTING,
            )
            .await;
            assert!(changed.is_some());
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (3, 3)
            );
        });
    }

    /// A name that goes to the first client in its queue as its owner releases it counts for that
    /// client.
    #[test]
    fn peaks_of_names_released_to_the_queue() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            request(
                &mut registry,
                "org.busd.One",
                ":busd.1",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.Two",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            request(
                &mut registry,
                "org.busd.One",
                ":busd.2",
                RequestNameFlags::empty(),
            )
            .await;
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (2, 1)
            );

            let (_, changed) = registry
                .release_name(
                    "org.busd.One".try_into().unwrap(),
                    UniqueName::try_from(":busd.1").unwrap(),
                )
                .await;
            assert!(changed.is_some_and(|changed| changed.new_owner.is_some()));
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (2, 2)
            );
        });
    }

    /// The names that another instance of the bus hands over count too.
    #[test]
    fn peaks_of_names_inserted() {
        let mut registry = NameRegistry::default();
        let owner = |name: &str| NameOwner::new(name.try_into().unwrap(), false);
        for (name, owner_name) in [
            ("org.busd.One", ":busd.1"),
            ("org.busd.Two", ":busd.1"),
            ("org.busd.Three", ":busd.2"),
        ] {
            registry.insert(
                name.try_into().unwrap(),
                owner(owner_name),
                [owner(":busd.3")],
            );
        }

        assert_eq!(
            (registry.peak_names(), registry.peak_names_per_owner()),
            (3, 2)
        );
    }

    /// The peak of the names of one owner follows the names that the owner has at a time: a name
    /// that it lost, by letting go of it, by its being taken over or by going, no longer counts
    /// once it gains another.
    #[test]
    fn peak_of_an_owner_counts_the_names_that_it_has() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            let replace = RequestNameFlags::REPLACE_EXISTING;
            let owner = UniqueName::try_from(":busd.1").unwrap();

            request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
            registry
                .release_name("org.busd.One".try_into().unwrap(), owner.clone())
                .await;
            request(&mut registry, "org.busd.Two", ":busd.1", replaceable).await;
            assert_eq!(registry.peak_names_per_owner(), 1);

            request(&mut registry, "org.busd.Two", ":busd.2", replace).await;
            request(&mut registry, "org.busd.Three", ":busd.1", replaceable).await;
            assert_eq!(registry.peak_names_per_owner(), 1);

            registry.release_all(owner).await;
            request(&mut registry, "org.busd.Four", ":busd.1", replaceable).await;
            assert_eq!(registry.peak_names_per_owner(), 1);

            request(&mut registry, "org.busd.Five", ":busd.1", replaceable).await;
            assert_eq!(registry.peak_names_per_owner(), 2);
        });
    }

    /// A peer that queued for a name and then took it over is in the queue no more: the name that
    /// it lets go of doesn't come back to it. The peer that it took the name from asked not to be
    /// queued, so no one is left to get the name.
    #[test]
    fn taking_a_name_over_leaves_its_queue() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            let first = replaceable | RequestNameFlags::DO_NOT_QUEUE;
            request(&mut registry, "org.busd.One", ":busd.1", first).await;
            let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", replaceable).await;
            assert_eq!(reply, RequestNameReply::InQueue);
            let replace = RequestNameFlags::REPLACE_EXISTING;
            let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", replace).await;
            assert_eq!(reply, RequestNameReply::PrimaryOwner);
            assert_eq!(queue(&registry, "org.busd.One"), [""; 0]);

            let owner = UniqueName::try_from(":busd.2").unwrap();
            let (_, changed) = registry
                .release_name("org.busd.One".try_into().unwrap(), owner)
                .await;
            assert_eq!(changed.unwrap().new_owner, None);
            assert_eq!(registry.lookup(&"org.busd.One".try_into().unwrap()), None);
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (1, 1)
            );
            let owned = registry.owned_by(":busd.2");
            assert_eq!((owned.now, owned.peak), (0, 1));
        });
    }

    /// A peer that asks for a taken name more than once is in its queue once, with the flags of
    /// its last request, so that a peer that is gone never gets the name.
    #[test]
    fn queueing_twice_is_queueing_once() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let flags = RequestNameFlags::empty();
            request(&mut registry, "org.busd.One", ":busd.1", flags).await;
            for flags in [flags, RequestNameFlags::ALLOW_REPLACEMENT] {
                let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", flags).await;
                assert_eq!(reply, RequestNameReply::InQueue);
            }
            assert_eq!(queue(&registry, "org.busd.One"), [":busd.2"]);
            let name = WellKnownName::try_from("org.busd.One").unwrap();
            let queued = registry.waiting_list(name.clone()).unwrap().next().unwrap();
            assert!(queued.allow_replacement());

            // The peer goes, and then the owner lets go of the name: no one has it.
            let gone = UniqueName::try_from(":busd.2").unwrap();
            assert!(registry.release_all(gone).await.is_empty());
            let owner = UniqueName::try_from(":busd.1").unwrap();
            let (_, changed) = registry.release_name(name.clone(), owner).await;
            assert_eq!(changed.unwrap().new_owner, None);
            assert_eq!(registry.lookup(&name), None);
        });
    }

    /// The registry tells how many names an owner has and the most that it had. Releasing a name
    /// keeps the peak, and `release_all`, as for a peer that disconnects, resets both.
    #[test]
    fn names_of_one_owner() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let flags = RequestNameFlags::empty();
            let owner = UniqueName::try_from(":busd.1").unwrap();
            let owned = registry.owned_by(":busd.1");
            assert_eq!((owned.now, owned.peak), (0, 0));

            request(&mut registry, "org.busd.One", ":busd.1", flags).await;
            request(&mut registry, "org.busd.Two", ":busd.1", flags).await;
            request(&mut registry, "org.busd.Three", ":busd.2", flags).await;
            // Queueing for a name is not owning it.
            request(&mut registry, "org.busd.Three", ":busd.1", flags).await;
            let owned = registry.owned_by(":busd.1");
            assert_eq!((owned.now, owned.peak), (2, 2));

            registry
                .release_name("org.busd.One".try_into().unwrap(), owner.clone())
                .await;
            let owned = registry.owned_by(":busd.1");
            assert_eq!((owned.now, owned.peak), (1, 2));

            registry.release_all(owner).await;
            let owned = registry.owned_by(":busd.1");
            assert_eq!((owned.now, owned.peak), (0, 0));
            let owned = registry.owned_by(":busd.2");
            assert_eq!((owned.now, owned.peak), (1, 1));
        });
    }

    /// A peer is known by the names that it owns and those that it queues for, each once.
    #[test]
    fn names_of_owners_and_queued_peers() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            assert!(registry.names_of(":busd.1").is_empty());

            let flags = RequestNameFlags::empty();
            request(&mut registry, "org.busd.One", ":busd.1", flags).await;
            request(&mut registry, "org.busd.Two", ":busd.2", flags).await;
            request(&mut registry, "org.busd.Two", ":busd.1", flags).await;
            request(&mut registry, "org.busd.Two", ":busd.1", flags).await;
            assert_eq!(
                names_of(&registry, ":busd.1"),
                ["org.busd.One", "org.busd.Two"]
            );
            assert_eq!(names_of(&registry, ":busd.2"), ["org.busd.Two"]);
        });
    }

    /// A name that another peer takes over is a name of that peer, and no more one of its owner.
    #[test]
    fn names_of_peers_after_a_name_is_taken_over() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
            request(
                &mut registry,
                "org.busd.One",
                ":busd.2",
                RequestNameFlags::REPLACE_EXISTING,
            )
            .await;

            assert!(registry.names_of(":busd.1").is_empty());
            assert_eq!(names_of(&registry, ":busd.2"), ["org.busd.One"]);
        });
    }

    /// A name that its owner releases goes from the names of the owner, and stays one of the peer
    /// that it goes to from the queue; a peer that leaves the queue loses it too.
    #[test]
    fn names_of_peers_after_a_name_is_released() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let flags = RequestNameFlags::empty();
            request(&mut registry, "org.busd.One", ":busd.1", flags).await;
            request(&mut registry, "org.busd.Two", ":busd.1", flags).await;
            request(&mut registry, "org.busd.One", ":busd.2", flags).await;
            request(&mut registry, "org.busd.One", ":busd.3", flags).await;

            release(&mut registry, "org.busd.One", ":busd.1").await;
            assert_eq!(names_of(&registry, ":busd.1"), ["org.busd.Two"]);
            assert_eq!(names_of(&registry, ":busd.2"), ["org.busd.One"]);
            release(&mut registry, "org.busd.One", ":busd.3").await;
            assert!(registry.names_of(":busd.3").is_empty());

            release(&mut registry, "org.busd.Two", ":busd.1").await;
            assert!(registry.names_of(":busd.1").is_empty());
            let changes = registry
                .release_all(UniqueName::try_from(":busd.2").unwrap())
                .await;
            assert_eq!(changes.len(), 1);
            assert!(registry.names_of(":busd.2").is_empty());
        });
    }

    /// A peer that queued for a name and then took it over has the name once, and no more once
    /// it lets go of it.
    #[test]
    fn names_of_a_peer_that_took_over_a_name_it_queued_for() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            let replace = RequestNameFlags::REPLACE_EXISTING;
            let first = replaceable | RequestNameFlags::DO_NOT_QUEUE;
            request(&mut registry, "org.busd.One", ":busd.1", first).await;
            request(&mut registry, "org.busd.One", ":busd.2", replaceable).await;
            request(&mut registry, "org.busd.One", ":busd.2", replace).await;
            assert_eq!(names_of(&registry, ":busd.2"), ["org.busd.One"]);

            release(&mut registry, "org.busd.One", ":busd.2").await;
            assert!(registry.names_of(":busd.2").is_empty());
        });
    }

    /// The names that another instance of the bus hands over are names of their owners and of the
    /// peers that queue for them.
    #[test]
    fn names_of_peers_after_names_are_inserted() {
        let mut registry = NameRegistry::default();
        let owner = |name: &str| NameOwner::new(name.try_into().unwrap(), false);
        registry.insert(
            "org.busd.One".try_into().unwrap(),
            owner(":busd.1"),
            [owner(":busd.2"), owner(":busd.3")],
        );
        registry.insert("org.busd.Two".try_into().unwrap(), owner(":busd.2"), []);

        assert_eq!(names_of(&registry, ":busd.1"), ["org.busd.One"]);
        assert_eq!(
            names_of(&registry, ":busd.2"),
            ["org.busd.One", "org.busd.Two"]
        );
        assert_eq!(names_of(&registry, ":busd.3"), ["org.busd.One"]);
    }

    /// The bus has its own name, which nothing that the registry is asked to do takes from it.
    #[test]
    fn names_of_the_bus() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            assert_eq!(names_of(&registry, fdo::BUS_NAME), [fdo::BUS_NAME]);

            let flags = RequestNameFlags::empty();
            request(&mut registry, "org.busd.One", fdo::BUS_NAME, flags).await;
            assert_eq!(names_of(&registry, fdo::BUS_NAME), [fdo::BUS_NAME]);
            release(&mut registry, "org.busd.One", fdo::BUS_NAME).await;
            registry.insert(
                "org.busd.Two".try_into().unwrap(),
                NameOwner::new(fdo::BUS_NAME.try_into().unwrap(), false),
                [],
            );
            release(&mut registry, "org.busd.Two", fdo::BUS_NAME).await;
            assert_eq!(names_of(&registry, fdo::BUS_NAME), [fdo::BUS_NAME]);
        });
    }

    /// The names of `peer` in `registry`, sorted.
    fn names_of<'r>(registry: &'r NameRegistry, peer: &str) -> Vec<&'r str> {
        let mut names: Vec<_> = registry
            .names_of(peer)
            .iter()
            .map(|name| name.as_str())
            .collect();
        names.sort_unstable();

        names
    }

    /// Release `name` for `owner`.
    async fn release(registry: &mut NameRegistry, name: &str, owner: &str) -> ReleaseNameReply {
        let (reply, _) = registry
            .release_name(
                name.try_into().unwrap(),
                UniqueName::try_from(owner).unwrap(),
            )
            .await;

        reply
    }

    /// The unique names of the peers that queue for `name` in `registry`, in order.
    fn queue<'r>(registry: &'r NameRegistry, name: &str) -> Vec<&'r str> {
        registry
            .waiting_list(name.try_into().unwrap())
            .unwrap()
            .map(|waiting| waiting.unique_name().as_str())
            .collect()
    }

    /// Request `name` for `owner`.
    async fn request(
        registry: &mut NameRegistry,
        name: &str,
        owner: &str,
        flags: RequestNameFlags,
    ) -> (RequestNameReply, Option<NameOwnerChanged>) {
        registry
            .request_name(
                name.try_into().unwrap(),
                UniqueName::try_from(owner).unwrap(),
                flags,
            )
            .await
    }
}
