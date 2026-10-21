extern crate busd;

use std::{fs::File, io::Write, os::fd::FromRawFd, path::PathBuf, pin::pin};

use busd::{bus, config::Config, error, info, warn};

use anyhow::Result;
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

    let config_path = if args.system {
        PathBuf::from("/usr/share/dbus-1/system.conf")
    } else if let Some(config_path) = args.config {
        config_path
    } else {
        PathBuf::from("/usr/share/dbus-1/session.conf")
    };
    info!("reading configuration file {} ...", config_path.display());
    let config = Config::read_file(&config_path)?;

    let address = if let Some(address) = args.address {
        Some(address)
    } else {
        config.listen.as_ref().map(ToString::to_string)
    };

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

    let mut signals = Signals::new(&SharedRuntime::current()?, [Signal::Interrupt])?;

    {
        let interrupted = pin!(signals.recv());
        let running = pin!(bus.run());
        match select(interrupted, running).await {
            Either::Left((Ok(_), _)) => info!("Received SIGINT, shutting down.."),
            Either::Left((Err(e), _)) => error!("Failed to wait for SIGINT: {}", e),
            Either::Right((Ok(()), _)) => warn!("Bus stopped, shutting down.."),
            Either::Right((Err(e), _)) => error!("Bus stopped with an error: {}", e),
        }
    }

    if let Err(e) = bus.cleanup().await {
        error!("Failed to clean up: {}", e);
    }

    Ok(())
}
