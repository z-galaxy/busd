<p align="center">
  <a href="https://crates.io/crates/busd">
    <img alt="crates.io" src="https://img.shields.io/crates/v/busd">
  </a>
  <a href="https://github.com/z-galaxy/busd/actions/workflows/ci.yml">
    <img alt="Build Status"
      src="https://github.com/z-galaxy/busd/actions/workflows/ci.yml/badge.svg">
  </a>
  <a href="https://app.codspeed.io/z-galaxy/busd?utm_source=badge">
    <img alt="CodSpeed" src="https://img.shields.io/endpoint?url=https://codspeed.io/badge.json">
  </a>
</p>

<p align="center">
  <img alt="Project logo: a bus"
    src="https://raw.githubusercontent.com/z-galaxy/busd/d1eebb1097c46e3a195d9994c4539c7ec51b5266/data/logo.svg"
    width="200">
</p>

<h1 align="center">busd</h1>

A D-Bus bus (broker) implementation in Rust. Since it's pure Rust, it's much easier to cross-build
than other D-Bus brokers.

## Status

Alpha. The essentials are in place, but busd is not ready for production use yet. It is meant to
serve as a system bus as well as a session bus, but until it enforces the [policies] of its
configuration, it can only serve as a session bus, and it doesn't start services on demand
([service activation]) yet. The [feature parity] issues track what else it lacks.

## Performance

