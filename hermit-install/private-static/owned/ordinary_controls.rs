//! Same-owner ordinary44 identity/transfer observations; no syscall authority.
use std::path::Path;
use reverie_liteinst_artifact::{FileIdentity, Runner, sha256, write_new};
use reverie_liteinst_artifact::private_static::{StaticArchive, VerifiedStaticSource};
use serde_json::{Value, json};
fn need(ok:bool,message:&str)->Result<(),String> {if ok {Ok(())} else {Err(message.into())}}
fn retain_identity(value:&Value,files:&mut Vec<FileIdentity>)->Result<(),String> {
    // Values originate solely from serialization of the private factory result.
    // These public file records cannot create a source/archive/provider token.
    let file:FileIdentity=serde_json::from_value(value.clone()).map_err(|e|e.to_string())?;
    file.verify()?;files.push(file);Ok(())
}
fn close(files:&[FileIdentity])->Result<(),String> {
    let errors=files.iter().filter_map(|f|f.verify().err()).collect::<Vec<_>>();
    need(errors.is_empty(),&errors.join("; "))
}
pub fn observe_and_control(source:&VerifiedStaticSource,archive:&StaticArchive,
    elf:&FileIdentity,map:&FileIdentity,objdump:&FileIdentity,runner:&mut Runner,
    logs:&Path,files:&mut Vec<FileIdentity>)->Result<(),String> {
    files.extend([elf.clone(),map.clone(),objdump.clone()]);
    let mut reached=false;
    let mut transfer_error=None;
    let mut complete_diagnostic=None;
    let first=source.with_ordinary_trusted_gate(archive,elf,map,runner,|provider,runner| {
        reached=true;
        let identity=serde_json::to_value(provider).map_err(|e|e.to_string())?;
        for key in ["runtime_archive","artifact","link_map"] {
            retain_identity(identity.get(key).ok_or("ordinary serialized file role absent")?,files)?;
        }
        let sources=identity["sources"].as_array().ok_or("ordinary source identities absent")?;
        need(sources.len()==2,"ordinary source identity count differs")?;
        for value in sources {retain_identity(value,files)?;}
        let observed=provider.inspect_entry_transfers(objdump,runner);
        let (diagnostic,diagnostic_error,refusal)=match observed {
            Ok(value)=>{
                let value=serde_json::to_value(&value).map_err(|e|e.to_string())?;
                for key in ["artifact","objdump_tool","disassembly"] {
                    retain_identity(value.get(key).ok_or("ordinary transfer file role absent")?,files)?;
                }
                let refusal=value.get("transfer_refusal").ok_or("ordinary transfer refusal field absent")?.clone();
                need(refusal.is_null()||refusal.is_string(),"ordinary transfer refusal has wrong type")?;
                complete_diagnostic=Some(value.clone());(value,Value::Null,refusal)
            },
            Err(error)=>{transfer_error=Some(error.clone());(Value::Null,Value::String(error),Value::Null)},
        };
        let bytes=serde_json::to_vec_pretty(&json!({
            "state":if transfer_error.is_none(){"OBSERVED_NOT_PROVEN"}else{"TRANSFER_DIAGNOSTIC_ERROR_NOT_PROVEN"},
            "provider":identity,"entry_transfer_diagnostic":diagnostic,
            "transfer_refusal":refusal,"transfer_error":diagnostic_error,
            "qualification":"NOT_PROVEN","readiness":"NOT_PROVEN","guest_release":false
        })).map_err(|e|e.to_string())?;
        let path=logs.join("ordinary-gate.not-proven.json");write_new(&path,&bytes)?;
        let file=FileIdentity::read(&path)?;
        need(file.sha256==sha256(&bytes),"ordinary serialized observation differs")?;
        files.push(file);
        // Preserve a diagnostic refusal/error as data so the real factory's
        // success close runs before the independent callback-error close control.
        Ok(())
    });
    let mut control_detail="NOT_REACHED".to_string();
    let mut control_error=None;
    if first.is_ok() {
        need(reached,"ordinary positive callback not reached")?;
        let sentinel="actual ordinary gate callback error control";
        let mut control_reached=false;
        let observed=source.with_ordinary_trusted_gate(archive,elf,map,runner,|_,_| {
            control_reached=true;Err::<(),String>(sentinel.into())
        });
        match observed {
            Err(e) if control_reached&&e==sentinel=>{
                control_detail="EXACT_CALLBACK_ERROR_AND_SUCCESSFUL_CLOSE".into();
                println!("ORDINARY_GATE_CALLBACK_ERROR_CLOSE=EXACT_CALLBACK_ERROR_AND_SUCCESSFUL_CLOSE");
            },
            other=>{control_error=Some(format!("ordinary callback error/close differs: {other:?}"));
                control_detail=format!("REFUSED: {control_error:?}");}
        }
    }
    let outcome=match (&first,&transfer_error,&control_error) {
        (Ok(()),None,None)=>Ok(()),
        _=>Err(format!("ordinary factory={first:?}; transfer_error={transfer_error:?}; callback_control={control_error:?}")),
    };
    let archive_close=archive.verify();
    let file_close=close(files);
    let detail=logs.join("ordinary-gate-controls.debug");
    let detail_result=write_new(&detail,format!("FACTORY={first:?}\nCALLBACK_REACHED={reached}\nCALLBACK_ERROR_CLOSE={control_detail}\nTRANSFER_ERROR={transfer_error:?}\nARCHIVE_CLOSE={archive_close:?}\nFILE_CLOSE={file_close:?}\nSCOPE=OBSERVATIONS_ONLY_NOT_PROVEN\n").as_bytes());
    if detail_result.is_ok() {files.push(FileIdentity::read(&detail)?);}
    if outcome.is_err()||archive_close.is_err()||file_close.is_err()||detail_result.is_err() {
        return Err(format!("{outcome:?}; archive_close={archive_close:?}; file_close={file_close:?}; detail={detail_result:?}"));
    }
    need(complete_diagnostic.is_some(),"ordinary complete caller diagnostic absent")?;
    println!("ORDINARY_GATE_SUCCESS_CLOSE=OBSERVED_NOT_PROVEN");
    close(files)
}
