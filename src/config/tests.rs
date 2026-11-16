use rule::{Access, ConnectOperation, NameOwnership, Operation, ReceiveOperation, SendOperation};

use super::*;

#[test]
fn config_parse_with_dtd_and_root_element_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig></busconfig>
        "#;
    Config::parse(input).expect("should parse XML input");
}

#[test]
#[should_panic]
fn config_parse_with_type_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <type>not-a-valid-message-bus-type</type>
        </busconfig>
        "#;
    Config::parse(input).expect("should parse XML input");
}

#[test]
fn config_parse_with_fork_and_keep_umask_and_syslog_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <allow_anonymous />
            <fork />
            <keep_umask/>
            <syslog />
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    // `<allow_anonymous />` is ignored.
    assert_eq!(
        config,
        Config {
            fork: true,
            keep_umask: true,
            syslog: true,
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_auth_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <auth>ANONYMOUS</auth>
            <auth>EXTERNAL</auth>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    // `<auth>` is ignored.
    assert_eq!(config, Config::default());
}

#[test]
fn config_parse_with_limit_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <limit name="max_incoming_bytes">1000000000</limit>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[test]
fn config_parse_with_listen_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <listen>unix:path=/tmp/foo</listen>
            <listen>unix:path=/tmp/bar</listen>
            <listen>tcp:host=localhost,port=0,family=ipv4</listen>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    // The last unix socket wins, and the TCP address is ignored.
    assert_eq!(
        config,
        Config {
            listen: Some(Address::from_str("unix:path=/tmp/bar").expect("should parse address")),
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_overlapped_lists_ok() {
    // confirm this works with/without quick-xml's [`overlapped-lists`] feature
    // [`overlapped-lists`]: https://docs.rs/quick-xml/latest/quick_xml/#overlapped-lists
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <auth>ANONYMOUS</auth>
            <listen>unix:path=/tmp/foo</listen>
            <policy context="default">
                <allow own="*"/>
                <deny own="*"/>
                <allow own="*"/>
            </policy>
            <auth>EXTERNAL</auth>
            <listen>tcp:host=localhost,port=1234</listen>
            <policy context="default">
                <deny own="*"/>
                <allow own="*"/>
                <deny own="*"/>
            </policy>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            listen: Some(Address::from_str("unix:path=/tmp/foo").expect("should parse address")),
            policies: vec![
                Policy::DefaultContext(vec![
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                    Rule {
                        access: Access::Deny,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                ]),
                Policy::DefaultContext(vec![
                    Rule {
                        access: Access::Deny,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                    Rule {
                        access: Access::Deny,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    },
                ]),
            ],
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_pidfile_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <pidfile>/var/run/busd.pid</pidfile>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            pidfile: Some(PathBuf::from("/var/run/busd.pid")),
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_policies_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy context="default">
                <allow own="org.freedesktop.DBus"/>
                <allow own_prefix="org.freedesktop"/>
                <allow group="wheel" />
                <allow user="root" />
            </policy>
            <policy user="root">
                <allow
                    send_broadcast="true"
                    send_destination="org.freedesktop.DBus"
                    send_error="something bad"
                    send_interface="org.freedesktop.systemd1.Activator"
                    send_member="DoSomething"
                    send_path="/org/freedesktop"
                    send_type="signal"
                    max_fds="128"
                    min_fds="12"
                    />
                <allow
                    receive_error="something bad"
                    receive_interface="org.freedesktop.systemd1.Activator"
                    receive_member="DoSomething"
                    receive_path="/org/freedesktop"
                    receive_sender="org.freedesktop.DBus"
                    receive_type="signal"
                    max_fds="128"
                    min_fds="12"
                    />
            </policy>
            <policy group="network">
                <allow send_destination_prefix="org.freedesktop" send_member="DoSomething" />
                <allow receive_sender="org.freedesktop.Avahi" receive_member="DoSomething"/>
            </policy>
            <policy context="mandatory">
                <deny send_destination="net.connman.iwd"/>
            </policy>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            policies: vec![
                Policy::DefaultContext(vec![
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Exact(String::from("org.freedesktop.DBus")))
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Own(NameOwnership {
                            own: Some(Name::Prefix(String::from("org.freedesktop")))
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Connect(ConnectOperation {
                            group: Some(String::from("wheel")),
                            user: None,
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Connect(ConnectOperation {
                            group: None,
                            user: Some(String::from("root")),
                        })
                    },
                ]),
                Policy::User(
                    vec![
                        Rule {
                            access: Access::Allow,
                            operation: Operation::Send(SendOperation {
                                broadcast: Some(true),
                                destination: Some(Name::Exact(String::from(
                                    "org.freedesktop.DBus"
                                ))),
                                error: Some(String::from("something bad")),
                                interface: Some(String::from("org.freedesktop.systemd1.Activator")),
                                max_fds: Some(128),
                                member: Some(String::from("DoSomething")),
                                min_fds: Some(12),
                                path: Some(String::from("/org/freedesktop")),
                                r#type: Some(MessageType::Signal),
                            })
                        },
                        Rule {
                            access: Access::Allow,
                            operation: Operation::Receive(ReceiveOperation {
                                error: Some(String::from("something bad")),
                                interface: Some(String::from("org.freedesktop.systemd1.Activator")),
                                max_fds: Some(128),
                                member: Some(String::from("DoSomething")),
                                min_fds: Some(12),
                                path: Some(String::from("/org/freedesktop")),
                                sender: Some(String::from("org.freedesktop.DBus")),
                                r#type: Some(MessageType::Signal),
                            })
                        }
                    ],
                    String::from("root")
                ),
                Policy::Group(
                    vec![
                        Rule {
                            access: Access::Allow,
                            operation: Operation::Send(SendOperation {
                                broadcast: None,
                                destination: Some(Name::Prefix(String::from("org.freedesktop"))),
                                error: None,
                                interface: None,
                                max_fds: None,
                                member: Some(String::from("DoSomething")),
                                min_fds: None,
                                path: None,
                                r#type: None
                            })
                        },
                        // `<allow send_member=...` should be dropped
                        Rule {
                            access: Access::Allow,
                            operation: Operation::Receive(ReceiveOperation {
                                sender: Some(String::from("org.freedesktop.Avahi")),
                                error: None,
                                interface: None,
                                max_fds: None,
                                member: Some(String::from("DoSomething")),
                                min_fds: None,
                                path: None,
                                r#type: None
                            })
                        },
                    ],
                    String::from("network")
                ),
                Policy::MandatoryContext(vec![Rule {
                    access: Access::Deny,
                    operation: Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("net.connman.iwd"))),
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None
                    })
                },]),
            ],
            ..Default::default()
        }
    );
}

#[should_panic]
#[test]
fn config_parse_with_policies_with_group_and_user_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy user="root">
                <allow group="wheel" user="root" />
            </policy>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[test]
fn config_parse_with_policies_with_ignored_rules_and_rule_attributes_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy context="default">
                <allow send_destination="*" eavesdrop="true"/>
                <allow eavesdrop="true"/>
                <deny eavesdrop="true"/>
                <deny send_requested_reply="true" send_type="method_return"/>
                <allow send_requested_reply="false" send_type="method_return"/>
                <deny receive_requested_reply="true" receive_type="error"/>
                <allow receive_requested_reply="false" receive_type="error"/>
            </policy>
            <policy at_console="true">
                <allow send_destination="org.freedesktop.DBus" send_interface="org.freedesktop.systemd1.Activator"/>
            </policy>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            policies: vec![
                Policy::DefaultContext(vec![
                    Rule { access: Access::Allow, operation: // `eavesdrop="true"` is dropped, keep other attributes
                        Operation::Send(SendOperation {
                            broadcast: None,
                            destination: Some(Name::Any),
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            r#type: None
                        }) },
                    // `<allow eavesdrop="true"/>` is a receive rule with no restriction
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Receive(ReceiveOperation {
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            sender: None,
                            r#type: None
                        })
                    },
                    // `<deny eavesdrop="true"/>` is dropped: it only concerns eavesdropping
                    // `send_requested_reply` is dropped, keep other attributes
                    Rule {
                        access: Access::Deny,
                        operation: Operation::Send(SendOperation {
                            broadcast: None,
                            destination: None,
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            r#type: Some(MessageType::MethodReturn)
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Send(SendOperation {
                            broadcast: None,
                            destination: None,
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            r#type: Some(MessageType::MethodReturn)
                        })
                    },
                    // `receive_requested_reply` is dropped, keep other attributes
                    Rule {
                        access: Access::Deny,
                        operation: Operation::Receive(ReceiveOperation {
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            sender: None,
                            r#type: Some(MessageType::Error)
                        })
                    },
                    Rule {
                        access: Access::Allow,
                        operation: Operation::Receive(ReceiveOperation {
                            error: None,
                            interface: None,
                            max_fds: None,
                            member: None,
                            min_fds: None,
                            path: None,
                            sender: None,
                            r#type: Some(MessageType::Error)
                        })
                    },
                ]),
                Policy::Unsupported,
            ],
            ..Default::default()
        }
    );
}

