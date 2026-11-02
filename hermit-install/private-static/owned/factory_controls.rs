//! Genuine factory controls using one actual owned source/archive lifetime.
//! No substitute source token, old archive import, or LLD call on negatives.
use std::path::Path;
use reverie_liteinst_artifact::{FileIdentity, Runner, write_new};
use reverie_liteinst_artifact::private_static::{SourceExpectation, StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::InitializerLinkInputs;

pub fn negatives(source:&VerifiedStaticSource, archive:&StaticArchive, expected:&SourceExpectation<'_>,
    normal:&InitializerLinkInputs<'_>, runner:&mut Runner, directory:&Path)->Result<usize,String> {
    std::fs::create_dir(directory).map_err(|e|e.to_string())?;
    let original_libc=FileIdentity::read(normal.libc)?;
    let original_ssp=FileIdentity::read(normal.stack_chk_source)?;
    let original_unwind=FileIdentity::read(normal.supporting_archives.first().ok_or("missing actual unwind")?)?;
    let original_script=FileIdentity::read(normal.script)?;
    let originals=[original_libc,original_ssp,original_unwind,original_script];
    let result=(|| {
        let mut paths=Vec::new();
        for (i,file) in originals.iter().enumerate() {
            let mut bytes=std::fs::read(&file.path).map_err(|e|e.to_string())?;
            if i==3 { bytes.extend_from_slice(b"\nINCLUDE not-an-authorized-input\n"); }
            else { let first=bytes.first_mut().ok_or("empty actual factory input")?;*first^=1; }
            let path=directory.join(format!("changed-{i}.input"));write_new(&path,&bytes)?;paths.push(path);
        }
        // Two actual separately resolved contexts, same actual metadata and
        // ONE real build. No old receipt is imported and no second build runs.
        if source.metadata_sha256()!=archive.metadata_sha256() {
            return Err("outer actual source/archive metadata differs".into());
        }
        let owner_out=directory.join("refused-owner-link");
        let owner_input=InitializerLinkInputs { lld:normal.lld,script:normal.script,libc:normal.libc,
            supporting_archives:normal.supporting_archives,memcpy_source:normal.memcpy_source,
            musl_source_authority:normal.musl_source_authority,stack_chk_source:normal.stack_chk_source,
            output_directory:&owner_out };
        VerifiedStaticSource::with_resolved(expected,runner,|other,runner| {
            if other.metadata_sha256()!=archive.metadata_sha256() {
                return Err("nested actual metadata differs: wrong-owner control not exercised".into());
            }
            let start=runner.commands.len();
            let owner_error="static archive belongs to another source transaction";
            match other.link_and_join_initializer_definitions(archive,&owner_input,runner) {
                Err(e) if e==owner_error=>{},Err(e)=>return Err(format!("wrong owner refused elsewhere: {e}")),
                Ok(_)=>return Err("different actual source context accepted the archive".into()),
            }
            // Also cover the existing diagnostic consumer. The absent output
            // is intentional: ownership must refuse BEFORE trying to read it.
            match other.inspect_final(archive,&directory.join("not-produced.not-qualified.elf"),runner) {
                Err(e) if e==owner_error=>{},Err(e)=>return Err(format!("inspection owner refused elsewhere: {e}")),
                Ok(_)=>return Err("inspection accepted another context's archive".into()),
            }
            // All placeholders are genuine existing archive FileIdentities.
            // Owner rejection must precede trying to consume any as ELF/map/tool.
            match other.inspect_private_startup_census(archive, archive.file(),
                archive.file(), archive.file(), runner) {
                Err(e) if e==owner_error=>{},Err(e)=>return Err(format!("census owner refused elsewhere: {e}")),
                Ok(_)=>return Err("census accepted another context's archive".into()),
            }
            // Ownership must refuse before treating these genuine archive file
            // identities as ELF/map input or entering the ordinary callback.
            match other.with_ordinary_trusted_gate(archive, archive.file(), archive.file(),
                runner, |_,_| Err::<(),String>("foreign ordinary callback reached".into())) {
                Err(e) if e==owner_error=>{},
                Err(e)=>return Err(format!("ordinary owner refused elsewhere: {e}")),
                Ok(_)=>return Err("ordinary accepted another context's archive".into()),
            }
            if owner_out.exists() || runner.commands[start..].iter().any(|c|
                c.program.as_str()==normal.lld.to_string_lossy().as_ref()) {
                return Err("wrong-owner control reached output/linker".into());
            }
            archive.verify()?;
            println!("FACTORY_NEGATIVE_OWNER=EXACT_REFUSAL_LINK_INSPECT_CENSUS_AND_ORDINARY SAME_ACTUAL_METADATA CONSUMER_ASSERTIONS=8");
            Ok(())
        })?;
        for (case,message) in [
            (0,"initializer libc is not exact private July12 native input"),
            (1,"initializer SSP source differs from source-bound musl archive member"),
            (2,"initializer supporting inputs differ from existing V12 private libunwind"),
            (3,"initializer script can introduce unbound file inputs"),
        ] {
            let unwind=if case==2 { paths[2].as_path() } else { normal.supporting_archives[0] };
            let support=[unwind];let out=directory.join(format!("refused-link-{case}"));
            let input=InitializerLinkInputs { lld:normal.lld,
                script:if case==3 { &paths[3] } else { normal.script },
                libc:if case==0 { &paths[0] } else { normal.libc },supporting_archives:&support,
                memcpy_source:normal.memcpy_source,musl_source_authority:normal.musl_source_authority,
                stack_chk_source:if case==1 { &paths[1] } else { normal.stack_chk_source },output_directory:&out };
            let start=runner.commands.len();
            let observed=source.link_and_join_initializer_definitions(archive,&input,runner);
            // Exact error, not any failure: a source close error cannot count.
            match observed { Err(e) if e==message=>{},Err(e)=>return Err(format!("case{case} refused at wrong check: {e}")),
                Ok(_)=>return Err(format!("case{case} unexpectedly admitted changed actual input")), }
            if out.exists() || runner.commands[start..].iter().any(|c|c.program.as_str()==normal.lld.to_string_lossy().as_ref()) {
                return Err(format!("case{case} reached linker instead of its intended refusal"));
            }
            archive.verify()?;
            println!("FACTORY_NEGATIVE_{case}=EXACT_REFUSAL {message}");
        }
        Ok(5)
    })();
    let close=originals.iter().map(FileIdentity::verify).collect::<Result<Vec<_>,_>>();
    match(result,close) { (Ok(n),Ok(_))=>Ok(n),(Err(e),Ok(_))=>Err(e),
        (Ok(_),Err(c))=>Err(format!("factory original-input close: {c}")),
        (Err(e),Err(c))=>Err(format!("{e}; factory original-input close: {c}")), }
}
