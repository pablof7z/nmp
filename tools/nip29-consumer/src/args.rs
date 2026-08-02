use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Online,
    LiveAdversarial,
    ProvenanceGrowth,
    Restart,
    RestartConflict,
}

#[derive(Debug)]
pub struct Args {
    pub mode: Mode,
    pub relay_a: String,
    pub relay_b: String,
    pub viewer: String,
    pub followed: String,
    pub outsider: String,
    pub writer_secret_file: PathBuf,
    pub store_path: PathBuf,
    pub ready_file: Option<PathBuf>,
    pub stage_dir: Option<PathBuf>,
    pub settle_secs: u64,
}

impl Args {
    pub fn parse_or_exit() -> Self {
        match Self::parse(std::env::args().skip(1)) {
            Ok(args) => args,
            Err(error) => {
                eprintln!("{error}\n");
                usage();
                std::process::exit(2);
            }
        }
    }

    fn parse(argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut values = argv;
        let mode = match values.next().as_deref() {
            Some("online") => Mode::Online,
            Some("live-adversarial") => Mode::LiveAdversarial,
            Some("provenance-growth") => Mode::ProvenanceGrowth,
            Some("restart") => Mode::Restart,
            Some("restart-conflict") => Mode::RestartConflict,
            Some("--help" | "-h" | "help") => {
                usage();
                std::process::exit(0);
            }
            Some(other) => return Err(format!("unknown mode {other:?}")),
            None => return Err("missing mode".to_string()),
        };

        let mut relay_a = None;
        let mut relay_b = None;
        let mut viewer = None;
        let mut followed = None;
        let mut outsider = None;
        let mut writer_secret_file = None;
        let mut store_path = None;
        let mut ready_file = None;
        let mut stage_dir = None;
        let mut settle_secs = 20;
        while let Some(flag) = values.next() {
            let value = values
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))?;
            match flag.as_str() {
                "--relay-a" => relay_a = Some(value),
                "--relay-b" => relay_b = Some(value),
                "--viewer" => viewer = Some(value),
                "--followed" => followed = Some(value),
                "--outsider" => outsider = Some(value),
                "--writer-secret-file" => writer_secret_file = Some(value.into()),
                "--store" => store_path = Some(value.into()),
                "--ready-file" => ready_file = Some(value.into()),
                "--stage-dir" => stage_dir = Some(value.into()),
                "--settle-secs" => {
                    settle_secs = value
                        .parse::<u64>()
                        .map_err(|_| "--settle-secs must be an integer".to_string())?;
                }
                other => return Err(format!("unknown option {other:?}")),
            }
        }

        Ok(Self {
            mode,
            relay_a: required("--relay-a", relay_a)?,
            relay_b: required("--relay-b", relay_b)?,
            viewer: required("--viewer", viewer)?,
            followed: required("--followed", followed)?,
            outsider: required("--outsider", outsider)?,
            writer_secret_file: required("--writer-secret-file", writer_secret_file)?,
            store_path: required("--store", store_path)?,
            ready_file,
            stage_dir,
            settle_secs,
        })
    }
}

fn required<T>(name: &str, value: Option<T>) -> Result<T, String> {
    value.ok_or_else(|| format!("missing {name}"))
}

fn usage() {
    eprintln!(
        "usage: nmp-nip29-consumer <online|live-adversarial|provenance-growth|restart|restart-conflict>"
    );
    eprintln!("  --relay-a <ws-url> --relay-b <ws-url>");
    eprintln!("  --viewer <hex> --followed <hex> --outsider <hex>");
    eprintln!("  --writer-secret-file <path> --store <path>");
    eprintln!("  [--ready-file <path>] [--stage-dir <path>] [--settle-secs <seconds>]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_complete_application_boundary() {
        let args = Args::parse(
            [
                "online",
                "--relay-a",
                "ws://127.0.0.1:1",
                "--relay-b",
                "ws://127.0.0.1:2",
                "--viewer",
                "aa",
                "--followed",
                "bb",
                "--outsider",
                "cc",
                "--writer-secret-file",
                "/tmp/key",
                "--store",
                "/tmp/store",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("complete arguments parse");
        assert_eq!(args.mode, Mode::Online);
        assert_eq!(args.settle_secs, 20);
    }
}
