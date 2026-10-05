pub fn init() {
    #[cfg(all(feature = "tracing-subscriber", not(feature = "console-subscriber")))]
    {
        use std::{env, str::FromStr};

        use tracing_subscriber::{
            filter::{LevelFilter, Targets},
            layer::SubscriberExt,
            util::SubscriberInitExt,
            FmtSubscriber,
        };

        // `RUST_LOG` holds comma-separated `target=level` directives, e.g. `busd=debug,zbus=info`,
        // with a bare `level` applying to every target. Targets left out are silent, and with the
        // variable unset only errors are shown.
        let targets = match env::var("RUST_LOG") {
            Ok(directives) => match Targets::from_str(&directives) {
                Ok(targets) => targets,
                Err(e) => {
                    eprintln!("Ignoring invalid `RUST_LOG` value `{directives}`: {e}");
                    Targets::new().with_default(LevelFilter::ERROR)
                }
            },
            Err(_) => Targets::new().with_default(LevelFilter::ERROR),
        };
        // The formatter's own level filter defaults to INFO, which would cap whatever `targets`
        // lets through, so open it fully and leave the filtering to `targets`.
        FmtSubscriber::builder()
            .with_max_level(LevelFilter::TRACE)
            .finish()
            .with(targets)
            .init();
    }

    #[cfg(feature = "console-subscriber")]
    console_subscriber::init();
}
