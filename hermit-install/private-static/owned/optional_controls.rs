//! Actual same-owner optional observations and returned-ledger verification.
//! No callback-close claim. Mutations affect only newly owned input copies.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::panic::{catch_unwind, AssertUnwindSafe};
use reverie_liteinst_artifact::{FileIdentity, Runner, sha256, write_new};
use reverie_liteinst_artifact::private_static::{StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::{
    OptionalSourceInputs, OptionalZeroObservation, SelectedInitializerDefinitions,
};
fn need(ok:bool, message:&str)->Result<(),String> { if ok {Ok(())} else {Err(message.into())} }

pub struct Inputs {
    pub script: FileIdentity,
    sources: [FileIdentity;3],
    originals: Vec<FileIdentity>,
}
impl Inputs {
    // Exact copies become the actual link/observation inputs before either call.
    pub fn prepare(script:&Path, sources:[&Path;3], logs:&Path)->Result<Self,String> {
        let directory=logs.join("optional-inputs");
        std::fs::create_dir(&directory).map_err(|e|e.to_string())?;
        let paths=[script,sources[0],sources[1],sources[2]];
        let names=["selected-link-script.ld","std-weak-syscall.rs","std-destructors.rs","musl-dl-iterate-phdr.c"];
        let mut originals=Vec::new(); let mut copies=Vec::new();
        for (path,name) in paths.into_iter().zip(names) {
            let original=FileIdentity::read(path)?;
            need(std::fs::metadata(&original.path).map_err(|e|e.to_string())?.len()<=128*1024*1024,
                "optional original input exceeds bound")?;
            let bytes=std::fs::read(&original.path).map_err(|e|e.to_string())?;
            original.verify()?;
            need(sha256(&bytes)==original.sha256,"optional original changed while copying")?;
            let target=directory.join(name);write_new(&target,&bytes)?;
            let copy=FileIdentity::read(&target)?;
            need(copy.sha256==original.sha256,"optional actual input copy differs")?;
            originals.push(original);copies.push(copy);
        }
        let result=Self {script:copies[0].clone(),sources:[copies[1].clone(),copies[2].clone(),copies[3].clone()],originals};
        result.verify()?;Ok(result)
    }
    pub fn files(&self)->Vec<FileIdentity> {
        self.originals.iter().chain(std::iter::once(&self.script)).chain(&self.sources).cloned().collect()
    }
    pub fn verify(&self)->Result<(),String> {
        let errors=self.files().iter().filter_map(|f|f.verify().err()).collect::<Vec<_>>();
        need(errors.is_empty(),&errors.join("; "))
    }
}

struct RestoreCopy {
    file:File, identity:FileIdentity, bytes:Vec<u8>, metadata:(u64,u64,u32,u64),
    backup:FileIdentity, journal:FileIdentity, attempted:bool,
}
impl RestoreCopy {
    fn new(identity:&FileIdentity, role:&str, logs:&Path)->Result<Self,String> {
        identity.verify()?;
        need(identity.path.parent()==Some(logs.join("optional-inputs").as_path()),
            "optional mutation is not a newly owned actual input copy")?;
        let mut file=OpenOptions::new().read(true).write(true).open(&identity.path).map_err(|e|e.to_string())?;
        let m=file.metadata().map_err(|e|e.to_string())?;
        need(m.is_file()&&!m.permissions().readonly()&&m.len()>0&&m.len()<=128*1024*1024,
            "optional copy not bounded writable nonempty regular file")?;
        let mut bytes=Vec::new();(&mut file).take(128*1024*1024+1).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
        need(bytes.len() as u64==m.len()&&sha256(&bytes)==identity.sha256,"optional copy changed before backup")?;
        let backup_path=logs.join(format!("optional-{role}.original.bin"));write_new(&backup_path,&bytes)?;
        let backup=FileIdentity::read(&backup_path)?;
        need(backup.sha256==identity.sha256,"optional backup differs")?;
        let journal_path=logs.join(format!("optional-{role}.journal.json"));
        write_new(&journal_path,&serde_json::to_vec_pretty(&serde_json::json!({
            "state":"BACKED_UP_NOT_YET_MUTATED","path":identity.path,"sha256":identity.sha256,
            "bytes":bytes.len(),"dev":m.dev(),"ino":m.ino(),"mode":m.mode(),"nlink":m.nlink(),
            "backup":backup.path,"scope":"RETURNED_LEDGER_VERIFY_ONLY_NEW_COPY_NO_EXECUTION"
        })).map_err(|e|e.to_string())?)?;
        let journal=FileIdentity::read(&journal_path)?;
        File::open(logs).and_then(|directory|directory.sync_all()).map_err(|e|e.to_string())?;
        let result=Self {file,identity:identity.clone(),bytes,metadata:(m.dev(),m.ino(),m.mode(),m.nlink()),backup,journal,attempted:false};
        result.verify_restored()?;Ok(result)
    }
    fn same_inode(&self)->Result<(),String> {
        let path=std::fs::symlink_metadata(&self.identity.path).map_err(|e|e.to_string())?;
        let held=self.file.metadata().map_err(|e|e.to_string())?;
        for m in [&path,&held] {need(m.is_file()&&(m.dev(),m.ino(),m.mode(),m.nlink())==self.metadata,
            "optional copy inode/mode/link custody changed")?;} Ok(())
    }
    fn verify_restored(&self)->Result<(),String> {
        self.same_inode()?;self.backup.verify()?;self.journal.verify()?;self.identity.verify()?;
        need(self.file.metadata().map_err(|e|e.to_string())?.len()==self.bytes.len() as u64,"optional copy restored length differs")
    }
    fn alter(&mut self)->Result<(),String> {
        self.verify_restored()?;self.attempted=true;
        self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
        self.file.write_all(&[self.bytes[0]^1]).map_err(|e|e.to_string())?;
        self.file.sync_all().map_err(|e|e.to_string())?;self.same_inode()?;
        need(FileIdentity::read(&self.identity.path)?.sha256!=self.identity.sha256,"optional copy mutation not observed")
    }
    fn restore(&mut self)->Result<(),String> {
        if self.attempted {self.same_inode()?;self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
            self.file.write_all(&self.bytes).map_err(|e|e.to_string())?;
            self.file.set_len(self.bytes.len() as u64).map_err(|e|e.to_string())?;
            self.file.sync_all().map_err(|e|e.to_string())?;}
        self.verify_restored()?;self.attempted=false;Ok(())
    }
}
impl Drop for RestoreCopy {
    fn drop(&mut self) {if self.attempted {if let Err(e)=self.restore() {
        eprintln!("OPTIONAL_COPY_RESTORE_FAILED: {e}; backup={}",self.backup.path.display());}}}
}
fn returned_close(observation:&OptionalZeroObservation, selected:&SelectedInitializerDefinitions,
    archive:&StaticArchive, identity:&FileIdentity, role:&str, logs:&Path, files:&mut Vec<FileIdentity>)->Result<(),String> {
    let mut guard=RestoreCopy::new(identity,role,logs)?;
    let outcome=catch_unwind(AssertUnwindSafe(|| {guard.alter()?;observation.verify()}));
    // Restore before inspecting the result or running another factory/tool.
    let restore=guard.restore();
    let outcome_text=match &outcome {Ok(v)=>format!("{v:?}"),Err(_)=>"CONTROL_PANICKED".into()};
    let receipt=logs.join(format!("optional-{role}.restore.debug"));
    let recorded=write_new(&receipt,format!("RESULT={outcome_text}\nRESTORE={restore:?}\nORIGINAL={:?}\n",guard.identity).as_bytes());
    // Keep both errors; Drop remains active if explicit restoration failed.
    if restore.is_err()||recorded.is_err() {return Err(format!("optional restore={restore:?}; receipt={recorded:?}"));}
    files.extend([guard.identity.clone(),guard.backup.clone(),guard.journal.clone(),FileIdentity::read(&receipt)?]);
    guard.verify_restored()?;observation.verify()?;selected.verify()?;archive.verify()?;
    let exact=format!("file identity changed: {}",identity.path.display());
    match outcome {Ok(Err(e)) if e==exact=>{},_=>return Err(format!("optional returned-ledger control refused elsewhere: {outcome_text}"))}
    println!("OPTIONAL_ZERO_{role}_CLOSE=EXACT_RETURNED_FILE_REFUSAL_RESTORED");Ok(())
}

pub fn observe_and_controls(source:&VerifiedStaticSource, archive:&StaticArchive,
    selected:&SelectedInitializerDefinitions, inputs:&Inputs, objdump:&Path, runner:&mut Runner,
    logs:&Path, files:&mut Vec<FileIdentity>, retained:&mut Option<OptionalZeroObservation>)->Result<(),String> {
    inputs.verify()?;files.extend(inputs.files());
    let input=OptionalSourceInputs {objdump,std_weak_syscall:&inputs.sources[0].path,
        std_destructor_registration:&inputs.sources[1].path,musl_dl_iterate_phdr:&inputs.sources[2].path};
    let observed=source.observe_optional_zero_references(archive,selected,&input,runner)?;
    observed.verify()?;
    need(observed.selected_use_count()>0,"optional observation has no actual selected use")?;
    let bytes=format!("{observed:#?}\nSELECTED_USES={}\nUNSELECTED_INPUT_USES={}\nSCOPE=FINITE_OPTIONAL_ZERO_USE_OBSERVATION_NOT_PROVEN\n",
        observed.selected_use_count(),observed.unselected_input_use_count()).into_bytes();
    let path=logs.join("optional-zero.not-proven.debug");write_new(&path,&bytes)?;
    let file=FileIdentity::read(&path)?;need(file.sha256==sha256(&bytes),"optional serialized observation differs")?;
    files.push(file);*retained=Some(observed);
    let observed=retained.as_ref().ok_or("actual optional observation not retained")?;
    println!("OPTIONAL_ZERO_FULL_PRODUCER_LEDGER=OBSERVED_NOT_PROVEN");
    returned_close(observed,selected,archive,&inputs.sources[0],"SOURCE_LEDGER",logs,files)?;
    returned_close(observed,selected,archive,&inputs.script,"EXTRA_LINK_LEDGER",logs,files)?;
    inputs.verify()?;observed.verify()?;selected.verify()?;archive.verify()?;
    println!("OPTIONAL_ZERO_CALLBACK_CLOSE=NOT_TESTED_NO_CALLBACK_API");
    for f in files {f.verify()?;}Ok(())
}
