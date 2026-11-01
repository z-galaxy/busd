extern crate busd;

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
    zruntime::block_on(run())
}

async fn run() -> Result<()> {
    busd::tracing_subscriber::init();

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

    if let Err(e) = bus.cleanup().await {
        error!("Failed to clean up: {}", e);
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use busd::config::Config;
    use clap::Parser;

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

    const MISSING: &str = "/nonexistent/busd/session.conf";
    const UNREACHABLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml/session.conf");
    const SESSION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/session.conf");
}
