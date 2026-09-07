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
        FmtSubscriber::builder().finish().with(targets).init();
    }

    #[cfg(feature = "console-subscriber")]
    console_subscriber::init();
}
