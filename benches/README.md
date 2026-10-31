# Benchmarks

The benchmarks have clients of their own make traffic through the bus: method calls, by one client
and by many at once, with a byte array or a dict of dicts of variants for their body; signals, as a
burst to one subscriber and as a broadcast to many; many callers that share one connection; and
connecting. The clients leave the sender out of their messages, as those of libdbus, GLib and
sd-bus do, so that the bus has to set it, as it must for most real clients. They took over from
[dbus-bench], which measured busd with these workloads until then, and keeps its results.

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

[dbus-bench]: https://github.com/zeenix/dbus-bench
