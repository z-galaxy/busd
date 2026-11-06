extern crate busd;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{
    fs::File,
    io::Write,
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    pin::pin,
};

use busd::{bus, config::Config, error, info, warn};

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::future::{select, Either};
use rustix::process::{getrlimit, setrlimit, Resource};
use zruntime::{
    signal::{Signal, Signals},
    SharedRuntime,
};

/// A simple D-Bus broker.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The address to listen on.
    /// Takes precedence over any `<listen>` element in the configuration file.
    #[clap(short = 'a', long, value_parser)]
    address: Option<String>,

    /// Use the given configuration file.
    #[clap(long)]
    config: Option<PathBuf>,

    /// Print the address of the message bus to standard output.
    #[clap(long)]
    print_address: bool,

    /// File descriptor to which readiness notifications are sent.
    ///
    /// Once the server is listening to connections on the specified socket, it will print
    /// `READY=1\n` into this file descriptor and close it.
    ///
    /// This readiness notification mechanism which works on both systemd and s6.
    ///
    /// This feature is only available on unix-like platforms.
    #[clap(long)]
    ready_fd: Option<i32>,

    /// Equivalent to `--config /usr/share/dbus-1/session.conf`.
    /// This is the default if `--config` and `--system` are unspecified.
    ///
    /// As the default, the file is optional: busd goes by its built-in defaults if it doesn't
    /// exist.
    #[clap(long)]
    session: bool,

    /// Equivalent to `--config /usr/share/dbus-1/system.conf`.
    #[clap(long)]
    system: bool,
}

fn main() -> Result<()> {
    // The next instance of busd, which it hands over to, starts with the same limit.
    #[cfg(target_os = "linux")]
    let fd_limit = getrlimit(Resource::Nofile).current;
    // Before the runtime, or anything else, opens file descriptors of its own.
    let raised = raise_fd_limit();
    // systemd passes file descriptors through environment variables, which busd removes as it
    // takes them: that may only happen before any other thread starts.
    #[cfg(target_os = "linux")]
    let passed = busd::systemd::take_listen_fds()?;

    zruntime::block_on(run(
        raised,
        #[cfg(target_os = "linux")]
        fd_limit,
        #[cfg(target_os = "linux")]
        passed,
    ))
}

async fn run(
    raised: rustix::io::Result<()>,
    #[cfg(target_os = "linux")] fd_limit: Option<u64>,
    #[cfg(target_os = "linux")] passed: Vec<(String, OwnedFd)>,
) -> Result<()> {
    busd::tracing_subscriber::init();
    if let Err(e) = raised {
        warn!("Failed to raise the limit of open file descriptors: {e}");
    }

    let args = Args::parse();
    let config = read_config(&args, Path::new(SESSION_CONFIG))?;

    let address = if let Some(address) = args.address {
        Some(address)
    } else {
        config.listen.as_ref().map(ToString::to_string)
    };

    // SIGTERM is what service managers, such as systemd, and `kill` send to stop a service. Taking
    // the signals over before the bus listens, and so before anyone can tell that it's ready, has
    // the bus clean up however soon one of them comes.
    let mut signals = Signals::new(
        &SharedRuntime::current()?,
        [Signal::Interrupt, Signal::Terminate],
    )?;

    #[cfg(target_os = "linux")]
    let notifier = busd::systemd::Notifier::from_env().unwrap_or_else(|e| {
        warn!("Ignoring `$NOTIFY_SOCKET`: {e}");

        None
    });
    // Under systemd, the bus takes over from its previous instance, or listens on the socket of the
    // socket activation, if systemd passes either.
    #[cfg(target_os = "linux")]
    let mut bus = bus::Bus::for_systemd(passed, notifier.as_ref(), address.as_deref()).await?;
    #[cfg(not(target_os = "linux"))]
    let mut bus = bus::Bus::for_address(address.as_deref()).await?;

    if let Some(fd) = args.ready_fd {
        // SAFETY: We don't have any way to know if the fd is valid or not. The parent process is
        // responsible for passing a valid fd.
        let mut ready_file = unsafe { File::from_raw_fd(fd) };
        ready_file.write_all(b"READY=1\n")?;
    }

    if args.print_address {
        println!("{}", bus.address());
    }

    // Under systemd, a service of `Type=notify` tells systemd once it's ready.
    #[cfg(target_os = "linux")]
    if let Some(notifier) = &notifier {
        if let Err(e) = notifier.notify("READY=1") {
            warn!("Failed to tell systemd that the bus is ready: {e}");
        }
    }

    {
        let signaled = pin!(signals.recv());
        let running = pin!(bus.run());
        match select(signaled, running).await {
            Either::Left((Ok(signal), _)) => {
                let name = match signal {
                    Signal::Terminate => "SIGTERM",
                    _ => "SIGINT",
                };
                info!("Received {}, shutting down..", name);
            }
            Either::Left((Err(e), _)) => error!("Failed to wait for signals: {}", e),
            Either::Right((Ok(()), _)) => warn!("Bus stopped, shutting down.."),
            Either::Right((Err(e), _)) => error!("Bus stopped with an error: {}", e),
        }
    }

    // With a file descriptor store of systemd's to keep the clients in, the bus hands them over to
    // its next instance rather than disconnect them. On a restart, systemd starts the next instance
    // with them, and on a stop, it closes them.
    #[cfg(target_os = "linux")]
    if let Some(notifier) = &notifier {
        return bus.hand_over_to_systemd(notifier, fd_limit).await;
    }

    if let Err(e) = bus.cleanup().await {
        error!("Failed to clean up: {}", e);
    }

    Ok(())
}

