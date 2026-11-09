//! Explicit structural observation, never an execution or admission owner.
use reverie_liteinst_artifact::{FileIdentity, Runner};
use reverie_liteinst_artifact::private_static::{StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::{OptionalZeroObservation, SelectedInitializerDefinitions};
use reverie_liteinst_artifact::private_machine_diagnostic::NativeMemcpyInputs;
use serde_json::{json, Value};

pub fn observe(source:&VerifiedStaticSource, archive:&StaticArchive,
    elf:&FileIdentity, map:&FileIdentity, selected:&SelectedInitializerDefinitions, optional:&OptionalZeroObservation,
    native:&NativeMemcpyInputs<'_>, objdump:&FileIdentity, runner:&mut Runner,
)->Result<Value,String> {
    // Both calls inspect the same actual file and close the same live owner.
    // No name list or JSON can substitute for the private optional observation.
    let strict = source.inspect_final(archive, &elf.path, runner);
    let explicit = source.inspect_final_with_selected_and_optional_zero(archive, &elf.path, selected, optional, runner);
    let combined = source.inspect_entry_transfers_with_ordinary_and_musl_memcpy(
        archive, elf, map, native, objdump, runner);
    // Evaluate all independent closes even if a body refused.
    let closes = [archive.verify(), selected.verify(), optional.verify(), elf.verify(), map.verify(), objdump.verify()];
    let close_errors:Vec<_> = closes.into_iter().filter_map(|r|r.err()).collect();
    let default_error = match strict { Err(error)=>error, Ok(_)=>"UNEXPECTED_STRICT_SUCCESS".into() };
    let (inspection, structural_error) = match explicit {
        Ok(value) => (json!({"artifact":value.artifact(), "archive":value.archive(),
            "entry":value.entry(),"gate_sha256":value.gate_sha256(),
            "program_headers":value.program_headers(),"tls":value.tls(),
            "target_layout":{"control_bytes":value.layout().control_bytes(),
                "control_alignment":value.layout().control_alignment(),
                "relay_bytes":value.layout().relay_bytes(),"relay_alignment":value.layout().relay_alignment(),
                "header_bytes":value.layout().header_bytes()}}), None),
        Err(error)=>(Value::Null,Some(error)),
    };
    let (diagnostic,combined_error) = match combined {
        Ok(value)=>(serde_json::to_value(value).map_err(|e|e.to_string())?,None),
        Err(error)=>(Value::Null,Some(error)),
    };
    Ok(json!({"default_strict":{"state":"FAIL_RETAINED","error":default_error},
        "selected_binding_api":"SAME_SOURCE_SELECTED_AND_OPTIONAL_ZERO",
        "explicit_structural":inspection,"structural_error":structural_error,
        "combined_entry_transfer_diagnostic":diagnostic,"combined_error":combined_error,
        "close_errors":close_errors,"artifact":elf,"runtime_archive":archive.file(),"link_map":map,
        "optional_selected_uses":optional.selected_use_count(),
        "optional_unselected_input_uses":optional.unselected_input_use_count(),
        "qualification":"NOT_PROVEN","pre_fs_closure":"NOT_PROVEN","readiness":"NOT_PROVEN",
        "guests_executed":0,"guest_release":false}))
}

pub fn require_complete(value:&Value)->Result<(),String> {
    if value["selected_binding_api"] != "SAME_SOURCE_SELECTED_AND_OPTIONAL_ZERO" ||
        value["default_strict"]["error"] != "static final ELF has unresolved symbols" ||
        !value["structural_error"].is_null() || value["explicit_structural"].is_null() ||
        !value["combined_error"].is_null() || value["combined_entry_transfer_diagnostic"].is_null() ||
        value["close_errors"].as_array().is_none_or(|errors|!errors.is_empty()) {
        return Err(format!("explicit structural/combined observation refused: {value}"));
    }
    Ok(())
}

// Reconstructed file records are custody checks only; they cannot construct any
// source, archive, optional, provider or inspection authority.
pub fn retain_diagnostic_files(value:&Value, files:&mut Vec<FileIdentity>)->Result<(),String> {
    let diagnostic=&value["combined_entry_transfer_diagnostic"];
    if !diagnostic.is_null() {
        for key in ["artifact","objdump_tool","disassembly"] {
            let file:FileIdentity=serde_json::from_value(diagnostic[key].clone()).map_err(|e|e.to_string())?;
            file.verify()?;files.push(file);
        }
        let native=&diagnostic["native_memcpy"];
        if !native.is_null() {
            for key in ["archive","source","source_inventory","link_map"] {
                let file:FileIdentity=serde_json::from_value(native[key].clone()).map_err(|e|e.to_string())?;
                file.verify()?;files.push(file);
            }
        }
    }
    Ok(())
}
