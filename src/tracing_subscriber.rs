pub fn init() {
    #[cfg(feature = "tracing-subscriber")]
    {
        use tracing_subscriber::{
            filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, FmtSubscriber,
        };

        let targets = targets(std::env::var("RUST_LOG").ok().as_deref());
        // The formatter's own level filter defaults to INFO, which would cap whatever `targets`
        // lets through, so open it fully and leave the filtering to `targets`. The logs go to
        // stderr, so that stdout carries only what `--print-address` prints.
        FmtSubscriber::builder()
            .with_max_level(LevelFilter::TRACE)
            .with_writer(std::io::stderr)
            .finish()
            .with(targets)
            .init();
    }
}

/// The levels to log each target at, as `directives`, the value of `$RUST_LOG`, set them.
///
/// The directives are comma-separated `target=level` ones, e.g. `busd=debug,zbus=trace`, and a bare
/// `level` for the targets that no directive names. Those log their info, warnings and errors,
/// unless the directives say otherwise, as all of them do without directives: only debugging and
/// tracing have to be asked for. An empty value is no directives, and an invalid one is reported on
/// stderr, and ignored.
#[cfg(feature = "tracing-subscriber")]
fn targets(directives: Option<&str>) -> tracing_subscriber::filter::Targets {
    use std::str::FromStr;

    use tracing_subscriber::filter::{LevelFilter, Targets};

    // `Targets` takes an empty directive for one that names every target, with every level.
    let targets = match directives.filter(|directives| !directives.is_empty()) {
        Some(directives) => Targets::from_str(directives).unwrap_or_else(|e| {
            eprintln!("Ignoring invalid `RUST_LOG` value `{directives}`: {e}");

            Targets::new()
        }),
        None => Targets::new(),
    };

    match targets.default_level() {
        Some(_) => targets,
        None => targets.with_default(LevelFilter::INFO),
    }
}

#[cfg(all(test, feature = "tracing-subscriber"))]
mod tests {
    use tracing::Level;

    use super::targets;

    /// Without directives, or with invalid ones, every target gets its info, warnings and errors
    /// logged, but not its debugging or tracing.
    #[test]
    fn info_by_default() {
        for directives in [None, Some(""), Some("busd=loud"), Some("a=b=c")] {
            let logged = targets(directives);
            for target in ["busd", "busd::bus", "zbus"] {
                for level in [Level::ERROR, Level::WARN, Level::INFO] {
                    assert!(logged.would_enable(target, &level), "{directives:?}");
                }
                for level in [Level::DEBUG, Level::TRACE] {
                    assert!(!logged.would_enable(target, &level), "{directives:?}");
                }
            }
        }
    }

    /// Directives for some targets leave the others at info, unless a bare level says otherwise.
    #[test]
    fn directives() {
        let logged = targets(Some("busd=debug,zbus=off"));
        assert!(logged.would_enable("busd::bus", &Level::DEBUG));
        assert!(!logged.would_enable("busd", &Level::TRACE));
        assert!(!logged.would_enable("zbus", &Level::ERROR));
        assert!(logged.would_enable("zruntime", &Level::INFO));
        assert!(!logged.would_enable("zruntime", &Level::DEBUG));

        let logged = targets(Some("warn,busd=trace"));
        assert!(logged.would_enable("busd", &Level::TRACE));
        assert!(logged.would_enable("zbus", &Level::WARN));
        assert!(!logged.would_enable("zbus", &Level::INFO));
    }
}
