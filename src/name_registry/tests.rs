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
    let owner = |name: &str| NameOwner::new(name.try_into().unwrap(), false, false);
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

/// The owner that a name is taken over from is first in line to get it back, ahead of those
/// that queued before, unless it asked not to queue.
#[test]
fn a_replaced_owner_queues_for_its_name() {
    zruntime::block_on(async {
        let mut registry = NameRegistry::default();
        let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
        let replace = RequestNameFlags::REPLACE_EXISTING;
        request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
        request(&mut registry, "org.busd.One", ":busd.3", replaceable).await;
        let (reply, changed) = request(&mut registry, "org.busd.One", ":busd.2", replace).await;
        assert_eq!(reply, RequestNameReply::PrimaryOwner);
        let changed = changed.unwrap();
        assert_eq!(changed.old_owner.unwrap(), ":busd.1");
        assert_eq!(queue(&registry, "org.busd.One"), [":busd.1", ":busd.3"]);

        let name = WellKnownName::try_from("org.busd.One").unwrap();
        let owner = UniqueName::try_from(":busd.2").unwrap();
        let (_, changed) = registry.release_name(name.clone(), owner).await;
        assert_eq!(changed.unwrap().new_owner.unwrap(), ":busd.1");
        // It queued as it asked for the name, so it lets another take the name over still.
        let (reply, _) = request(&mut registry, "org.busd.One", ":busd.2", replace).await;
        assert_eq!(reply, RequestNameReply::PrimaryOwner);

        let unqueued = replaceable | RequestNameFlags::DO_NOT_QUEUE;
        request(&mut registry, "org.busd.Two", ":busd.1", unqueued).await;
        request(&mut registry, "org.busd.Two", ":busd.2", replace).await;
        assert_eq!(queue(&registry, "org.busd.Two"), [""; 0]);
    });
}

/// The flags of an owner are those of its last request for the name: one that asks again
/// changes whether it queues once another takes the name over, and whether another may.
#[test]
fn an_owner_that_asks_again_changes_its_flags() {
    zruntime::block_on(async {
        let mut registry = NameRegistry::default();
        let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
        let replace = RequestNameFlags::REPLACE_EXISTING;
        let unqueued = replaceable | RequestNameFlags::DO_NOT_QUEUE;
        request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
        let (reply, _) = request(&mut registry, "org.busd.One", ":busd.1", unqueued).await;
        assert_eq!(reply, RequestNameReply::AlreadyOwner);
        request(&mut registry, "org.busd.One", ":busd.2", replace).await;
        assert_eq!(queue(&registry, "org.busd.One"), [""; 0]);

        // The new owner asked without `ALLOW_REPLACEMENT`, and then asks again with it.
        let (reply, _) = request(&mut registry, "org.busd.One", ":busd.1", replace).await;
        assert_eq!(reply, RequestNameReply::InQueue);
        request(&mut registry, "org.busd.One", ":busd.2", replaceable).await;
        let (reply, _) = request(&mut registry, "org.busd.One", ":busd.1", replace).await;
        assert_eq!(reply, RequestNameReply::PrimaryOwner);
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

/// A name that another peer takes over is a name of that peer. It stays one of the peer that
/// had it, which queues for it, unless that peer asked not to queue.
#[test]
fn names_of_peers_after_a_name_is_taken_over() {
    zruntime::block_on(async {
        let mut registry = NameRegistry::default();
        let replaceable = RequestNameFlags::ALLOW_REPLACEMENT;
        let replace = RequestNameFlags::REPLACE_EXISTING;
        request(&mut registry, "org.busd.One", ":busd.1", replaceable).await;
        request(&mut registry, "org.busd.One", ":busd.2", replace).await;
        assert_eq!(names_of(&registry, ":busd.1"), ["org.busd.One"]);
        assert_eq!(names_of(&registry, ":busd.2"), ["org.busd.One"]);

        let unqueued = replaceable | RequestNameFlags::DO_NOT_QUEUE;
        request(&mut registry, "org.busd.Two", ":busd.1", unqueued).await;
        request(&mut registry, "org.busd.Two", ":busd.2", replace).await;
        assert_eq!(names_of(&registry, ":busd.1"), ["org.busd.One"]);
        assert_eq!(
            names_of(&registry, ":busd.2"),
            ["org.busd.One", "org.busd.Two"]
        );
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
    let owner = |name: &str| NameOwner::new(name.try_into().unwrap(), false, false);
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
            NameOwner::new(fdo::BUS_NAME.try_into().unwrap(), false, false),
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
