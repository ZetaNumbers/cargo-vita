use core::panic;
use std::{
    env,
    io::{self, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use cargo_metadata::{Artifact, Message, Package};
use clap::{Args, Subcommand};
use colored::Colorize;
use either::Either;
use tee::TeeReader;
use walkdir::WalkDir;

use crate::meta::{parse_crate_metadata, PackageMetadata, TitleId, VITA_TARGET};

use super::Executor;

#[derive(Args, Debug)]
pub struct Build {
    #[command(subcommand)]
    cmd: BuildCmd,

    #[arg(trailing_var_arg = true)]
    #[arg(allow_hyphen_values = true)]
    #[arg(global = true)]
    #[arg(name = "CARGO_ARGS")]
    args: Vec<String>,
}
#[derive(Subcommand, Debug)]
#[command(allow_external_subcommands = true)]
enum BuildCmd {
    Elf,
    Velf,
    Eboot,
    Sfo(Sfo),
    Vpk(Vpk),
}

#[derive(Args, Debug)]
struct Sfo {
    /// An alphanumeric string of 9 characters. Used as a fallback in case title_id is not defined in Cargo.toml.
    #[arg(long, value_parser = clap::value_parser!(TitleId))]
    default_title_id: Option<TitleId>,
}

#[derive(Args, Debug)]
struct Vpk {
    #[command(flatten)]
    sfo: Sfo,
}

impl Executor for Build {
    fn execute(&self, verbose: u8) {
        let (meta, _) = parse_crate_metadata(None);
        let sdk = std::env::var("VITASDK");
        let sdk = meta
            .vita_sdk
            .as_deref()
            .or_else(|| sdk.as_deref().ok())
            .unwrap_or_else(|| {
                panic!(
                    "VITASDK environment variable isn't set. Please install the SDK \
                    from https://vitasdk.org/ and set the VITASDK environment variable."
                )
            });

        match &self.cmd {
            BuildCmd::Elf => {
                build_elf(&meta, sdk, &self.args, verbose);
            }
            BuildCmd::Velf => {
                for artifact in build_elf(&meta, sdk, &self.args, verbose) {
                    let (meta, _) = parse_crate_metadata(Some(&artifact));

                    strip(&artifact, sdk, &meta, verbose);
                    velf(&artifact, sdk, &meta, verbose);
                }
            }
            BuildCmd::Eboot => {
                for artifact in build_elf(&meta, sdk, &self.args, verbose) {
                    let (meta, _) = parse_crate_metadata(Some(&artifact));

                    strip(&artifact, sdk, &meta, verbose);
                    velf(&artifact, sdk, &meta, verbose);
                    eboot(&artifact, sdk, &meta, verbose);
                }
            }
            BuildCmd::Sfo(args) => {
                for artifact in build_elf(&meta, sdk, &self.args, verbose) {
                    let (meta, pkg) = parse_crate_metadata(Some(&artifact));
                    let pkg = pkg.expect("artifact does not have a package");

                    sfo(&args, &artifact, sdk, &meta, &pkg, verbose);
                }
            }
            BuildCmd::Vpk(args) => {
                for artifact in build_elf(&meta, sdk, &self.args, verbose) {
                    let (meta, pkg) = parse_crate_metadata(Some(&artifact));
                    let pkg = pkg.expect("artifact does not have a package");

                    strip(&artifact, sdk, &meta, verbose);
                    velf(&artifact, sdk, &meta, verbose);
                    eboot(&artifact, sdk, &meta, verbose);
                    sfo(&args.sfo, &artifact, sdk, &meta, &pkg, verbose);
                    vpk(&artifact, sdk, &meta, verbose);
                }
            }
        };
    }
}

fn build_elf(meta: &PackageMetadata, sdk: &str, args: &[String], verbose: u8) -> Vec<Artifact> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let rust_flags = env::var("RUSTFLAGS").unwrap_or_default()
        + " --cfg mio_unsupported_force_poll_poll --cfg mio_unsupported_force_waker_pipe";

    let mut command = Command::new(cargo);

    if let Ok(path) = env::var("PATH") {
        let sdk_path = Path::new(sdk).join("bin");
        let path = format!("{}:{path}", sdk_path.display());
        command.env("PATH", path);
    }

    command
        .env("RUSTFLAGS", rust_flags)
        .env("TARGET_CC", "arm-vita-eabi-gcc")
        .env("TARGET_CXX", "arm-vita-eabi-g++")
        .env("VITASDK", sdk)
        .arg("build")
        .arg("-Z")
        .arg(format!("build-std={}", meta.build_std))
        .arg("--target")
        .arg(VITA_TARGET)
        .arg("--message-format")
        .arg("json-render-diagnostics")
        .args(args)
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Running cargo:".blue());
    }

    let mut process = command.spawn().unwrap();
    let command_stdout = process.stdout.take().unwrap();

    let reader = if verbose > 1 {
        Either::Left(BufReader::new(TeeReader::new(command_stdout, io::stdout())))
    } else {
        Either::Right(BufReader::new(command_stdout))
    };

    let messages: Vec<Message> = Message::parse_stream(reader)
        .collect::<io::Result<_>>()
        .unwrap();

    messages
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::CompilerArtifact(art) if art.executable.is_some() => Some(art.clone()),
            _ => None,
        })
        .collect()
}

