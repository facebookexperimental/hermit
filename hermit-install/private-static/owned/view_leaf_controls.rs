//! Eight actual leaf controls: six same-source and two foreign-source admissions. Never calls the observed machine code.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::panic::{catch_unwind, AssertUnwindSafe};
use reverie_liteinst_artifact::{FileIdentity, Runner, sha256, write_new};
use reverie_liteinst_artifact::private_static::{SourceExpectation, StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::SelectedInitializerDefinitions;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Leaf { Site, Table }
impl Leaf { fn name(self) -> &'static str { match self {Self::Site=>"site",Self::Table=>"table"} } }
fn need(ok:bool, message:&str)->Result<(),String> {if ok {Ok(())} else {Err(message.into())}}
fn invoke<T>(kind:Leaf, source:&VerifiedStaticSource, archive:&StaticArchive,
    selected:&SelectedInitializerDefinitions, elf:&FileIdentity, map:&FileIdentity,
    runner:&mut Runner, callback:impl FnOnce(Value)->Result<T,String>)->Result<T,String> {
    match kind {
        Leaf::Site=>source.with_view_site_leaf(archive,selected,elf,map,runner,|observation,_| {
            callback(serde_json::to_value(observation).map_err(|e|e.to_string())?)
        }),
        Leaf::Table=>source.with_view_table_leaf(archive,selected,elf,map,runner,|observation,_| {
            callback(serde_json::to_value(observation).map_err(|e|e.to_string())?)
        }),
    }
}
fn close(selected:&SelectedInitializerDefinitions,archive:&StaticArchive,files:&[FileIdentity])->Result<(),String> {
    let mut errors=Vec::new();
    if let Err(e)=selected.verify() {errors.push(e);}
    if let Err(e)=archive.verify() {errors.push(e);}
    for f in files {if let Err(e)=f.verify() {errors.push(e);}}
    need(errors.is_empty(),&errors.join("; "))
}

// Only the newly copied script used by this actual link may be mutated. No
// original input, source, archive, ELF/map, inode, mode or link is substituted.
struct RestoreScript {
    file:File,identity:FileIdentity,bytes:Vec<u8>,metadata:(u64,u64,u32,u64),
    backup:FileIdentity,journal:FileIdentity,attempted:bool,
}
impl RestoreScript {
    fn new(identity:&FileIdentity,role:&str,logs:&Path)->Result<Self,String> {
        identity.verify()?;
        need(identity.path.parent()==Some(logs.join("optional-inputs").as_path()) &&
            identity.path.file_name().is_some_and(|s|s=="selected-link-script.ld"),
            "view mutation is not the actual newly copied selected link script")?;
        let mut file=OpenOptions::new().read(true).write(true).open(&identity.path).map_err(|e|e.to_string())?;
        let m=file.metadata().map_err(|e|e.to_string())?;
        need(m.is_file()&&!m.permissions().readonly()&&m.len()>0&&m.len()<=128*1024*1024,
            "view selected script is not bounded writable regular input")?;
        let mut bytes=Vec::new();(&mut file).take(128*1024*1024+1).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
        need(bytes.len() as u64==m.len()&&sha256(&bytes)==identity.sha256,"view script changed before backup")?;
        let backup_path=logs.join(format!("view-{role}-script.original.bin"));
        write_new(&backup_path,&bytes)?;
        File::open(&backup_path).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;
        let backup=FileIdentity::read(&backup_path)?;
        need(backup.sha256==identity.sha256,"view script durable backup differs")?;
        let journal_path=logs.join(format!("view-{role}-script.journal.json"));
        write_new(&journal_path,&serde_json::to_vec_pretty(&json!({
            "state":"BACKED_UP_NOT_YET_MUTATED","role":role,"path":identity.path,
            "sha256":identity.sha256,"bytes":bytes.len(),"dev":m.dev(),"ino":m.ino(),
            "mode":m.mode(),"nlink":m.nlink(),"backup":backup.path,
            "scope":"ACTUAL_LEAF_CALLBACK_SELECTED_LEDGER_CLOSE_ONLY_NO_EXECUTION"
        })).map_err(|e|e.to_string())?)?;
        File::open(&journal_path).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;
        File::open(logs).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;
        let journal=FileIdentity::read(&journal_path)?;
        let result=Self {file,identity:identity.clone(),bytes,metadata:(m.dev(),m.ino(),m.mode(),m.nlink()),backup,journal,attempted:false};
        result.verify_restored()?;Ok(result)
    }
    fn same_inode(&self)->Result<(),String> {
        let path=std::fs::symlink_metadata(&self.identity.path).map_err(|e|e.to_string())?;
        let held=self.file.metadata().map_err(|e|e.to_string())?;
        for m in [&path,&held] {need(m.is_file()&&(m.dev(),m.ino(),m.mode(),m.nlink())==self.metadata,
            "view script inode/mode/link custody changed")?;} Ok(())
    }
    fn verify_restored(&self)->Result<(),String> {
        self.same_inode()?;self.backup.verify()?;self.journal.verify()?;self.identity.verify()?;
        need(self.file.metadata().map_err(|e|e.to_string())?.len()==self.bytes.len() as u64,
            "view restored script length differs")
    }
    fn alter(&mut self)->Result<(),String> {
        self.verify_restored()?;self.attempted=true;
        self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
        self.file.write_all(&[self.bytes[0]^1]).map_err(|e|e.to_string())?;
        self.file.sync_all().map_err(|e|e.to_string())?;self.same_inode()?;
        need(FileIdentity::read(&self.identity.path)?.sha256!=self.identity.sha256,"view script alteration not observed")
    }
    fn restore(&mut self)->Result<(),String> {
        if self.attempted {self.same_inode()?;
            self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
            self.file.write_all(&self.bytes).map_err(|e|e.to_string())?;
            self.file.set_len(self.bytes.len() as u64).map_err(|e|e.to_string())?;
            self.file.sync_all().map_err(|e|e.to_string())?;}
        self.verify_restored()?;self.attempted=false;Ok(())
    }
}
impl Drop for RestoreScript {
    fn drop(&mut self) {if self.attempted {if let Err(e)=self.restore() {
        eprintln!("VIEW_LEAF_SCRIPT_RESTORE_FAILED: {e}; backup={}",self.backup.path.display());
    }}}
}

pub struct Outcome {
    pub report:Value,
    pub files:Vec<FileIdentity>,
    /// Fatal: no later consumer may run, even if Drop subsequently retries.
    pub restoration_failed:bool,
}
fn retain_json(path:&Path,value:&Value,files:&mut Vec<FileIdentity>)->Result<(),String> {
    let bytes=serde_json::to_vec_pretty(value).map_err(|e|e.to_string())?;
    write_new(path,&bytes)?;let file=FileIdentity::read(path)?;
    need(file.sha256==sha256(&bytes),"view retained serialization differs")?;
    files.push(file);Ok(())
}

pub fn observe_and_controls(source:&VerifiedStaticSource,archive:&StaticArchive,
    selected:&SelectedInitializerDefinitions,elf:&FileIdentity,map:&FileIdentity,
    script:&FileIdentity,runner:&mut Runner,logs:&Path)->Outcome {
    let mut files=vec![elf.clone(),map.clone(),script.clone()];
    let mut roles=Vec::new();let mut restoration_failed=false;
    for kind in [Leaf::Site,Leaf::Table] {
        let role=kind.name();let mut rows=Vec::new();
        let mut reached=0usize;let mut observed=None;
        let positive=invoke(kind,source,archive,selected,elf,map,runner,|value| {
            reached+=1;
            retain_json(&logs.join(format!("view-{role}-leaf.not-qualified.json")),&value,&mut files)?;
            observed=Some(value);Ok(())
        });
        let closed=close(selected,archive,&files);
        rows.push(json!({"case":"positive","state":if positive.is_ok()&&reached==1&&closed.is_ok(){"PASS"}else{"FAIL"},
            "callback_count":reached,"factory_result":format!("{positive:?}"),"post_close":format!("{closed:?}")}));
        let sentinel=format!("actual View::{role} leaf callback error control");
        let mut reached=0usize;
        let error=invoke(kind,source,archive,selected,elf,map,runner,|_| {
            reached+=1;Err::<(),String>(sentinel.clone())
        });
        let closed=close(selected,archive,&files);
        rows.push(json!({"case":"callback_error","state":if reached==1&&matches!(&error,Err(e) if e==&sentinel)&&closed.is_ok(){"PASS"}else{"FAIL"},
            "callback_count":reached,"expected_exact_error":sentinel,"factory_result":format!("{error:?}"),"post_close":format!("{closed:?}")}));
        // No mutation starts before the complete durable backup/journal exists.
        let mutation=match RestoreScript::new(script,role,logs) {
            Err(error)=>json!({"case":"producer_ledger_mutation_close","state":"FAIL","callback_count":0,"setup_error":error,"restore":"NOT_MUTATED"}),
            Ok(mut restore)=>{
                let mut reached=0usize;
                let result=catch_unwind(AssertUnwindSafe(||invoke(kind,source,archive,selected,elf,map,runner,|_| {
                    reached+=1;restore.alter()?;Ok(())
                })));
                // Restore before interpreting any error or doing another close.
                let restored=restore.restore();restoration_failed=restored.is_err();
                let expected=format!("static source transaction close: file identity changed: {}",script.path.display());
                let exact=reached==1&&matches!(&result,Ok(Err(e)) if e==&expected);
                let closed=if restored.is_ok(){close(selected,archive,&files)}else{Err("restoration unconfirmed; later consumer forbidden".into())};
                files.extend([restore.backup.clone(),restore.journal.clone()]);
                let detail=match &result {Ok(v)=>format!("{v:?}"),Err(_)=>"CALLBACK_PANICKED".into()};
                json!({"case":"producer_ledger_mutation_close","state":if exact&&restored.is_ok()&&closed.is_ok(){"PASS"}else{"FAIL"},
                    "callback_count":reached,"expected_exact_error":expected,"factory_result":detail,
                    "restore":format!("{restored:?}"),"post_restore_close":format!("{closed:?}"),
                    "backup":restore.backup.clone(),"journal":restore.journal.clone()})
            }
        };
        rows.push(mutation);
        let passed=rows.len()==3&&rows.iter().all(|r|r["state"]=="PASS");
        roles.push(json!({"role":role,"state":if passed{"OBSERVED_NOT_QUALIFIED"}else{"REFUSED"},"controls":rows,"observation":observed}));
        if restoration_failed {break;}
    }
    let passed=roles.len()==2&&roles.iter().all(|r|r["state"]=="OBSERVED_NOT_QUALIFIED");
    Outcome {report:json!({"schema":1,"state":if passed{"SIX_CONTROLS_PENDING_FOREIGN_OWNER"}else{"REFUSED"},
        "roles":roles,"required_roles":2,"required_controls":8,"restoration_failed":restoration_failed,
        "qualification":"NOT_PROVEN","caller_and_async":"NOT_PROVEN","readiness":"NOT_PROVEN","guests_executed":0}),files,restoration_failed}
}

/// Two real API-boundary negatives under a shared nested source context. This
/// runs AFTER existing native controls so their original exact2 metadata guard
/// remains unchanged. Both real leaf APIs must reject before either callback.
pub fn foreign_owner_controls(source:&VerifiedStaticSource,archive:&StaticArchive,
    selected:&SelectedInitializerDefinitions,expected:&SourceExpectation<'_>,
    elf:&FileIdentity,map:&FileIdentity,lld:&Path,runner:&mut Runner,
    report:&mut Value,files:&[FileIdentity])->Result<(),String> {
    let expected_error="static archive belongs to another source transaction";
    let start=runner.commands.len();
    let mut rows=Vec::new();
    let nested=VerifiedStaticSource::with_resolved(expected,runner,|foreign,runner| {
        need(source.metadata_sha256()==archive.metadata_sha256() &&
            foreign.metadata_sha256()==archive.metadata_sha256(),
            "View foreign-owner control needs equal actual metadata")?;
        for kind in [Leaf::Site,Leaf::Table] {
            let mut reached=0usize;
            let result=invoke(kind,foreign,archive,selected,elf,map,runner,|_| {
                reached+=1;Err::<(),String>("foreign View leaf callback reached".into())
            });
            rows.push((kind,json!({"case":"foreign_source_owner",
                "state":if reached==0&&matches!(&result,Err(e) if e==expected_error){"PASS"}else{"FAIL"},
                "callback_count":reached,"expected_exact_error":expected_error,
                "factory_result":format!("{result:?}"),
                "outer_metadata_sha256":source.metadata_sha256(),
                "nested_metadata_sha256":foreign.metadata_sha256(),
                "archive_metadata_sha256":archive.metadata_sha256()})));
        }
        Ok(()) // with_resolved must still close this real nested source.
    });
    let closed=close(selected,archive,files);
    let no_link=runner.commands[start..].iter().all(|c|c.program.as_str()!=lld.to_string_lossy().as_ref());
    let roles=report["roles"].as_array_mut().ok_or("View foreign control has no actual role report")?;
    for kind in [Leaf::Site,Leaf::Table] {
        let name=kind.name();
        let mut row=rows.iter().find(|(k,_)|k.name()==name).map(|(_,r)|r.clone()).unwrap_or_else(||
            json!({"case":"foreign_source_owner","state":"FAIL","callback_count":0,
                "factory_result":"NOT_REACHED","expected_exact_error":expected_error}));
        row["nested_source_close"]=json!(format!("{nested:?}"));
        row["outer_input_close"]=json!(format!("{closed:?}"));
        row["no_additional_link"]=json!(no_link);
        if nested.is_err()||closed.is_err()||!no_link {row["state"]=json!("FAIL");}
        let role=roles.iter_mut().find(|r|r["role"]==name).ok_or("View foreign control role missing")?;
        let controls=role["controls"].as_array_mut().ok_or("View original six controls absent")?;
        controls.push(row);
        let pass=controls.len()==4&&controls.iter().all(|r|r["state"]=="PASS");
        role["state"]=json!(if pass{"OBSERVED_NOT_QUALIFIED"}else{"REFUSED"});
    }
    let pass=roles.len()==2&&roles.iter().all(|r|r["state"]=="OBSERVED_NOT_QUALIFIED");
    report["state"]=json!(if pass{"ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED"}else{"REFUSED"});
    Ok(())
}

/// Write after all actual controls; the main caller retains this identity across
/// the outer source close before emitting any closed protocol marker.
pub fn retain_report(logs:&Path,report:&Value,files:&mut Vec<FileIdentity>)->Result<FileIdentity,String> {
    let path:PathBuf=logs.join("view-leaf-controls.not-qualified.json");
    retain_json(&path,report,files)?;
    files.last().cloned().ok_or("view leaf report identity absent".into())
}
