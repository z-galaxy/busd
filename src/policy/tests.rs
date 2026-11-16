use zbus::names::OwnedWellKnownName;

use super::*;

/// Without any policy, everything is allowed, but only the clients of the bus's user may
/// connect: for a configuration that says nothing of its bus, and for that of a session bus.
#[test]
fn no_policy_is_open() {
    for config in ["", "<type>session</type>"] {
        let policy = Policy::new(&parse(config), &ids(), BUS_UID);

        assert!(policy.may_connect(BUS_UID, &[]));
        assert!(!policy.may_connect(ALICE, &[ALICE]));
        let peer = policy.for_peer(BUS_UID, &[]);
        assert!(peer.sends_freely());
        assert!(peer.receives_freely());
        assert!(peer.may_own("org.example.Service"));
        assert!(peer.may_send(&call(), &[]));
        assert!(peer.may_receive(&signal(), &names(&["org.example.Service"])));
    }
}

/// A system bus without any policy has no rule that allows anything: its own user may still
/// connect, and nothing else is allowed, to that user either.
#[test]
fn no_policy_of_a_system_bus_denies_everything() {
    let policy = Policy::new(&parse("<type>system</type>"), &ids(), BUS_UID);

    assert!(policy.may_connect(BUS_UID, &[]));
    assert!(!policy.may_connect(ALICE, &[ALICE]));
    for uid in [BUS_UID, ROOT, ALICE] {
        let peer = policy.for_peer(uid, &[]);
        assert!(!peer.sends_freely());
        assert!(!peer.receives_freely());
        assert!(!peer.may_own("org.example.Service"));
        assert!(!peer.may_send(&call(), &names(&["org.freedesktop.DBus"])));
        assert!(!peer.may_receive(&signal(), &[]));
    }
}

/// The policy of the bus itself lets it own, send and receive everything.
#[test]
fn the_bus_is_unrestricted() {
    let peer = PeerPolicy::unrestricted();

    assert!(peer.sends_freely());
    assert!(peer.receives_freely());
    assert!(peer.may_own("org.freedesktop.DBus"));
    assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
    assert!(peer.may_receive(&signal(), &[]));
}

/// The default policy of a peer denies everything.
#[test]
fn the_default_peer_policy_denies_everything() {
    let peer = PeerPolicy::default();

    assert!(!peer.sends_freely());
    assert!(!peer.receives_freely());
    assert!(!peer.may_own("org.example.Service"));
    assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));
    assert!(!peer.may_receive(&signal(), &[]));
}

/// Where no rule matches, the answer is deny.
#[test]
fn what_no_rule_matches_is_denied() {
    let policy = policy(
        r#"<policy context="default">
                <allow own="org.example.Other"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[ALICE]);
    assert!(!peer.sends_freely());
    assert!(!peer.receives_freely());
    assert!(!peer.may_own("org.example.Service"));
    assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));
    assert!(!peer.may_receive(&call(), &names(&["org.example.Service"])));
    assert!(!policy.may_connect(ALICE, &[ALICE]));
}

/// Within a context, a rule outranks the rules before it in the configuration.
#[test]
fn later_rules_outrank_earlier_ones() {
    let allow_then_deny = policy(
        r#"<policy context="default">
                <allow own="org.example.Service"/>
                <deny own="org.example.Service"/>
            </policy>"#,
    );
    assert!(!allow_then_deny
        .for_peer(ALICE, &[])
        .may_own("org.example.Service"));

    let deny_then_allow = policy(
        r#"<policy context="default">
                <deny own="org.example.Service"/>
                <allow own="org.example.Service"/>
            </policy>"#,
    );
    assert!(deny_then_allow
        .for_peer(ALICE, &[])
        .may_own("org.example.Service"));
}

/// The rules of separate policies of one context rank by where they stand in the
/// configuration, whatever stands between them.
#[test]
fn rules_of_separate_policies_rank_by_position() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_destination="org.example.Service"/>
                <deny own="*"/>
            </policy>
            <policy user="alice">
                <allow own="org.example.Service"/>
            </policy>
            <policy context="default">
                <deny send_destination="org.example.Service" send_member="Forbidden"/>
                <allow own="*"/>
            </policy>"#,
    );

    let peer = policy.for_peer(BOB, &[]);
    let service = names(&["org.example.Service"]);
    assert!(peer.may_send(&call(), &service));
    let forbidden = Facts {
        member: Some("Forbidden"),
        ..call()
    };
    assert!(!peer.may_send(&forbidden, &service));
    assert!(peer.may_own("org.example.Other"));
}

