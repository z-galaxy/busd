use std::collections::{HashMap, VecDeque};
use zbus::{
    fdo::{ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::{
        BusName, OwnedBusName, OwnedUniqueName, OwnedWellKnownName, UniqueName, WellKnownName,
    },
};

#[derive(Debug, Default)]
pub struct NameRegistry {
    names: HashMap<OwnedWellKnownName, NameEntry>,
    /// The current and peak name counts of each owner, kept up to date as names change owners, so
    /// that neither takes a pass over the names. An entry stays after its owner released its last
    /// name, to preserve the peak: `release_all` removes it, when the peer disconnects or becomes
    /// a monitor.
    owned: HashMap<OwnedUniqueName, Owned>,
    /// The most names that had an owner at a time.
    peak_names: usize,
    /// The most names that one owner had at a time.
    peak_names_per_owner: usize,
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
                    self.note_lost(&old_owner);
                    self.note_gained(&unique_name);

                    (
                        RequestNameReply::PrimaryOwner,
                        Some(NameOwnerChanged {
                            name: BusName::from(name).into(),
                            old_owner: Some(old_owner),
                            new_owner: Some(unique_name),
                        }),
                    )
                } else if !flags.contains(RequestNameFlags::DO_NOT_QUEUE) {
                    let owner = NameOwner {
                        unique_name: OwnedUniqueName::from(unique_name.clone()),
                        allow_replacement: flags.contains(RequestNameFlags::ALLOW_REPLACEMENT),
                    };
                    entry.waiting_list.push_back(owner);

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
        self.names.insert(name, entry);
        self.note_gained(&owner_name);
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

    /// A name that passes on to the one that lets go of it, which queued for it before it took it
    /// over, is one name of that owner, and never two.
    #[test]
    fn peak_of_an_owner_that_a_name_comes_back_to() {
        zruntime::block_on(async {
            let mut registry = NameRegistry::default();
            let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
            request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
            let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", replaceable).await;
            assert_eq!(reply, RequestNameReply::InQueue);
            let replace = RequestNameFlags::REPLACE_EXISTING;
            let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", replace).await;
            assert_eq!(reply, RequestNameReply::PrimaryOwner);

            let owner = UniqueName::try_from(":busd.2").unwrap();
            let (_, changed) = registry
                .release_name("org.busd.One".try_into().unwrap(), owner.clone())
                .await;
            let changed = changed.unwrap();
            assert_eq!(changed.new_owner.as_deref(), Some(&owner));
            assert_eq!(
                (registry.peak_names(), registry.peak_names_per_owner()),
                (1, 1)
            );
            let owned = registry.owned_by(":busd.2");
            assert_eq!((owned.now, owned.peak), (1, 1));
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