/// Raise the soft limit of the file descriptors that busd may have open to the hard limit, or to
/// `OPEN_MAX` at most on Apple's platforms, which take no more than that.
///
/// busd has a socket open for each of its clients, and on Linux a file descriptor for the process
/// of each. The soft limit that systemd and most shells set, 1024, or 256 on macOS, would hold that
/// to a few hundred clients, while the hard one, 524288 under systemd, holds plenty. The runtime of
/// busd waits on its file descriptors with epoll, kqueue or `poll`, or on Apple's platforms with a
/// `select` whose sets have no fixed size, so none of them is held to the 1024 that the sets of a
/// fixed size take.
fn raise_fd_limit() -> rustix::io::Result<()> {
    let mut limit = getrlimit(Resource::Nofile);
    // `None` is no limit.
    let Some(current) = limit.current else {
        return Ok(());
    };
    #[cfg(target_vendor = "apple")]
    let maximum = Some(
        limit
            .maximum
            .map_or(OPEN_MAX, |maximum| maximum.min(OPEN_MAX)),
    );
    #[cfg(not(target_vendor = "apple"))]
    let maximum = limit.maximum;
    if maximum.is_some_and(|maximum| maximum <= current) {
        return Ok(());
    }
    limit.current = maximum;

    setrlimit(Resource::Nofile, limit)
}

/// The configuration that `args` ask for.
///
/// The session configuration, at `session_config`, only has to exist if `--session` asks for it:
/// without `--config`, `--session` or `--system`, the bus goes by its built-in defaults if there's
/// no file there, as on a system without the reference implementation of D-Bus.
fn read_config(args: &Args, session_config: &Path) -> Result<Config> {
    let path = if args.system {
        Path::new(SYSTEM_CONFIG)
    } else if let Some(path) = &args.config {
        path.as_path()
    } else if args.session
        || session_config
            .try_exists()
            .with_context(|| format!("failed to look for {}", session_config.display()))?
    {
        session_config
    } else {
        info!(
            "configuration file {} doesn't exist, going by the defaults",
            session_config.display(),
        );

        return Ok(Config::default());
    };
    info!("reading configuration file {} ...", path.display());

    Config::read_file(path)
        .with_context(|| format!("failed to read configuration file {}", path.display()))
}

const SESSION_CONFIG: &str = "/usr/share/dbus-1/session.conf";
const SYSTEM_CONFIG: &str = "/usr/share/dbus-1/system.conf";
/// The most file descriptors that a process may have open on Apple's platforms, which refuse a soft
/// limit above that, as `setrlimit(2)` tells.
#[cfg(target_vendor = "apple")]
const OPEN_MAX: u64 = 10240;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use busd::config::Config;
    use clap::{CommandFactory, Parser};

    use super::{read_config, Args};

    #[test]
    fn missing_session_config_is_optional() {
        let args = Args::parse_from(["busd"]);
        let config = read_config(&args, Path::new(MISSING)).unwrap();

        assert_eq!(config, Config::default());
    }

    #[test]
    fn missing_session_config_asked_for() {
        let args = Args::parse_from(["busd", "--session"]);
        let error = read_config(&args, Path::new(MISSING)).unwrap_err();

        assert!(error.to_string().contains(MISSING), "{error:?}");
    }

    #[test]
    fn unreachable_session_config() {
        // Whether there's a file at this path can't be known, as its parent is a file.
        let args = Args::parse_from(["busd"]);
        let error = read_config(&args, Path::new(UNREACHABLE)).unwrap_err();

        assert!(error.to_string().contains(UNREACHABLE), "{error:?}");
    }

    #[test]
    fn missing_config() {
        // The session configuration exists, so that `--config` has to take precedence over it.
        let args = Args::parse_from(["busd", "--config", MISSING]);
        let error = read_config(&args, Path::new(SESSION)).unwrap_err();

        assert!(error.to_string().contains(MISSING), "{error:?}");
    }

    #[test]
    fn session_config() {
        let args = Args::parse_from(["busd"]);
        let config = read_config(&args, Path::new(SESSION)).unwrap();

        assert_eq!(config, Config::read_file(SESSION).unwrap());
        assert_ne!(config, Config::default());
    }

    #[test]
    fn usage() {
        let help = Args::command().render_help().to_string();

        assert!(help.contains("Usage: busd [OPTIONS]"), "{help}");
    }

    #[test]
    fn error_context() {
        let error = Args::try_parse_from(["busd", "--ready-fd", "x"]).unwrap_err();

        assert!(error.to_string().contains("--ready-fd"), "{error}");
    }

    const MISSING: &str = "/nonexistent/busd/session.conf";
    const UNREACHABLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml/session.conf");
    const SESSION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/session.conf");
}