/// A rule of a context outranks every rule of the contexts below it, wherever it stands in
/// the configuration: default < group < user < mandatory.
#[test]
fn contexts_rank_by_kind() {
    let policy = policy(
        r#"<policy context="mandatory">
                <deny own="org.example.Mandatory"/>
            </policy>
            <policy user="alice">
                <allow own="org.example.Mandatory"/>
                <deny own="org.example.User"/>
            </policy>
            <policy group="wheel">
                <allow own="org.example.User"/>
                <deny own="org.example.Group"/>
            </policy>
            <policy context="default">
                <allow own="*"/>
            </policy>"#,
    );

    let alice = policy.for_peer(ALICE, &[ALICE, WHEEL]);
    assert!(!alice.may_own("org.example.Mandatory"));
    assert!(!alice.may_own("org.example.User"));
    assert!(!alice.may_own("org.example.Group"));
    assert!(alice.may_own("org.example.Default"));

    // Bob is in wheel too, but has no user policy, so the group's rules give his answers.
    let bob = policy.for_peer(BOB, &[BOB, WHEEL]);
    assert!(!bob.may_own("org.example.Mandatory"));
    assert!(bob.may_own("org.example.User"));
    assert!(!bob.may_own("org.example.Group"));
}

/// The rules of a user policy apply to the peers of that user alone, whether the policy names
/// the user or its uid, and a policy of a user that does not resolve is left out.
#[test]
fn user_policies_apply_to_their_users() {
    let policy = policy(
        r#"<policy user="alice">
                <allow own="org.example.Alice"/>
            </policy>
            <policy user="1001">
                <allow own="org.example.Bob"/>
            </policy>
            <policy user="mallory">
                <allow own="*"/>
            </policy>"#,
    );

    let alice = policy.for_peer(ALICE, &[]);
    assert!(alice.may_own("org.example.Alice"));
    assert!(!alice.may_own("org.example.Bob"));
    let bob = policy.for_peer(BOB, &[]);
    assert!(!bob.may_own("org.example.Alice"));
    assert!(bob.may_own("org.example.Bob"));
    let other = policy.for_peer(4000, &[]);
    assert!(!other.may_own("org.example.Alice"));
    assert!(!other.may_own("org.example.Other"));
}

/// The rules of a group policy apply to the peers that have the group among their gids,
/// whether the policy names the group or its gid, and a policy of a group that does not
/// resolve is left out.
#[test]
fn group_policies_apply_to_their_members() {
    let policy = policy(
        r#"<policy group="wheel">
                <allow own="org.example.Wheel"/>
            </policy>
            <policy group="20">
                <allow own="org.example.Twenty"/>
            </policy>
            <policy group="nogroup">
                <allow own="*"/>
            </policy>"#,
    );

    let member = policy.for_peer(BOB, &[BOB, WHEEL, WHEEL]);
    assert!(member.may_own("org.example.Wheel"));
    assert!(!member.may_own("org.example.Twenty"));
    assert!(!member.may_own("org.example.Other"));
    let other = policy.for_peer(BOB, &[BOB, 20]);
    assert!(!other.may_own("org.example.Wheel"));
    assert!(other.may_own("org.example.Twenty"));
    // Being the user of the group's name is not being in the group.
    let named = policy.for_peer(ALICE, &[ALICE]);
    assert!(!named.may_own("org.example.Wheel"));
}

/// The rules of a policy of the `at_console` context are left out, whichever value it has, but
/// the configuration has a policy all the same, so it is not the open one.
#[test]
fn console_policies_are_left_out() {
    let policy = policy(
        r#"<policy at_console="true">
                <allow own="org.example.Console"/>
            </policy>
            <policy at_console="false">
                <allow own="org.example.NotConsole"/>
            </policy>"#,
    );

    for uid in [ALICE, 999, ROOT] {
        let peer = policy.for_peer(uid, &[]);
        assert!(!peer.may_own("org.example.Console"), "uid {uid}");
        assert!(!peer.may_own("org.example.NotConsole"), "uid {uid}");
        assert!(!peer.sends_freely(), "uid {uid}");
    }
}