#[should_panic]
#[test]
fn config_parse_with_policies_with_own_and_own_prefix_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy user="root">
                <allow own="org.freedesktop.DBus" own_prefix="org.freedesktop" />
            </policy>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[should_panic]
#[test]
fn config_parse_with_policies_with_send_destination_and_send_destination_prefix_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy user="root">
                <allow send_destination="org.freedesktop.DBus" send_destination_prefix="org.freedesktop" />
            </policy>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[should_panic]
#[test]
fn config_parse_with_policies_with_send_and_receive_attributes_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy user="root">
                <allow send_destination="org.freedesktop.DBus" receive_sender="org.freedesktop.Avahi" />
            </policy>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[should_panic]
#[test]
fn config_parse_with_policies_without_attributes_error() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy user="root">
                <allow />
            </policy>
        </busconfig>
        "#;

    Config::parse(input).expect("should parse XML input");
}

#[test]
fn config_parse_with_servicedir_and_standard_session_servicedirs_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <servicedir>/example</servicedir>
            <standard_session_servicedirs />
            <servicedir>/anotherexample</servicedir>
            <standard_session_servicedirs />
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    // TODO: improve test: contents are dynamic depending upon environment variables
    assert_eq!(config.servicedirs.first(), Some(&PathBuf::from("/example")));
    assert_eq!(
        config.servicedirs.last(),
        Some(&PathBuf::from("/usr/share/dbus-1/services"))
    );
}

