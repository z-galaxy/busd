#!/bin/bash
#
# Builds busd with profile-guided optimization (PGO): first with instrumentation, which records
# what busd does while the clients of the comparison with other brokers, in `benches/compare`, make
# their traffic through it, once for each workload, and then again, as it ships, with the profile
# of that traffic, which tells the compiler what to optimize for.
#
#     rustup component add llvm-tools
#     ./build-pgo.sh [ARGUMENTS OF CARGO BUILD]
#
# It prints the path of busd once it's built: `target/<host>/release/busd`, such as
# `target/x86_64-unknown-linux-gnu/release/busd`, or the same in `CARGO_TARGET_DIR`. The
# instrumented build and the profile stay in `target/pgo`. The arguments go to both builds of busd,
# such as `--no-default-features`, and so do the flags of `RUSTFLAGS`, which those of PGO are added
# to.
#
# The profile is merged by the llvm-profdata of `LLVM_PROFDATA`, if it's set, or else by that of
# rustup's llvm-tools component, or else by the one on `PATH`. It must be of the LLVM that rustc is
# built with. The clients need busd's dev-dependencies to build.

set -euo pipefail

fail() {
    echo "error: $*" >&2
    exit 1
}

# A relative `CARGO_TARGET_DIR` is relative to the current directory, which the script leaves for
# the checkout.
case ${CARGO_TARGET_DIR:-} in
'' | /*) ;;
*) CARGO_TARGET_DIR=$PWD/$CARGO_TARGET_DIR ;;
esac
cd "$(dirname "$0")"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

pgo=$CARGO_TARGET_DIR/pgo
profiles=$pgo/profiles
profile=$pgo/busd.profdata
# `RUSTFLAGS` is split at whitespace.
case $pgo in
*[[:space:]]*) fail "RUSTFLAGS can't name $pgo in the flags of PGO, as it has whitespace" ;;
esac

# Naming the target, the host's, keeps `RUSTFLAGS` away from build scripts and procedural macros,
# which would be instrumented too otherwise.
host=$(rustc -vV | sed -n 's/^host: //p')

if [ -n "${LLVM_PROFDATA:-}" ]; then
    llvm_profdata=$(command -v "$LLVM_PROFDATA") ||
        fail "LLVM_PROFDATA names $LLVM_PROFDATA, which is no program"
else
    llvm_profdata=$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata
    if [ ! -x "$llvm_profdata" ]; then
        llvm_profdata=$(command -v llvm-profdata) ||
            fail "no llvm-profdata: install one with \`rustup component add llvm-tools\`," \
                "or name one in LLVM_PROFDATA"
    fi
fi
# Each major version of LLVM has a format of its own for the profiles, which the others may not
# read.
llvm=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
profdata_llvm=$("$llvm_profdata" --version | sed -n 's/.*LLVM version \([0-9]*\).*/\1/p' |
    head -n 1)
if [ -n "$llvm" ] && [ -n "$profdata_llvm" ] && [ "$llvm" != "$profdata_llvm" ]; then
    fail "$llvm_profdata is of LLVM $profdata_llvm, but rustc of LLVM $llvm: install that of" \
        "rustc with \`rustup component add llvm-tools\`, or name one of LLVM $llvm in LLVM_PROFDATA"
fi

echo "Building the clients that busd is trained with" >&2
cargo bench --locked --bench compare --no-run

echo "Building busd with instrumentation" >&2
RUSTFLAGS="${RUSTFLAGS:-} -Cprofile-generate=$profiles" \
    cargo build --release --locked --bin busd --target "$host" --target-dir "$pgo/instrumented" "$@"
instrumented=$pgo/instrumented/$host/release/busd

# Runs of busd add to the profiles that they find there, and those of an earlier build would be
# merged too.
rm -rf "$profiles"
# busd writes its profile where `-Cprofile-generate` says, unless this says otherwise.
unset LLVM_PROFILE_FILE

run=$(mktemp -d)
pid=
stop() {
    if [ -n "$pid" ]; then
        kill "$pid" 2> /dev/null || true
        wait "$pid" 2> /dev/null || true
    fi
    rm -rf "$run"
}
trap stop EXIT

# busd writes a line to it once it listens, and closes it: if busd fails first, the read that waits
# for the line ends without one.
mkfifo "$run/ready"
for workload in calls signals latency shared dicts; do
    echo "Training busd with the $workload workload" >&2
    address=unix:path=$run/$workload
    "$instrumented" --address "$address" --ready-fd 3 3> "$run/ready" 2> "$run/busd.log" &
    pid=$!
    if ! read -r -t 60 _ < "$run/ready"; then
        cat "$run/busd.log" >&2
        fail "the instrumented busd didn't start"
    fi

    cargo bench --locked --quiet --bench compare -- client "$workload" --address "$address" \
        > /dev/null

    # busd writes its profile as it exits, which it does with 0 on SIGTERM. It aborts on a panic,
    # which loses the profile.
    kill -TERM "$pid"
    status=0
    wait "$pid" || status=$?
    pid=
    if [ "$status" -ne 0 ]; then
        cat "$run/busd.log" >&2
        fail "the instrumented busd exited with $status, so its profile is lost"
    fi
done

if ! compgen -G "$profiles/*.profraw" > /dev/null; then
    fail "the instrumented busd left no profile in $profiles: is it instrumented? Cargo" \
        "ignores RUSTFLAGS if CARGO_ENCODED_RUSTFLAGS is set"
fi
echo "Merging the profile into $profile" >&2
"$llvm_profdata" merge -o "$profile" "$profiles"

echo "Building busd with the profile" >&2
RUSTFLAGS="${RUSTFLAGS:-} -Cprofile-use=$profile" \
    cargo build --release --locked --bin busd --target "$host" "$@"

echo "$CARGO_TARGET_DIR/$host/release/busd"