/// A client of the bus's user may connect where no rule matches it, and no other may.
#[test]
fn the_bus_user_may_connect_where_no_rule_matches() {
    let policy = policy(
        r#"<policy context="default">
                <allow own="*"/>
            </policy>"#,
    );

    assert!(policy.may_connect(BUS_UID, &[]));
    assert!(!policy.may_connect(ALICE, &[ALICE]));
}

/// Every connect rule that matches a client of the bus's user outranks the implicit allow.
#[test]
fn connect_rules_outrank_the_bus_user() {
    let policy = policy(
        r#"<policy context="default">
                <deny user="*"/>
            </policy>"#,
    );

    assert!(!policy.may_connect(BUS_UID, &[]));
}

/// A connect rule matches the clients of the user or in the group that it names, by name or
/// by number, and `*` matches all.
#[test]
fn connect_rules_match_users_and_groups() {
    let policy = policy(
        r#"<policy context="default">
                <allow user="alice"/>
                <allow user="2000"/>
                <allow group="wheel"/>
                <allow group="mallory"/>
            </policy>"#,
    );

    assert!(policy.may_connect(ALICE, &[ALICE]));
    assert!(policy.may_connect(2000, &[]));
    assert!(policy.may_connect(BOB, &[BOB, WHEEL]));
    assert!(!policy.may_connect(BOB, &[BOB]));

    let everyone = self::policy(
        r#"<policy context="default">
                <allow group="*"/>
            </policy>"#,
    );
    assert!(everyone.may_connect(BOB, &[]));
}

/// A mandatory connect rule outranks a default one, wherever they stand.
#[test]
fn mandatory_connect_rules_outrank_default_ones() {
    let policy = policy(
        r#"<policy context="mandatory">
                <deny user="bob"/>
            </policy>
            <policy context="default">
                <allow user="*"/>
            </policy>"#,
    );

    assert!(policy.may_connect(ALICE, &[]));
    assert!(!policy.may_connect(BOB, &[]));
}

/// Connect rules count in the default and mandatory contexts alone.
#[test]
fn connect_rules_of_other_contexts_are_left_out() {
    let policy = policy(
        r#"<policy user="alice">
                <allow user="*"/>
            </policy>
            <policy group="wheel">
                <allow user="*"/>
            </policy>
            <policy at_console="true">
                <allow user="*"/>
            </policy>"#,
    );

    assert!(!policy.may_connect(ALICE, &[ALICE, WHEEL]));
}

