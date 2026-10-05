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
    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }
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
                        Some(owner) => {
                            entry.owner = owner;
                            Some(entry.owner.unique_name.clone())
                        }
                        None => {
                            self.names.remove(name.as_str());

                            None
                        }
                    };

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

        all_changed
    }

    pub fn lookup(&self, name: WellKnownName) -> Option<OwnedUniqueName> {
        self.names
            .get(name.as_str())
            .map(|e| e.owner.unique_name.clone())
    }

    pub fn all_names(&self) -> &HashMap<OwnedWellKnownName, NameEntry> {
        &self.names
    }

    pub fn waiting_list(
        &self,
        name: WellKnownName<'_>,
    ) -> Option<impl Iterator<Item = &NameOwner>> {
        self.names.get(name.as_str()).map(|e| e.waiting_list.iter())
    }
}

#[derive(Debug)]
pub struct NameOwnerChanged {
    pub name: OwnedBusName,
    pub old_owner: Option<OwnedUniqueName>,
    pub new_owner: Option<OwnedUniqueName>,
}