busd is as fast as [dbus-broker], the broker that most systems run, on method calls, faster than it
on signals, mostly with less memory than it, and well ahead of [dbus-daemon]. Measured with its
[comparison with other brokers](benches/README.md#against-other-brokers-compare) on a laptop with
an AMD Ryzen 7 7840U, running Fedora 44, as the medians of 60 rounds:

| Broker             | 1 client | 20 clients | CPU time with 20 clients | Peak memory |
|--------------------|---------:|-----------:|-------------------------:|------------:|
| busd               |  0.042 s |    0.289 s |                   0.28 s |     7.0 MiB |
| dbus-broker 37     |  0.044 s |    0.289 s |                   0.27 s |     8.1 MiB |
| dbus-daemon 1.16.2 |  0.090 s |    1.067 s |                   1.05 s |     6.5 MiB |

Each client makes 1000 method calls with 16 KiB of data each, and the 20 clients make theirs all
at once: busd takes 5.5% less time than dbus-broker with one client, and as long with 20, with 2%
more CPU time.

| Broker             | Burst to 1 subscriber | Broadcast to 20 | CPU time of the broadcast | Peak memory |
|--------------------|----------------------:|----------------:|--------------------------:|------------:|
| busd               |               0.114 s |         0.060 s |                    0.04 s |     6.3 MiB |
| dbus-broker 37     |               0.175 s |         0.181 s |                    0.16 s |     7.9 MiB |
| dbus-daemon 1.16.2 |               0.774 s |         1.907 s |                    1.88 s |     5.5 MiB |

The burst is 40,000 signals with 1 KiB of data each, to one subscriber, and the broadcast 6000 of
them to each of 20 subscribers: busd gets the burst through 35% faster than dbus-broker, and the
broadcast 67% faster, with 74% less CPU time, as it writes several of them to a subscriber with
each system call. The memory of dbus-broker is that of its two processes, its launcher and the
broker proper, together.

| Broker             | Round trip, 1 client | Round trip, 20 clients | Fan-out to 20  |
|--------------------|---------------------:|-----------------------:|---------------:|
| busd               |           30 / 53 µs |           274 / 437 µs |   0.7 / 1.4 ms |
| dbus-broker 37     |           31 / 58 µs |           278 / 391 µs |   1.9 / 3.1 ms |
| dbus-daemon 1.16.2 |          64 / 137 µs |         1022 / 1362 µs | 19.6 / 30.6 ms |

The latencies are the medians over the rounds of their p50 and their p99: the round trips of 5000
calls with 64 bytes of data each, by one client, and of the calls of the 20 clients above, and the
delays of the signals of a broadcast of 2000 of them to 20 subscribers, from just before each is
built until the subscriber takes it off its stream. busd delivers the signals 61% sooner than
dbus-broker at the median, but the slowest 1% of the round trips of 20 clients take it 11% longer.

That, and many clients at once, is where busd still lags behind dbus-broker: with 100 clients, each
calling a service of its own, it takes 10% longer, with 14% more CPU time and 57% more memory. With
100 callers that share one connection, it takes 29% less time, and with a dict of dicts of 100
variants for the body of the calls, which costs a broker far more to check than a byte array, 34%
less with 20 clients. The [benchmarks](benches/README.md) say how these are measured.

## Installation & Use

Currently, we can only offer installation from source:

```bash
cargo install -f busd
```

Running a session instance is super easy:

```bash
busd --print-address
```

`--print-address` will print the address of the bus to stdout. By default, busd listens where the
session configuration of [dbus-daemon], `/usr/share/dbus-1/session.conf`, tells it to: usually on a
socket in `/tmp`. Without that file, it listens on a socket in the user's runtime directory,
`$XDG_RUNTIME_DIR`, or in the directory for temporary files if there's none. You can then use the
address that busd printed to connect to the bus:

```bash
export DBUS_SESSION_BUS_ADDRESS="unix:path=/tmp/dbus-2818978930,guid=794623ae7cf44eb69a27f57e9ea4af6b"
busctl --user introspect org.freedesktop.DBus /org/freedesktop/DBus
busctl --user list
```

Since auto-starting of services is not yet implemented, you'll have to start services manually:

```bash
# Probably not the best example since the service just exits after a call to it.
/usr/libexec/dleyna-renderer-service &
busctl call --user com.intel.dleyna-renderer /com/intel/dLeynaRenderer com.intel.dLeynaRenderer.Manager GetRenderers
```

### Under systemd

On Linux, busd listens on the socket that systemd's [socket activation] passes, if any, rather than
on one of its own, and tells systemd once it's ready, for a service of `Type=notify`.

With a [file descriptor store], under systemd 254 or later, busd survives its restarts without its
clients noticing anything but a pause. On `SIGTERM`, rather than disconnect them, it stops taking
their messages, finishes what it took, such as forwarding those messages and answering the calls to
it among them, and hands their connections and its state over to systemd: their unique names, the
well-known names that they own or queue for, their match rules, the calls that wait for their
replies, and what zbus read of their messages but busd hasn't taken yet, as the start of one that a
client is in the middle of sending, with the file descriptors that came with it. On a restart,
systemd hands all of that over to the next instance of busd, which serves the clients on from
there, on the same connections. On a stop, systemd closes them. A session bus, for example:

```ini
# ~/.config/systemd/user/busd.socket
[Socket]
ListenStream=%t/bus

[Install]
WantedBy=sockets.target
```

```ini
# ~/.config/systemd/user/busd.service
[Service]
Type=notify
ExecStart=/usr/bin/busd --session
# One for each client, and for each file descriptor that came with the messages of a client that
# busd didn't take as it stops, and two more. Those that don't fit get disconnected on a restart.
FileDescriptorStoreMax=4096
# systemd passes the next instance what was stored under its soft limit of open files, which has to
# take that and a few more.
LimitNOFILE=8192:524288
```

`systemctl --user restart busd` then restarts busd, as after an update, with its clients connected
throughout. The store only keeps what busd hands over until the next instance takes it, which
`systemctl restart` starts right away, while `systemctl stop` followed by `systemctl start` doesn't
keep it. Without socket activation, busd keeps the socket that it listens on in the store too, and
as it can't tell a stop from a restart, it leaves the file of the socket behind on a stop, which it
replaces as it starts again. That takes the address of a socket file (`unix:path=`), rather than of
a directory for busd to make one in (`unix:dir=`, `unix:tmpdir=`), where it makes one of another
name each time that it starts afresh, and the files that it left behind stay. A new address only
applies once busd starts afresh, too, after a stop, as a restart keeps the socket that busd listens
on. Lower `LimitNOFILE=` with a stop and a start as well, rather than a restart: busd fits what it
hands over under the limit that it started with, and systemd fails to start the next instance if
that doesn't fit under the lower one.

### Profile-guided optimization

Those who build busd for others, such as distributions, can optimize it for the traffic of a bus,
with [profile-guided optimization] (PGO), by building it with `build-pgo.sh` rather than
`cargo build --release`:

```bash
rustup component add llvm-tools
./build-pgo.sh
```

The script builds busd with instrumentation first, has the clients of the
[comparison with other brokers](benches/README.md#against-other-brokers-compare) make their traffic
through it, once for each workload, and then builds it as it ships, with the profile of that
traffic, to `target/<host>/release/busd`, such as `target/x86_64-unknown-linux-gnu/release/busd`,
whose path it prints. On those workloads, on a cloud VM of 4 vCPUs, such a build took about 9% less
time than one without PGO with 20 or 100 clients at once, and 5 to 13% less CPU time, but little or
no less with one client. Its binary is 6% larger. A bus whose traffic differs from what busd trains
on may gain less.

The script needs the `llvm-profdata` of the LLVM that rustc is built with, which rustup's
`llvm-tools` component has, or another one in `LLVM_PROFDATA`, and busd's dev-dependencies, for the
clients. It passes its arguments, such as `--no-default-features`, on to both builds of busd, and
adds the flags of PGO to those of `RUSTFLAGS`, or else to those of `target.<triple>.rustflags` in
cargo's configuration: cargo then leaves out those of `build.rustflags`. The comparison measures
such a build against a plain one as another broker:

```bash
cargo bench --bench compare -- \
    --broker "busd-pgo=target/x86_64-unknown-linux-gnu/release/busd --address {address}"
```

## The plan

### Full compatibility with the D-Bus specification

Implement all features that the [specification] mandates or recommends. Where the specification
leaves the behaviour of a bus open, do as [dbus-broker] does.

### A system bus, as well as a session bus

Serve as the system bus of a machine, which takes the [policies] of its configuration, and limits on
what the clients of each user may have the bus keep, as with dbus-broker, so that no user can
exhaust the bus, or get around the policies, at the expense of the others.

### Additional Header Fields

While the D-Bus spec does not allow custom header fields in messages, `busd` will still support a
few additional on-demand (only) fields that are useful for certain applications. One example is the
addition of [peer credentials] to every message, which can avoid round trips on the bus.

## License

[MIT](LICENSE-MIT)

## Sponsors

<a href="https://codspeed.io/?utm_source=oss-sponsorship&utm_medium=z-galaxy">
  <picture>
    <source media="(prefers-color-scheme: dark)"
      srcset="https://codspeed.io/codspeed-logo-dark.svg">
    <source media="(prefers-color-scheme: light)"
      srcset="https://codspeed.io/codspeed-logo-light.svg">
    <img alt="CodSpeed logo" src="https://codspeed.io/codspeed-logo-light.svg" width="400">
  </picture>
</a>

[specification]: https://dbus.freedesktop.org/doc/dbus-specification.html
[profile-guided optimization]: https://doc.rust-lang.org/rustc/profile-guided-optimization.html
[socket activation]: https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html
[file descriptor store]: https://systemd.io/FILE_DESCRIPTOR_STORE/
[peer credentials]: https://github.com/z-galaxy/busd/issues/29
[dbus-broker]: https://github.com/bus1/dbus-broker
[dbus-daemon]: https://gitlab.freedesktop.org/dbus/dbus
[policies]: https://github.com/z-galaxy/busd/issues/79
[service activation]: https://github.com/z-galaxy/busd/issues/82
[feature parity]: https://github.com/z-galaxy/busd/labels/feature%20parity