/// `own` matches its name alone, `own="*"` every name, and `own_prefix` its name and the
/// names under it, on a boundary between elements.
#[test]
fn own_rules_match_names() {
    let exact = policy(r#"<policy context="default"><allow own="a.b"/></policy>"#);
    let peer = exact.for_peer(ALICE, &[]);
    assert!(peer.may_own("a.b"));
    assert!(!peer.may_own("a.b.c"));
    assert!(!peer.may_own("a.bc"));
    assert!(!peer.may_own("a"));

    let any = policy(r#"<policy context="default"><allow own="*"/></policy>"#);
    assert!(any.for_peer(ALICE, &[]).may_own("org.example.Service"));

    let prefix = policy(r#"<policy context="default"><allow own_prefix="a.b"/></policy>"#);
    let peer = prefix.for_peer(ALICE, &[]);
    assert!(peer.may_own("a.b"));
    assert!(peer.may_own("a.b.c"));
    assert!(peer.may_own("a.b.c.d"));
    assert!(!peer.may_own("a.bc"));
    assert!(!peer.may_own("a"));
}

/// `send_destination` matches a name of the receiver exactly, `send_destination_prefix` that
/// name and those under it, and `send_destination="*"` any receiver.
#[test]
fn send_rules_match_destinations() {
    let exact = policy(
        r#"<policy context="default">
                <allow send_destination="a.b"/>
            </policy>"#,
    );
    let peer = exact.for_peer(ALICE, &[]);
    assert!(!peer.sends_freely());
    assert!(peer.may_send(&call(), &names(&["a.b"])));
    assert!(!peer.may_send(&call(), &names(&["a.b.c"])));
    assert!(!peer.may_send(&call(), &[]));

    let prefix = policy(
        r#"<policy context="default">
                <allow send_destination_prefix="a.b"/>
            </policy>"#,
    );
    let peer = prefix.for_peer(ALICE, &[]);
    assert!(peer.may_send(&call(), &names(&["a.b"])));
    assert!(peer.may_send(&call(), &names(&["a.b.c"])));
    assert!(!peer.may_send(&call(), &names(&["a.bc"])));
    assert!(!peer.may_send(&call(), &[]));

    let any = policy(
        r#"<policy context="default">
                <allow send_destination="*"/>
            </policy>"#,
    );
    let peer = any.for_peer(ALICE, &[]);
    assert!(peer.sends_freely());
    assert!(peer.may_send(&call(), &[]));
}

/// A rule matches where one of the names of the other party does, a name it queues for
/// counting as one it owns.
#[test]
fn any_name_of_the_other_party_matches() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_destination="org.example.Queued"/>
                <allow receive_sender="org.example.Queued"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    let names = names(&["org.example.Owned", "org.example.Queued"]);
    assert!(peer.may_send(&call(), &names));
    assert!(peer.may_receive(&call(), &names));
}

/// A unique name is no well-known name, so a rule that names one never matches.
#[test]
fn unique_names_never_match() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_destination=":1.42"/>
                <allow receive_sender=":1.42"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(!peer.may_send(&call(), &[]));
    assert!(!peer.may_receive(&call(), &[]));
}

/// `send_path`, `send_interface` and `send_member` match the message's own, and a message
/// without an interface does not match a rule that names one.
#[test]
fn send_rules_match_paths_interfaces_and_members() {
    let path = policy(r#"<policy context="default"><allow send_path="/org/example"/></policy>"#);
    let peer = path.for_peer(ALICE, &[]);
    assert!(peer.may_send(&call(), &[]));
    assert!(!peer.may_send(
        &Facts {
            path: Some("/org"),
            ..call()
        },
        &[]
    ));

    let interface =
        policy(r#"<policy context="default"><allow send_interface="org.example.Iface"/></policy>"#);
    let peer = interface.for_peer(ALICE, &[]);
    assert!(peer.may_send(&call(), &[]));
    let other = Facts {
        interface: Some("org.example.Other"),
        ..call()
    };
    assert!(!peer.may_send(&other, &[]));
    let without = Facts {
        interface: None,
        ..call()
    };
    assert!(!peer.may_send(&without, &[]));

    let member = policy(r#"<policy context="default"><allow send_member="Do"/></policy>"#);
    let peer = member.for_peer(ALICE, &[]);
    assert!(peer.may_send(&call(), &[]));
    assert!(!peer.may_send(
        &Facts {
            member: Some("Undo"),
            ..call()
        },
        &[]
    ));
}

/// An attribute whose value is `*` always holds.
#[test]
fn wildcard_attributes_always_hold() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_path="*" send_interface="*" send_member="*" send_type="*"/>
                <allow receive_sender="*" receive_interface="*"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(peer.sends_freely());
    assert!(peer.receives_freely());
    let without = Facts {
        interface: None,
        ..signal()
    };
    assert!(peer.may_send(&without, &[]));
    assert!(peer.may_receive(&without, &[]));
}

/// `send_type` matches the type of the message.
#[test]
fn send_rules_match_types() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_type="signal"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(peer.may_send(&signal(), &[]));
    assert!(!peer.may_send(&call(), &[]));
}

/// `send_broadcast="true"` matches a signal without a destination, and
/// `send_broadcast="false"` any other message.
#[test]
fn send_rules_match_broadcasts() {
    let broadcast = Facts {
        broadcast: true,
        ..signal()
    };

    let only = policy(r#"<policy context="default"><allow send_broadcast="true"/></policy>"#);
    let peer = only.for_peer(ALICE, &[]);
    assert!(peer.may_send(&broadcast, &[]));
    assert!(!peer.may_send(&signal(), &names(&["org.example.Service"])));
    assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));

    let not = policy(r#"<policy context="default"><allow send_broadcast="false"/></policy>"#);
    let peer = not.for_peer(ALICE, &[]);
    assert!(!peer.may_send(&broadcast, &[]));
    assert!(peer.may_send(&signal(), &names(&["org.example.Service"])));
    assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
}

/// `min_fds` and `max_fds` bound the number of file descriptors of the message, inclusive.
#[test]
fn send_rules_match_numbers_of_fds() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_type="method_call" min_fds="1" max_fds="2"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    for (fds, allowed) in [(0, false), (1, true), (2, true), (3, false)] {
        assert_eq!(
            peer.may_send(&Facts { fds, ..call() }, &[]),
            allowed,
            "{fds} fds"
        );
    }
}

/// Receive rules match the names of the sender and the attributes of the message as send
/// rules do those of the receiver.
#[test]
fn receive_rules_match_senders_and_messages() {
    let policy = policy(
        r#"<policy context="default">
                <allow receive_sender="org.example.Service" receive_type="method_call"
                       receive_path="/org/example" receive_interface="org.example.Iface"
                       receive_member="Do" max_fds="1"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(!peer.receives_freely());
    let service = names(&["org.example.Service"]);
    assert!(peer.may_receive(&call(), &service));
    assert!(!peer.may_receive(&call(), &names(&["org.example.Other"])));
    assert!(!peer.may_receive(&call(), &[]));
    assert!(!peer.may_receive(&signal(), &service));
    assert!(!peer.may_receive(
        &Facts {
            path: Some("/"),
            ..call()
        },
        &service
    ));
    let without = Facts {
        interface: None,
        ..call()
    };
    assert!(!peer.may_receive(&without, &service));
    assert!(!peer.may_receive(
        &Facts {
            member: Some("Undo"),
            ..call()
        },
        &service
    ));
    assert!(!peer.may_receive(&Facts { fds: 2, ..call() }, &service));
    // The send rules are not the receive rules.
    assert!(!peer.may_send(&call(), &service));
}

/// Within each list of send rules, those that name no one, those that name a name and those
/// that name a prefix, a rule outranks the rules before it.
#[test]
fn later_send_rules_outrank_earlier_ones() {
    let service = names(&["org.example.Service"]);
    for (first, second) in [
        (r#"send_type="method_call""#, r#"send_member="Do""#),
        (
            r#"send_destination="org.example.Service""#,
            r#"send_destination="org.example.Service" send_member="Do""#,
        ),
        (
            r#"send_destination_prefix="org.example""#,
            r#"send_destination_prefix="org.example.Service""#,
        ),
    ] {
        for (allowed, denied) in [(first, second), (second, first)] {
            let peer = policy(&format!(
                r#"<policy context="default">
                        <deny {denied}/>
                        <allow {allowed}/>
                    </policy>"#
            ))
            .for_peer(ALICE, &[]);
            assert!(peer.may_send(&call(), &service), "{denied} then {allowed}");

            let peer = policy(&format!(
                r#"<policy context="default">
                        <allow {allowed}/>
                        <deny {denied}/>
                    </policy>"#
            ))
            .for_peer(ALICE, &[]);
            assert!(!peer.may_send(&call(), &service), "{allowed} then {denied}");
        }
    }
}

/// Within each list of receive rules, those that name no one and those that name a name, a
/// rule outranks the rules before it.
#[test]
fn later_receive_rules_outrank_earlier_ones() {
    let service = names(&["org.example.Service"]);
    for (first, second) in [
        (r#"receive_type="method_call""#, r#"receive_member="Do""#),
        (
            r#"receive_sender="org.example.Service""#,
            r#"receive_sender="org.example.Service" receive_member="Do""#,
        ),
    ] {
        for (allowed, denied) in [(first, second), (second, first)] {
            let peer = policy(&format!(
                r#"<policy context="default">
                        <deny {denied}/>
                        <allow {allowed}/>
                    </policy>"#
            ))
            .for_peer(ALICE, &[]);
            assert!(
                peer.may_receive(&call(), &service),
                "{denied} then {allowed}"
            );

            let peer = policy(&format!(
                r#"<policy context="default">
                        <allow {allowed}/>
                        <deny {denied}/>
                    </policy>"#
            ))
            .for_peer(ALICE, &[]);
            assert!(
                !peer.may_receive(&call(), &service),
                "{allowed} then {denied}"
            );
        }
    }
}

/// A mandatory send or receive rule outranks a default one that the configuration has after
/// it.
#[test]
fn mandatory_message_rules_outrank_later_default_ones() {
    let policy = policy(
        r#"<policy context="mandatory">
                <deny send_destination="org.example.Service"/>
                <deny receive_sender="org.example.Service"/>
            </policy>
            <policy context="default">
                <allow send_destination="org.example.Service"/>
                <allow receive_sender="org.example.Service"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    let service = names(&["org.example.Service"]);
    assert!(!peer.may_send(&call(), &service));
    assert!(!peer.may_receive(&call(), &service));
}

/// The highest-ranking match decides, whether it is a rule that names no one, a name or a
/// prefix, and in whichever set of the peer it is.
#[test]
fn the_highest_match_decides_across_lists_and_sets() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_destination="org.example.Service"/>
                <deny send_destination_prefix="org.example"/>
                <allow send_member="Do"/>
            </policy>
            <policy user="alice">
                <deny send_destination="org.example.Service" send_member="Do"/>
            </policy>
            <policy group="wheel">
                <allow send_destination_prefix="org.example" send_member="Do"/>
            </policy>"#,
    );
    let service = names(&["org.example.Service"]);
    let undo = Facts {
        member: Some("Undo"),
        ..call()
    };

    // The rule that names no one outranks the prefix, which outranks the name.
    let bob = policy.for_peer(BOB, &[]);
    assert!(bob.may_send(&call(), &service));
    assert!(!bob.may_send(&undo, &service));
    assert!(bob.may_send(&call(), &[]));
    // The group's prefix outranks the default rules.
    let member = policy.for_peer(BOB, &[WHEEL]);
    assert!(member.may_send(&call(), &service));
    assert!(!member.may_send(&undo, &service));
    // The user's name outranks the group's prefix and the default rules.
    let alice = policy.for_peer(ALICE, &[WHEEL]);
    assert!(!alice.may_send(&call(), &service));
    assert!(alice.may_send(&call(), &names(&["org.example.Other"])));
}

/// Where the highest-ranking rule of a list does not match, a lower one of the list still
/// decides.
#[test]
fn a_lower_rule_decides_where_the_higher_ones_miss() {
    let policy = policy(
        r#"<policy context="default">
                <deny send_destination="org.example.Service"/>
                <allow send_destination="org.example.Service" send_member="Do"/>
                <deny send_destination="org.example.Service" send_member="Never"/>
                <allow send_destination_prefix="org.example" send_member="Do"/>
                <deny send_destination_prefix="org.example" send_member="Never"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
    assert!(peer.may_send(&call(), &names(&["org.example.Other"])));
    let undo = Facts {
        member: Some("Undo"),
        ..call()
    };
    assert!(!peer.may_send(&undo, &names(&["org.example.Service"])));
    assert!(!peer.may_send(&undo, &names(&["org.example.Other"])));
}

/// A peer sends or receives freely where its highest-ranking rule has no condition and
/// allows, and the verdict of such a rule that denies holds for every message.
#[test]
fn an_unconditional_top_rule_decides_every_message() {
    let policy = policy(
        r#"<policy context="default">
                <deny send_destination="org.example.Service"/>
                <allow send_destination="*"/>
                <allow receive_sender="org.example.Service"/>
                <deny eavesdrop="false"/>
            </policy>
            <policy user="alice">
                <deny send_type="method_call"/>
            </policy>"#,
    );

    let bob = policy.for_peer(BOB, &[]);
    assert!(bob.sends_freely());
    assert!(bob.may_send(&call(), &names(&["org.example.Service"])));
    assert!(!bob.receives_freely());
    assert!(!bob.may_receive(&call(), &names(&["org.example.Service"])));
    // Alice's own rule outranks the unconditional one, and has a condition.
    let alice = policy.for_peer(ALICE, &[]);
    assert!(!alice.sends_freely());
    assert!(!alice.may_send(&call(), &[]));
    assert!(alice.may_send(&signal(), &[]));
}

/// The rules about replies and errors are left out, so they neither decide nor keep a peer
/// from sending freely.
#[test]
fn rules_about_replies_are_left_out() {
    let policy = policy(
        r#"<policy context="default">
                <allow send_destination="*"/>
                <deny send_type="method_return"/>
                <deny send_type="error"/>
                <deny send_error="org.example.Error"/>
                <allow receive_sender="org.example.Service"/>
                <deny receive_type="method_return"/>
            </policy>"#,
    );

    let peer = policy.for_peer(ALICE, &[]);
    assert!(peer.sends_freely());
    assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
    assert!(peer.may_receive(&call(), &names(&["org.example.Service"])));
}

/// Everything is free under the stock session.conf.
#[test]
fn the_stock_session_conf_lets_everything_through() {
    let config = Config::read_file("tests/data/session.conf").expect("should parse");
    let policy = Policy::new(&config, &ids(), ALICE);

    assert!(policy.may_connect(ALICE, &[ALICE]));
    let peer = policy.for_peer(ALICE, &[ALICE]);
    assert!(peer.sends_freely());
    assert!(peer.receives_freely());
    assert!(peer.may_own("org.example.Service"));
}

/// Under the stock system.conf, an ordinary user may connect, send signals and talk to the
/// bus, but not call services, use the statistics of the bus, which root may, or own names.
#[test]
fn the_stock_system_conf_holds_back_ordinary_users() {
    let config = Config::read_file("tests/data/system.conf").expect("should parse");
    let ids = Ids::parse("root:x:0:0:::", "");
    let policy = Policy::new(&config, &ids, BUS_UID);
    let bus = names(&["org.freedesktop.DBus"]);
    let service = names(&["org.example.Service"]);
    let hello = Facts {
        path: Some("/org/freedesktop/DBus"),
        interface: Some("org.freedesktop.DBus"),
        member: Some("Hello"),
        ..call()
    };
    let stats = Facts {
        interface: Some("org.freedesktop.DBus.Debug.Stats"),
        member: Some("GetStats"),
        ..hello
    };

    assert!(policy.may_connect(ALICE, &[ALICE]));
    let alice = policy.for_peer(ALICE, &[ALICE]);
    assert!(!alice.sends_freely());
    assert!(!alice.may_send(&call(), &service));
    assert!(alice.may_send(&signal(), &service));
    assert!(alice.may_send(
        &Facts {
            broadcast: true,
            ..signal()
        },
        &[]
    ));
    assert!(alice.may_send(&hello, &bus));
    assert!(!alice.may_send(&stats, &bus));
    assert!(alice.may_receive(&call(), &service));
    assert!(!alice.may_own("org.example.Service"));

    let root = policy.for_peer(ROOT, &[ROOT]);
    assert!(root.may_send(&stats, &bus));
    assert!(!root.may_send(&call(), &service));
    assert!(!root.may_own("org.example.Service"));
}

/// The facts of a message are those of its header, and a signal without a destination is a
/// broadcast.
#[test]
fn facts_of_a_message() {
    let msg = zbus::Message::signal("/org/example", "org.example.Iface", "Changed")
        .build(&())
        .expect("should build");
    let header = msg.header();

    let facts = Facts::new(&msg, &header);
    assert_eq!(facts.msg_type, Type::Signal);
    assert_eq!(facts.path, Some("/org/example"));
    assert_eq!(facts.interface, Some("org.example.Iface"));
    assert_eq!(facts.member, Some("Changed"));
    assert!(facts.broadcast);
    assert_eq!(facts.fds, 0);

    let msg = zbus::Message::method_call("/org/example", "Do")
        .destination("org.example.Service")
        .build(&())
        .expect("should build");
    let header = msg.header();

    let facts = Facts::new(&msg, &header);
    assert_eq!(facts.msg_type, Type::MethodCall);
    assert_eq!(facts.interface, None);
    assert!(!facts.broadcast);
}

/// The policy of the `<policy>` elements of `policies`, for a bus of `BUS_UID`.
fn policy(policies: &str) -> Policy {
    Policy::new(&parse(policies), &ids(), BUS_UID)
}

/// The configuration that has `elements` in it.
fn parse(elements: &str) -> Config {
    Config::parse(&format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
            "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig>{elements}</busconfig>"#
    ))
    .expect("should parse XML input")
}

/// The ids of the users root, messagebus, alice and bob, and of the groups root and wheel.
fn ids() -> Ids {
    Ids::parse(
        "root:x:0:0:::\nmessagebus:x:101:101:::\nalice:x:1000:1000:::\nbob:x:1001:1001:::\n",
        "root:x:0:\nwheel:x:10:alice,bob\n",
    )
}

/// A method call of `org.example.Iface.Do` on `/org/example`, with a destination.
fn call() -> Facts<'static> {
    Facts {
        msg_type: Type::MethodCall,
        path: Some("/org/example"),
        interface: Some("org.example.Iface"),
        member: Some("Do"),
        broadcast: false,
        fds: 0,
    }
}

/// A signal like [`call`], with a destination.
fn signal() -> Facts<'static> {
    Facts {
        msg_type: Type::Signal,
        ..call()
    }
}

/// The well-known names of `names`.
fn names(names: &[&str]) -> Vec<OwnedWellKnownName> {
    names
        .iter()
        .map(|name| OwnedWellKnownName::try_from(*name).expect("should be a well-known name"))
        .collect()
}

const BUS_UID: u32 = 101;
const ROOT: u32 = 0;
const ALICE: u32 = 1000;
const BOB: u32 = 1001;
const WHEEL: u32 = 10;
