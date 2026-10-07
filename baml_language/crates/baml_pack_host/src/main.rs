//! Entry point for prebuilt pack hosts with an OS-native embedded section.
use std::process::ExitCode;

fn main() -> ExitCode {
    let envelope = (|| {
        let section = libsui::find_section(baml_exec::PACK_SECTION_NAME)
            .map_err(|e| format!("Failed to read embedded section: {e}"))?
            .ok_or(
                "No embedded BAML package found. This binary must be built with `baml pack`."
                    .to_owned(),
            )?;
        baml_artifact::decode(baml_artifact::ArtifactKind::PackedProgram, section)
            .map_err(|e| format!("Failed to deserialize pack envelope: {e}"))
    })();
    match envelope {
        Ok(envelope) => baml_pack_host::run_envelope(envelope),
        Err(error) => {
            baml_exec::print_error(error);
            ExitCode::FAILURE
        }
    }
}
