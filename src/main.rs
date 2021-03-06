extern crate cargo;
extern crate chrono;
extern crate curl;
extern crate flate2;
extern crate rustc_version;
extern crate serde_json;
extern crate tar;
extern crate tempdir;
extern crate term;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::hash::{Hash, SipHasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, mem};

use cargo::util::{self, CargoResult, ChainError, Config, Filesystem};
use cargo::core::shell::{ColorConfig, Verbosity};
use serde_json::{de, Value};

mod sysroot;

type Spec = HashMap<String, Value>;

fn main() {
    let config_opt = &mut None;
    if let Err(e) = run(config_opt) {
        let e = e.into();

        if let Some(config) = config_opt.as_ref() {
            cargo::handle_error(e, &mut config.shell())
        } else {
            cargo::handle_error(e, &mut cargo::shell(Verbosity::Verbose, ColorConfig::Auto));
        }
    }
}

fn run(config_opt: &mut Option<Config>) -> CargoResult<()> {
    *config_opt = Some(try!(Config::default()));
    let config = config_opt.as_ref().unwrap();
    let root = &try!(env::home_dir()
        .map(|p| Filesystem::new(p.join(".xargo")))
        .chain_error(|| {
            util::human("Xargo couldn't find your home directory. This probably means that $HOME \
                         was not set")
        }));

    let (mut cargo, target, verbose) = try!(parse_args());

    let rustflags = &try!(rustflags(config));
    let mut with_sysroot = false;
    if let Some(target) = target {
        try!(sysroot::create(config, &target, root, verbose, rustflags));
        with_sysroot = true;
    } else if let Some(triple) = try!(config.get_string("build.target")) {
        if let Some(target) = try!(Target::from(&triple.val)) {
            try!(sysroot::create(config, &target, root, verbose, rustflags));
            with_sysroot = true;
        }
    }

    let lock = if with_sysroot {
        let lock = try!(root.open_ro("date", config, "xargo"));

        {
            let sysroot = lock.parent().display();

            if rustflags.is_empty() {
                cargo.env("RUSTFLAGS", format!("--sysroot={}", sysroot));
            } else {
                cargo.env("RUSTFLAGS",
                          format!("{} --sysroot={}", rustflags.join(" "), sysroot));
            }
        }

        Some(lock)
    } else {
        None
    };

    if !try!(cargo.status()).success() {
        return Err(util::human("`cargo` process didn't exit successfully"));
    }
    // Forbid modifications of the `sysroot` during the execution of the `cargo` command
    mem::drop(lock);

    Ok(())
}

/// Custom target with specification file
pub struct Target {
    // Hasher that has already digested the contents of $triple.json
    hasher: SipHasher,
    // Path to $triple.json file
    path: PathBuf,
    spec: Spec,
    triple: String,
}

impl Target {
    fn from(arg: &str) -> CargoResult<Option<Self>> {
        let json = &PathBuf::from(format!("{}.json", arg));

        if json.is_file() {
            return Ok(Some(try!(Target::from_path(json))));
        }

        let target_path = &env::var_os("RUST_TARGET_PATH").unwrap_or(OsString::new());

        for dir in env::split_paths(target_path) {
            let path = &dir.join(json);

            if path.is_file() {
                return Ok(Some(try!(Target::from_path(path))));
            }
        }

        Ok(None)
    }

    fn from_path(path: &Path) -> CargoResult<Self> {
        fn hash(path: &Path) -> CargoResult<SipHasher> {
            let mut h = SipHasher::new();
            let contents = &mut String::new();
            try!(try!(File::open(path)).read_to_string(contents));
            contents.hash(&mut h);
            Ok(h)
        }

        let triple = path.file_stem().unwrap().to_string_lossy().into_owned();

        Ok(Target {
            hasher: try!(hash(path)),
            path: try!(fs::canonicalize(path)),
            spec: try!(parse_json(path)),
            triple: triple,
        })
    }
}

fn parse_json(p: &Path) -> CargoResult<Spec> {
    let json = &mut String::new();
    try!(try!(File::open(p)).read_to_string(json));
    de::from_str(json).map_err(|_| util::human("Target specification file is not valid JSON"))
}

fn parse_args() -> CargoResult<(Command, Option<Target>, bool)> {
    let mut cargo = Command::new("cargo");
    let mut target = None;
    let mut verbose = false;

    let mut next_is_target = false;
    for arg_os in env::args_os().skip(1) {
        for arg in arg_os.to_string_lossy().split(' ') {
            if target.is_none() {
                if next_is_target {
                    target = try!(Target::from(arg));
                } else {
                    if arg == "--target" {
                        next_is_target = true;
                    } else if arg.starts_with("--target=") {
                        if let Some(triple) = arg.split('=').skip(1).next() {
                            target = try!(Target::from(triple));
                        }
                    }
                }
            }

            if arg == "-v" || arg == "--verbose" {
                verbose = true;
            }
        }

        cargo.arg(arg_os);
    }

    Ok((cargo, target, verbose))
}

/// Returns the RUSTFLAGS the user has set either via the env variable or via build.rustflags
// NOTE Logic copied from cargo's Context::rustflags_args
fn rustflags(config: &Config) -> CargoResult<Vec<String>> {
    // First try RUSTFLAGS from the environment
    if let Some(a) = env::var("RUSTFLAGS").ok() {
        let args = a.split(" ").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        return Ok(args.collect());
    }

    // Then the build.rustflags value
    if let Some(args) = try!(config.get_list("build.rustflags")) {
        let args = args.val.into_iter().map(|a| a.0);
        return Ok(args.collect());
    }

    Ok(Vec::new())
}
