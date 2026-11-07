//! `feeling-catalog`: builds the one file that makes in-app browsing possible.
//! See the "Catalog" section of `docs/DESIGN.md`.

mod build;
mod categorize;
mod curated;
mod detect;
mod gate;
mod github;
mod http;
mod merge;
mod model;
mod rangezip;
mod state;
mod text;
mod tier;
mod wowi;
mod zipscan;

use std::path::PathBuf;
use std::process::ExitCode;

use build::Options;

const USAGE: &str =
    "Usage: feeling-catalog build --out <dir> --seeds <seeds.toml> --curated <curated.toml>
         [--previous <catalog.json>] [--state <catalog-state.json>]
         [--limit N] [--wowi-limit N] [--no-wowi] [--max-minutes N]

Reads GITHUB_TOKEN from the environment. Writes catalog.json and catalog-state.json to --out.
--max-minutes stops starting new downloads after that long, and still writes everything.";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    match args.next().as_deref() {
        Some("build") => {}
        Some(other) => return Err(format!("Unknown command `{other}`.\n{USAGE}")),
        None => return Err(USAGE.to_string()),
    }
    let mut options = Options::default();
    while let Some(flag) = args.next() {
        if flag == "--no-wowi" {
            options.no_wowi = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value.\n{USAGE}"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{flag} needs a whole number, not `{value}`."))
        };
        match flag.as_str() {
            "--out" => options.out = PathBuf::from(&value),
            "--seeds" => options.seeds = PathBuf::from(&value),
            "--curated" => options.curated = PathBuf::from(&value),
            "--previous" => options.previous = Some(PathBuf::from(&value)),
            "--state" => options.state = Some(PathBuf::from(&value)),
            "--limit" => options.limit = Some(usize::try_from(number()?).unwrap_or(usize::MAX)),
            "--wowi-limit" => {
                options.wowi_limit = Some(usize::try_from(number()?).unwrap_or(usize::MAX))
            }
            "--max-minutes" => options.max_minutes = Some(number()?),
            other => return Err(format!("Unknown option `{other}`.\n{USAGE}")),
        }
    }
    if options.out.as_os_str().is_empty()
        || options.seeds.as_os_str().is_empty()
        || options.curated.as_os_str().is_empty()
    {
        return Err(format!(
            "--out, --seeds and --curated are required.\n{USAGE}"
        ));
    }
    Ok(options)
}

fn main() -> ExitCode {
    let outcome = parse_args(std::env::args().skip(1)).and_then(|options| build::run(&options));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    const REQUIRED: [&str; 7] = ["build", "--out", "o", "--seeds", "s", "--curated", "c"];

    #[test]
    fn required_arguments_are_enough() {
        let options = parse(&REQUIRED).unwrap();
        assert_eq!(options.out, PathBuf::from("o"));
        assert_eq!(options.seeds, PathBuf::from("s"));
        assert_eq!(options.curated, PathBuf::from("c"));
        assert_eq!(options.limit, None);
        assert!(!options.no_wowi);
    }

    #[test]
    fn optional_arguments_are_read() {
        let mut args = REQUIRED.to_vec();
        args.extend([
            "--previous",
            "p",
            "--state",
            "st",
            "--limit",
            "5",
            "--wowi-limit",
            "25",
            "--no-wowi",
            "--max-minutes",
            "270",
        ]);
        let options = parse(&args).unwrap();
        assert_eq!(options.previous, Some(PathBuf::from("p")));
        assert_eq!(options.state, Some(PathBuf::from("st")));
        assert_eq!(options.limit, Some(5));
        assert_eq!(options.wowi_limit, Some(25));
        assert!(options.no_wowi);
        assert_eq!(options.max_minutes, Some(270));
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(parse(&[]).unwrap_err().contains("Usage"));
        assert!(parse(&["brew"]).unwrap_err().contains("Unknown command"));
        assert!(parse(&["build"]).unwrap_err().contains("required"));
        assert!(
            parse(&["build", "--out"])
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(
            parse(&["build", "--what", "x"])
                .unwrap_err()
                .contains("Unknown option")
        );
        let mut args = REQUIRED.to_vec();
        args.extend(["--limit", "many"]);
        assert!(parse(&args).unwrap_err().contains("whole number"));
    }
}
