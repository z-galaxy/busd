<img alt="Project logo: a bus" src="data/logo.svg" width="200">

[![Build Status](https://img.shields.io/github/actions/workflow/status/z-galaxy/busd/ci.yml?branch=main)](https://github.com/z-galaxy/busd/actions?query=branch%3Amain)
[![crates.io](https://img.shields.io/crates/v/busd.svg)](https://crates.io/crates/busd)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/z-galaxy/busd?utm_source=badge)

# busd

A D-Bus bus (broker) implementation in Rust. Since it's pure Rust, it's much easier to cross-build
than other D-Bus brokers.

## Status

Alpha. It's not ready for production use yet. Only the essentials are in place.

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

`--print-address` will print the address of the bus to stdout. You can then use that address to
connect to the bus:

```bash
export DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/1000/bus,guid=d0af79a44c000ce7985797ba649dbc05"
busctl --user introspect org.freedesktop.DBus /org/freedesktop/DBus
busctl --user list
```

Since auto-starting of services is not yet implemented, you'll have to start services manually:

```bash
# Probably not the best example since the service just exits after a call to it.
/usr/libexec/dleyna-renderer-service &
busctl call --user com.intel.dleyna-renderer /com/intel/dLeynaRenderer com.intel.dLeynaRenderer.Manager GetRenderers
```

## The plan

### Full compatibility with the D-Bus specification

Implement all features that manadated and recommended by the [specification].

### Additional Header Fields

While the D-Bus spec does not allow custom header fields in messages, `busd` will still support a
few additional on-demand (only) fields, that are useful for certain applications. One example is
addition of [peer credentials] on every message, which can avoid round-trips on the bus.

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
[peer credentials]: https://github.com/z-galaxy/busd/issues/29
[dbus-broker]: https://github.com/bus1/dbus-broker
[dbus-daemon]: https://gitlab.freedesktop.org/dbus/dbus
