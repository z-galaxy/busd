# Benchmarks

busd has two sets of benchmarks: `bus`, which CodSpeed runs on every change, to catch what slows
busd down, and `compare`, a comparison with other brokers, for the numbers of the
[README](../README.md#performance), which tell how busd fares against them.

Both have clients of their own make traffic through the bus: method calls, by one client and by
many at once, with a byte array or a dict of dicts of variants for their body; signals, as a burst
to one subscriber and as a broadcast to many; and many callers that share one connection. The
clients leave the sender out of their messages, as those of libdbus, GLib and sd-bus do, so that
the bus has to set it, as it must for most real clients. They are the workloads of [dbus-bench],
which measured busd with them before, and keeps those results.

## On CodSpeed: `bus`

```sh
cargo bench --bench bus
```

The bus runs in the process of the benchmark, on a thread of its own, as busd runs it: the
connections of all its clients on one runtime. The clients and the services run on threads of
their own, and connect to it over unix sockets. Each id connects them first, untimed, and then
times rounds of traffic on those connections, each from the clients being told to start until the
last of them is done. The clients build their calls once, and send them again in every round, and
the emitter its signals, so that the rounds time the bus rather than the building of the messages:

| Id | A round |
|---|---|
| `method-call/1-client` | 50 calls with 16 KiB of data, one after the other, by a client |
| `method-call/1-client-64B` | 100 such calls with 64 bytes |
| `method-call/20-clients` | 10 calls with 16 KiB by each of 20 clients at once |
| `method-call/100-clients` | 5 such calls by each of 100 clients at once |
| `method-call/100-callers-1-connection` | 5 such calls by each of 100 callers on one connection |
| `dicts/1-client`, `dicts/20-clients` | those calls, with a dict of dicts of 100 variants |
| `signal/burst-to-1` | 1000 signals with 1 KiB of data, back to back, to one subscriber |
| `signal/broadcast-to-20` | 100 such signals to each of 20 subscribers |
| `connection/hello` | a client connects, authenticates and says `Hello`, then goes |

Each client calls a service of its own, at a well-known name, but for those of
`method-call/100-clients`, which are called at the unique names of their connections, and those of
`method-call/100-callers-1-connection`, which share one.

CodSpeed measures them by the clock, on a bare-metal machine of 16 cores, for every push to `main`
and every pull request, and reports how a pull request changes them
(`.github/workflows/bench.yml`). The threads of an id share its clients, and its services, between
at most four of them each, so that every thread, the bus's included, has a CPU of its own there,
which it is pinned to.

## Against other brokers: `compare`

```sh
cargo bench --bench compare
```

This builds busd as it ships, with `cargo build --release`, and runs each workload against it and
against [dbus-broker] and [dbus-daemon], those of them that are installed, round after round: 60
rounds, or 40 for the shared connection, after 3 of warm-up. That takes half an hour or more. It
needs Linux, and leaves out the brokers that are missing, with a note: dbus-broker needs
`dbus-broker-launch`, `systemd-socket-activate` and journald, as a desktop's systemd provides them.
Keep the machine otherwise idle while it runs.

A quick check that everything works:

```sh
cargo bench --bench compare -- --rounds 2 --shared-rounds 2 --warmup 1
```

and busd of this checkout against another revision of it, such as `main`, as `baseline`, which is
built the same way:

```sh
cargo bench --bench compare -- --baseline main
```

`--workload` picks some of the workloads, `--broker` the brokers (another one as
`LABEL=COMMAND`), and `--help` lists the rest.

| Workload | Its two phases, or three |
|---|---|
| `calls` | 1000 calls with 16 KiB of data by one client, then 1000 by each of 20 clients at once |
| `signals` | 40,000 signals with 1 KiB of data to one subscriber, then 6000 to each of 20 |
| `latency` | the round trips of calls by one client, then 20; the delays of a broadcast to 20 |
| `shared` | 100 callers that share a connection, then 100 clients, 1000 calls each |
| `dicts` | the calls of `calls`, with a dict of dicts of 100 variants for their body |

Each run starts a broker of its own, runs the client of a workload against it once, as a process of
its own, and stops the broker, and the brokers take turns in an order shuffled anew for each
round, so that whatever drift the machine has hits all of them alike. Of each run, the comparison
records what the client measured, the CPU time of the broker and of the client, in all and in each
phase, and the broker's peak memory (`benches/compare/main.rs` says what each field is). The
results go to a directory of their own, `target/compare/results-<revision>-<UTC date and time>`,
unless `--out` names another:

* `summary.md`: the medians of each workload, in the shape of the README's tables, and how much
  busd differs from each other broker. It is printed at the end too.
* `<workload>.json`: every run, in the shape of the results of dbus-bench, and `<workload>.md`, the
  statistics of each field.
* `<workload>-busd-vs-<broker>.stats.md` and `.stats.json`: busd against each other broker, in
  pairs of runs of the same round, which cancels most of the drift of the machine, with the
  bootstrap intervals of each difference and a verdict on whether it exceeds `--margin` (5% by
  default).
* `environment.txt`: the versions of the kernel, the compiler, the clients and the other brokers.

`pairs` compares two brokers of a results file in pairs again, those of dbus-bench included:

```sh
cargo bench --bench compare -- pairs target/compare/results-REV-TIME/calls.json \
    --baseline dbus-broker --candidate busd
```

Under `cargo bench`, Ctrl-C makes cargo exit at once, while the comparison goes on to stop the
broker of the run under way and write the results of those that finished, until it says where they
are. The executable that `cargo bench --bench compare --no-run` names stops in the foreground.

The clients are those of dbus-bench, but on busd's own locked zbus and zruntime, built as
`cargo bench` builds them: with busd's release profile (opt-level "s", fat LTO and one codegen
unit), and with the features of zbus that busd and the other benchmarks need, which cargo unifies.
dbus-bench pinned its clients to a zbus and a zruntime of their own, built with opt-level 3: on a
VM of 4 vCPUs, the clients built this way took 3.5% longer on the calls of 20 clients through
dbus-broker. So the numbers compare with each other within a comparison, and only roughly with
dbus-bench's, or with those of a checkout whose zbus differs.

[dbus-bench]: https://github.com/zeenix/dbus-bench
[dbus-broker]: https://github.com/bus1/dbus-broker
[dbus-daemon]: https://gitlab.freedesktop.org/dbus/dbus
