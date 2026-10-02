//! A source artifact for Cargo, rather than a bytecode section in a prebuilt host.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};

#[derive(Serialize)]
struct Manifest {
    package: Package,
    workspace: toml::Table,
    dependencies: BTreeMap<&'static str, Dependency>,
    profile: BTreeMap<&'static str, Release>,
}
#[derive(Serialize)]
struct Package {
    name: &'static str,
    version: &'static str,
    edition: &'static str,
    publish: bool,
}
#[derive(Serialize)]
struct Dependency {
    path: String,
}
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct Release {
    lto: &'static str,
    codegen_units: u32,
    opt_level: u32,
}

pub(crate) fn write(
    directory: &Path,
    runtime: &Path,
    envelope: &[u8],
    module: &baml_compiler2_rust::NativeModule,
) -> Result<()> {
    let runtime = runtime
        .canonicalize()
        .context("cannot locate runtime source workspace")?;
    for required in [
        "Cargo.lock",
        "rust-toolchain.toml",
        "crates/bex_vm_types/src/compiled.rs",
        "crates/baml_pack_host/src/lib.rs",
    ] {
        if !runtime.join(required).is_file() {
            bail!("runtime source is missing {required}; use a matching BAML checkout");
        }
    }
    let mut dependencies = BTreeMap::new();
    for name in ["baml_artifact", "baml_pack_host", "bex_vm_types"] {
        let path = runtime.join("crates").join(name);
        if !path.join("Cargo.toml").is_file() {
            bail!("runtime source is missing crate {name}");
        }
        dependencies.insert(
            name,
            Dependency {
                path: path
                    .to_str()
                    .context("runtime source path is not UTF-8")?
                    .to_owned(),
            },
        );
    }
    let manifest = toml::to_string_pretty(&Manifest {
        package: Package {
            name: "baml-app",
            version: "0.0.0",
            edition: "2024",
            publish: false,
        },
        workspace: toml::Table::new(),
        dependencies,
        profile: BTreeMap::from([(
            "release",
            Release {
                lto: "fat",
                codegen_units: 1,
                opt_level: 3,
            },
        )]),
    })?;
    let mut report = format!(
        "{} functions compiled to Rust; {} functions retain bytecode.\n\n",
        module.compiled.len(),
        module.fallback.len()
    );
    for fallback in &module.fallback {
        use std::fmt::Write as _;
        let _ = writeln!(report, "{}: {}", fallback.function, fallback.reason);
    }
    report.push_str("\nNative-to-native call eligibility (entry from the VM remains resumable):\n");
    for direct in &module.direct_calls {
        use std::fmt::Write as _;
        let mode = if direct.eligible {
            "direct"
        } else {
            "resumable"
        };
        let _ = writeln!(report, "{}: {mode}: {}", direct.function, direct.reason);
    }
    // Creating a new directory refuses every existing path before any writes.
    std::fs::create_dir(directory)
        .with_context(|| format!("cannot create new Rust project {}", directory.display()))?;
    std::fs::create_dir(directory.join("src"))?;
    std::fs::write(directory.join("Cargo.toml"), manifest)?;
    std::fs::write(directory.join("program.bamlpack"), envelope)?;
    std::fs::write(directory.join("src/generated.rs"), &module.source)?;
    std::fs::write(directory.join("src/main.rs"), MAIN)?;
    std::fs::write(directory.join("native-support.txt"), report)?;
    std::fs::write(directory.join("README.md"), README)?;
    std::fs::copy(runtime.join("Cargo.lock"), directory.join("Cargo.lock"))?;
    std::fs::copy(
        runtime.join("rust-toolchain.toml"),
        directory.join("rust-toolchain.toml"),
    )?;
    let config = runtime.join(".cargo/config.toml");
    if config.is_file() {
        std::fs::create_dir(directory.join(".cargo"))?;
        std::fs::copy(config, directory.join(".cargo/config.toml"))?;
    }
    Ok(())
}

const MAIN: &str = r#"mod generated;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut envelope: baml_pack_host::PackEnvelope = match baml_artifact::decode(
        baml_artifact::ArtifactKind::PackedProgram, include_bytes!("../program.bamlpack"),
    ) {
        Ok(envelope) => envelope,
        Err(error) => { eprintln!("invalid embedded program: {error}"); return ExitCode::FAILURE; }
    };
    if let Err(error) = generated::install(&mut envelope.program) {
        eprintln!("invalid compiled implementation: {error}");
        return ExitCode::FAILURE;
    }
    baml_pack_host::run_envelope(envelope)
}
"#;
const README: &str = "# Generated BAML executable\n\nRun `cargo build --release` in this directory. The executable is `target/release/baml-app` (with `.exe` on Windows). It uses the normal packed BAML command-line interface.\n\n`native-support.txt` lists the compiled coverage and bytecode fallbacks. This artifact currently links the full BEX runtime, including runtime compilation; it makes no size or speed guarantee.\n\nCargo.toml refers to the explicitly selected runtime source checkout. Keep that checkout at the compiler revision used for generation. Cargo.lock and the Rust toolchain are copied for reproducible dependency selection. This project supports the native targets supported by the pack host; it is not a Wasm pack.\n";

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_directory_is_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("out");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("Cargo.toml"), "keep").unwrap();
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let module = baml_compiler2_rust::NativeModule {
            source: String::new(),
            compiled: vec![],
            fallback: vec![],
        };
        assert!(write(&directory, &runtime, &[], &module).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join("Cargo.toml")).unwrap(),
            "keep"
        );
    }
}