#[test]
fn config_parse_with_servicedir_and_standard_system_servicedirs_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <servicedir>/example</servicedir>
            <standard_system_servicedirs />
            <servicedir>/anotherexample</servicedir>
            <standard_system_servicedirs />
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            servicedirs: vec![
                PathBuf::from("/example"),
                PathBuf::from("/usr/local/share/dbus-1/system-services"),
                PathBuf::from("/usr/share/dbus-1/system-services"),
                PathBuf::from("/lib/dbus-1/system-services"),
                PathBuf::from("/anotherexample"),
                PathBuf::from("/usr/local/share/dbus-1/system-services"),
                PathBuf::from("/usr/share/dbus-1/system-services"),
                PathBuf::from("/lib/dbus-1/system-services"),
            ],
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_servicehelper_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <servicehelper>/example</servicehelper>
            <servicehelper>/anotherexample</servicehelper>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            servicehelper: Some(PathBuf::from("/anotherexample")),
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_type_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <type>session</type>
            <type>system</type>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            r#type: Some(BusType::System),
            ..Default::default()
        }
    );
}

#[test]
fn config_parse_with_user_ok() {
    let input = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <user>1000</user>
            <user>alice</user>
        </busconfig>
        "#;

    let config = Config::parse(input).expect("should parse XML input");

    assert_eq!(
        config,
        Config {
            user: Some(String::from("alice")),
            ..Default::default()
        }
    );
}