fn strip(artifact: &Artifact, sdk: &str, meta: &PackageMetadata, verbose: u8) {
    let sdk = Path::new(sdk);
    let mut command = Command::new(sdk.join("bin").join("arm-vita-eabi-strip").as_os_str());

    command
        .args(&meta.vita_strip_flags)
        .arg(
            artifact
                .executable
                .as_deref()
                .expect("Artifact has no executables"),
        )
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Stripping elf:".blue());
    }

    command.status().expect("Artifact has no executables");
}

fn velf(artifact: &Artifact, sdk: &str, _meta: &PackageMetadata, verbose: u8) {
    let sdk = Path::new(sdk);
    let mut command = Command::new(sdk.join("bin").join("vita-elf-create").as_os_str());
    let elf = artifact
        .executable
        .as_deref()
        .expect("Artifact has no executables");

    let mut velf = PathBuf::from(&elf);
    velf.set_extension("velf");

    command
        .arg(elf)
        .arg(&velf)
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Creating velf:".blue());
    }

    command.status().expect("vita-elf-create failed");
}

fn eboot(artifact: &Artifact, sdk: &str, meta: &PackageMetadata, verbose: u8) {
    let sdk = Path::new(sdk);
    let mut command = Command::new(sdk.join("bin").join("vita-make-fself").as_os_str());
    let elf = artifact
        .executable
        .as_deref()
        .expect("Artifact has no executables");

    let mut velf = PathBuf::from(&elf);
    velf.set_extension("velf");

    let mut eboot = PathBuf::from(&elf);
    eboot.set_extension("self");

    command
        .args(&meta.vita_make_fself_flags)
        .arg(&velf)
        .arg(&eboot)
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Creating eboot:".blue());
    }

    command.status().expect("vita-make-fself failed");
}

fn sfo(
    args: &Sfo,
    artifact: &Artifact,
    sdk: &str,
    meta: &PackageMetadata,
    pkg: &Package,
    verbose: u8,
) {
    let sdk = Path::new(sdk);
    let mut command = Command::new(sdk.join("bin").join("vita-mksfoex").as_os_str());
    let elf = artifact
        .executable
        .as_deref()
        .expect("Artifact has no executables");

    let mut sfo = PathBuf::from(&elf);
    sfo.set_extension("sfo");

    let title_name = meta.title_name.as_deref().unwrap_or_else(|| &pkg.name);

    let title_id = &meta
        .title_id
        .as_ref()
        .or(args.default_title_id.as_ref())
        .expect(&format!("title_id is not set for artifact {}", pkg.name))
        .0;

    command
        .args(&meta.vita_mksfoex_flags)
        .arg("-s")
        .arg(format!("TITLE_ID={title_id}"))
        .arg(title_name)
        .arg(sfo)
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Creating sfo:".blue());
    }

    command.status().expect("vita-mksfoex failed");
}

fn vpk(artifact: &Artifact, sdk: &str, meta: &PackageMetadata, verbose: u8) {
    let elf = artifact
        .executable
        .as_deref()
        .expect("Artifact has no executables");

    let mut eboot = PathBuf::from(&elf);
    eboot.set_extension("self");

    let mut vpk = PathBuf::from(&elf);
    vpk.set_extension("vpk");

    let mut sfo = PathBuf::from(&elf);
    sfo.set_extension("sfo");

    let sdk = Path::new(sdk);
    let mut command = Command::new(sdk.join("bin").join("vita-pack-vpk").as_os_str());
    command.arg("-s").arg(sfo);
    command.arg("-b").arg(eboot);

    if let Some(assets) = &meta.assets {
        let files = WalkDir::new(assets)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file());

        for file in files {
            command.arg("--add").arg(format!(
                "{}={}",
                file.path().display(), // path on FS
                file.path().strip_prefix(assets).unwrap().display()  // path in VPK
            ));
        }
    }

    command
        .arg(vpk)
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        println!("{} {command:?}", "Building vpk:".blue());
    }

    command.status().expect("vita-mksfoex failed");
}
