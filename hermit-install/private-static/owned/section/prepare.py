#!/usr/bin/env python3
"""Prepare V12 section/script inputs from actual maintained archive evidence; NEVER link."""
import argparse, fnmatch, hashlib, importlib.util, json, os, re, resource, struct, subprocess, sys, time
from contextlib import ExitStack
from pathlib import Path
sys.dont_write_bytecode = True
ASSETS=Path(__file__).resolve().parent.parent/'assets'
ROOT='detcore_liteinst_mapped_entry'
MAINTAINED=Path(__file__).resolve().parent.parent/'archive/prepare.py'
MAINTAINED_SHA='f04fa86505f68aa7ca44cb720a15d134f3c38d4072bc0b5e725482a073a55eeb'
# Preserve the existing private static-image C/TLS ABI; retention is not provenance.
ABI_ROOTS=['__init_tls', 'malloc', '__libc_malloc', '__libc_malloc_impl', 'calloc', '__libc_calloc', 'free', '__libc_free', 'realloc', '__libc_realloc', 'reallocarray', 'aligned_alloc', 'memalign', 'valloc', 'posix_memalign', 'malloc_usable_size', '__malloc_allzerop']
RO_ROOTS={'.reverie_inguest_fixed_gate_template':(4096,4096,2),'.rodata.hermit_mapped_layout':(96,8,0x200002)} # measured SHF_ALLOC / SHF_ALLOC|SHF_GNU_RETAIN
BASE,END=0x6000e1000000,0x6000ff000000
HELPERS={
 'archive_metadata.py':'d313a47a769e91e6d297c5452edf66b9129370f381c25e228385efdb01d60972',
}
LIMIT=128*1024*1024
RELOCS=1024*1024*1024

def need(ok,why):
 if not ok: raise RuntimeError(why)
def cancellation(pending,error):
 # Keep the first KeyboardInterrupt/SystemExit to re-raise after every close.
 return pending if pending is not None or isinstance(error,Exception) else error
def sha(path):
 h=hashlib.sha256()
 with Path(path).open('rb') as f:
  for part in iter(lambda:f.read(1024*1024),b''): h.update(part)
 return h.hexdigest()
def save(path,value):
 q=path.with_name(path.name+'.pending');q.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n');q.replace(path)
def module(name):
 path=ASSETS/name;need(sha(path)==HELPERS[name],'helper changed: '+name)
 spec=importlib.util.spec_from_file_location(name.replace('.','_'),path);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m
def identity(path):
 path=Path(path);need(path.is_file() and not path.is_symlink(),'not regular: '+str(path))
 return {'path':str(path),'bytes':path.stat().st_size,'sha256':sha(path)}
def response(tokens):
 need(all(not any(x in t for x in '\n\r\0') for t in tokens),'invalid response token')
 return '\n'.join('"'+t.replace('\\','\\\\').replace('"','\\"')+'"' for t in tokens)+'\n'

# Placement coordinates only, derived internally from this preparation's raw
# archive metadata. They cannot construct a source owner or qualified role.
ORDINARY_ROLES=('reverie_inguest_trusted_syscall','reverie_inguest_trusted_syscall_ip','reverie_inguest_trusted_syscall_return_ip')

def ordinary_selector(rows,objects,symbol_inputs,runtime_archive):
 archive=str(runtime_archive)
 need(re.fullmatch(r'/[A-Za-z0-9_./-]+',archive) is not None and all(p not in ('','.','..') for p in archive.split('/')[1:]),'ordinary archive selector path is not literal')
 ledgers=[entry for entry in symbol_inputs if str(entry[0])==archive]
 need(len(ledgers)==1,'ordinary runtime symbol ledger absent/duplicate')
 selected=[]
 for name in ORDINARY_ROLES:
  definitions=[]
  for path,physical,symbols in symbol_inputs:
   for symbol in symbols:
    if symbol['name']==name and symbol['section_index']!=0:
     need(str(path)==archive and symbol['object']['archive']==archive,'ordinary definition in foreign archive')
     definitions.append(symbol)
  need(len(definitions)==1,'ordinary defining role absent/duplicate: '+name)
  selected.append(definitions[0])
 owner,native,post=selected
 obj=owner['object']
 need(set(obj)=={'archive','ordinal','header_offset','member'} and obj['archive']==archive and type(obj['ordinal']) is int and obj['ordinal']>=0 and type(obj['header_offset']) is int and obj['header_offset']>=8,'ordinary physical identity malformed')
 member=obj['member']
 need(isinstance(member,str) and re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.-]*',member) is not None and '..' not in member,'ordinary member selector is not literal')
 need(sum(x==obj for x in objects)==1 and sum(x==obj for x in ledgers[0][1])==1,'ordinary physical identity absent/duplicate')
 need(sum(x['member']==member for x in objects)==1,'ordinary member basename ambiguous')
 need(all(s['object']==obj for s in selected),'ordinary role labels have different physical owners')
 index=owner['section_index']
 need(type(index) is int and index>0 and all(s['section_index']==index for s in selected),'ordinary role labels have different sections')
 need(owner['binding']==1 and owner['type']==2 and owner['visibility']==2 and owner['size']==44,'ordinary owner typed declaration differs')
 offset=owner['value']
 need(type(offset) is int and offset in (0,112),'ordinary owner offset is not a closed shape')
 for symbol,delta in ((native,32),(post,34)):
  need(symbol['binding']==1 and symbol['type']==0 and symbol['visibility']==2 and symbol['size']==0 and symbol['value']==offset+delta,'ordinary native/post typed declaration differs')
 sections=[row for row in rows if all(row.get(k)==v for k,v in obj.items()) and row['index']==index]
 need(len(sections)==1,'ordinary physical section absent/duplicate')
 section=sections[0]
 need(section['name']=='.text' and section['type']==1 and section['flags']==6 and section['align']==16 and (section['size'],offset) in ((177,0),(289,112)),'ordinary section is not a closed text shape')
 # A file selector uses the member name, so prove it denotes exactly this
 # physical section, not another section/member with an equal printable name.
 matches=[row for row in rows if row['archive']==archive and row['member']==member and row['name']=='.text']
 need(matches==[section],'ordinary exact selector matches ambiguous physical sections')
 return {'object':obj,'section':section,'roles':selected,'owner_offset':offset,'input_description':'"'+archive+':'+member+'"(.text)','scope':'PREPARATION_PLACEMENT_COORDINATES_NOT_QUALIFICATION'}

def script(rows,helper,symbol_inputs,objects,runtime_archive):
 ordinary=ordinary_selector(rows,objects,symbol_inputs,runtime_archive)
 # Reuse original strict input classification; only ordinary RO/data/BSS lose KEEP.
 # Only this new private entry and gate geometry are different roles.
 helper.ROOT=ROOT
 _,groups=helper.make_script(rows)
 gate='.reverie_inguest_mapped_gate_page'
 physical=[r for r in rows if r['name']==gate]
 need(len(physical)==1 and physical[0]['size']==4096 and physical[0]['flags']==6 and physical[0]['type']==1,'actual one-page RX gate section required')
 need(gate in groups['text'],'gate is not actual text')
 groups['text'].remove(gate);groups['gate']={gate}
 # Exact synthetic GOT names have explicit RO placement, never ambient orphan
 # placement. Remove matching physical sections from their old data group.
 for name,group in (('.got','got'),('.got.plt','gotplt')):
  for names in groups.values():names.discard(name)
  groups[group]={name}
 prefixes={'text':'.text.','ro':'.rodata.','data':'.data.','bss':'.bss.','gcc':'.gcc_except_table.','tdata':'.tdata.','tbss':'.tbss.'}
 patterns={}
 for group,names in groups.items():
  prefix=prefixes.get(group);subset={n for n in names if prefix and n.startswith(prefix)}
  selected=sorted(names-subset)
  if subset:
   observed={r['name'] for r in rows if fnmatch.fnmatchcase(r['name'],prefix+'*')}
   need(observed==subset,'compact selector captures foreign section: '+group)
   selected.append(prefix+'*')
  patterns[group]=selected
 before=hashlib.sha256();after=hashlib.sha256();count=0
 owner={n:g for g,ns in groups.items() for n in ns}
 for row in rows:
  expected=owner.get(row['name']);actual=[g for g,ps in patterns.items() if any(fnmatch.fnmatchcase(row['name'],p) for p in ps)]
  need(actual==([] if expected is None else [expected]),'physical input group mismatch')
  if expected:
   before.update(json.dumps([row,expected],sort_keys=True).encode());after.update(json.dumps([row,actual[0]],sort_keys=True).encode());count+=1
 need(before.digest()==after.digest(),'input grouping changed')
 # These real target objects are also consumed by host-only symbol lookup.
 # Require exact physical members before retaining them ahead of generic RO.
 for name,(size,alignment,flags) in RO_ROOTS.items():
  selected=[r for r in rows if r['name']==name]
  need(len(selected)==1 and selected[0]['archive']==rows[0]['archive'] and selected[0]['size']==size and selected[0]['flags']==flags and selected[0]['type']==1 and selected[0]['align']>=alignment and name in groups['ro'],'host-only RO ABI section differs: '+name)
 def inputs(g,keep=True):
  if g in ('init','fini','preinit'):
   prefix='.'+{'init':'init_array','fini':'fini_array','preinit':'preinit_array'}[g]
   return 'KEEP(*(SORT_BY_INIT_PRIORITY('+prefix+'.*))) KEEP(*('+prefix+'))'
  tokens=['SORT_BY_NAME('+n+')' if '*' in n else n for n in patterns[g]]
  if not tokens:return ''
  value='*('+' '.join(tokens)+')';return 'KEEP('+value+')' if keep else value
 lines=['OUTPUT_FORMAT(elf64-x86-64)','OUTPUT_ARCH(i386:x86-64)','ENTRY('+ROOT+')',
 'PHDRS { header PT_LOAD FILEHDR PHDRS FLAGS(4); text PT_LOAD FLAGS(5); ro PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); tls PT_TLS FLAGS(4); stack PT_GNU_STACK FLAGS(6); }','SECTIONS {',
 '  . = 0x%x + SIZEOF_HEADERS;'%BASE,
 '  __ehdr_start = 0x%x; __executable_start = 0x%x;'%(BASE,BASE),
 '  .image_header : { BYTE(0) } :header',
 '  ASSERT(. <= 0x%x, "header exceeds its RO page")'%(BASE+4096),
 '  . = 0x%x;'%(BASE+4096),
 '  .mapped_gate : { '+inputs('gate')+' } :text',
 '  ASSERT(ADDR(.mapped_gate) == 0x%x && SIZEOF(.mapped_gate) == 4096, "mapped gate ABI differs")'%(BASE+4096),
 '  .text : { '+ordinary['input_description']+' '+inputs('text',False)+' } :text',
 '  ASSERT(ADDR(.text) == reverie_inguest_mapped_gate_page_end, "ordinary text does not start at mapped gate end")',
 '  ASSERT(reverie_inguest_trusted_syscall == ADDR(.text) + %d, "ordinary owner/contribution offset differs")'%ordinary['owner_offset'],
 # Select these exact inputs before the generic .rodata.* selector. They
 # remain in the same RO PT_LOAD, but cannot inherit unrelated section flags.
 '  . = ALIGN(4096);',
 '  .gate_reference : { KEEP(*(.reverie_inguest_fixed_gate_template)) } :ro',
 '  ASSERT(SIZEOF(.gate_reference) == 4096 && (ADDR(.gate_reference) % 4096) == 0, "gate reference ABI differs")',
 '  .mapped_layout : { KEEP(*(.rodata.hermit_mapped_layout)) } :ro',
 '  ASSERT(SIZEOF(.mapped_layout) == 96 && (ADDR(.mapped_layout) % 8) == 0, "target layout ABI differs")',
 '  .rodata : { '+inputs('ro',False)+' } :ro',
 '  .eh_frame : { '+inputs('eh')+' } :ro','  .gcc_except_table : { '+inputs('gcc')+' } :ro',
 '  .got : { '+inputs('got',False)+' } :ro','  .got.plt : { '+inputs('gotplt',False)+' } :ro']
 for name in sorted(groups['notes']):lines.append('  '+name+' : { KEEP(*('+name+')) } :ro')
 lines+=['  . = ALIGN(4096);','  .data : { '+inputs('data',False)+' } :data']
 for group in ('preinit','init','fini'):
  name={'preinit':'preinit_array','init':'init_array','fini':'fini_array'}[group]
  lines.append('  .'+name+' : { __'+name+'_start = .; '+inputs(group)+' __'+name+'_end = .; } :data')
 align=max([r['align'] for r in rows if r['flags']&0x400]+[1])
 lines+=['  . = ALIGN(%d);'%align,'  .tdata : { '+inputs('tdata')+' } :data :tls',
 '  .tbss (NOLOAD) : { '+inputs('tbss')+' } :data :tls','  _edata = .; edata = .;',
 '  .bss (NOLOAD) : { __bss_start = .; '+inputs('bss',False)+' *(COMMON) } :data',
 '  _end = .; end = .;','  ASSERT(ALIGN(.,4096) <= 0x%x, "runtime window overflow")'%END]
 others={r['name'] for r in rows if r['name'] and not r['flags']&2 and r['type'] not in (0,2,3,4,9,17,18)}
 for name in sorted(others):lines.append('  '+name+' 0 : { *('+name+') } :NONE')
 for name in ('.symtab','.shstrtab','.strtab'):
  need(all(not r['flags']&2 for r in rows if r['name']==name),'allocated linker metadata')
  lines.append('  '+name+' 0 : { *('+name+') } :NONE')
 lines.append('}')
 return '\n'.join(lines)+'\n',{'physical_rows':len(rows),'classified':count,'before':before.hexdigest(),'after':after.hexdigest(),'patterns':patterns,'ordinary_first_input':ordinary}


def maintained_module():
 need(sha(MAINTAINED)==MAINTAINED_SHA,'reviewed maintained producer helper changed')
 spec=importlib.util.spec_from_file_location('declared_maintained_source_inputs',MAINTAINED)
 m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m

def main():
 ap=argparse.ArgumentParser(description=__doc__)
 for flag in ('build-result','build-sha256','build-binding','build-recipe','h-head','r-head','attempt','evidence-root','inspection-tools-json','inspection-tools-sha256','memcpy-source','musl-source-authority','stack-chk-source'):
  ap.add_argument('--'+flag,required=True)
 a=ap.parse_args()
 need(re.fullmatch('[a-z0-9][a-z0-9-]{0,31}',a.attempt),'bad attempt')
 need(all(re.fullmatch('[0-9a-f]{40}',v) for v in (a.h_head,a.r_head)),'independent full heads required')
 evidence=Path(a.evidence_root)
 need(evidence.is_absolute() and evidence.resolve()==evidence and evidence.is_dir(),'canonical evidence root required')
 out=evidence/('loader-maintained-final-link-inputs-'+a.attempt);out.mkdir(exist_ok=False)
 result={'state':'RUNNING','scope':'SECTION_SCRIPT_INPUT_PREPARATION_ONLY','link':'NOT_RUN','qualification':'NOT_PROVEN','startup':'NOT_RUN','readiness':'NOT_RUN','guests_executed':0,'tests_executed':0,'commands':[]}
 save(out/'result.json',result);guard=None;produced={};began=time.monotonic();cancelled=None
 try:
  buildpath=Path(a.build_result).resolve();bindingpath=Path(a.build_binding).resolve();recipepath=Path(a.build_recipe).resolve()
  need(all(p.is_relative_to(evidence) for p in (buildpath,bindingpath,recipepath)),'inputs outside owned evidence')
  need(sha(buildpath)==a.build_sha256,'actual maintained receipt changed')
  build=json.loads(buildpath.read_text());c=json.loads(bindingpath.read_text());recipe=json.loads(recipepath.read_text())
  need(build['state']=='ARCHIVE_BUILD_ONLY_NOT_QUALIFIED' and build['source_tools_closed'] is True,'maintained archive/source closure not successful')
  need(build['profile']=='liteinst-runtime' and build['target']=='x86_64-unknown-linux-musl','profile alias or wrong target refused')
  need(build['H_head']==c['H_head']==a.h_head and build['runtime_R_pin']==c['runtime_R_pin']==a.r_head and build['tool_R_head']==c['tool_R_head']==a.r_head,'wrong maintained source cohort')
  need(build['binding_sha256']==recipe['binding_sha256']==sha(bindingpath),'maintained binding/recipe mismatch')
  need(recipe['argv']==[c['python'],str(MAINTAINED),'run','--binding',str(bindingpath),'--binding-sha256',sha(bindingpath)],'not the actual maintained producer command')
  need(c['files'][str(MAINTAINED)]==MAINTAINED_SHA,'not reviewed maintained factory caller')
  need(build['tests_executed']==build['guests_executed']==0,'unexpected execution in archive result')
  maint=maintained_module();helper=module('archive_metadata.py')
  h=Path(c['H']);archive=Path(build['archive']['path'])
  need(c['target']==str(h/'target'),'not sole H target')
  need(archive==h/'target/x86_64-unknown-linux-musl/liteinst-runtime/libdetcore_liteinst.a','not actual maintained archive output')
  need(identity(archive)==build['archive'],'actual maintained archive identity changed')
  files=dict(c['files']);files.update({str(p):sha(p) for p in (buildpath,bindingpath,recipepath,Path(__file__).resolve())})
  files.update({str(ASSETS/n):digest for n,digest in HELPERS.items()})
  for command in build['commands']:
   need(command['exit']==0,'failed/incomplete maintained producer command')
   for stream in ('stdout','stderr'):
    fact=command[stream];need(identity(fact['path'])==fact,'maintained command output changed');files[fact['path']]=fact['sha256']
  files[str(archive)]=build['archive']['sha256']
  # Join the actual factory metadata and Cargo JSON streams by their exact
  # returned digests. This records evidence; it never constructs StaticArchive.
  records=[];commands=[]
  for path in sorted((buildpath.parent/'runner').glob('*.command.json')):
   command=json.loads(path.read_text());commands.append((path,command))
  metadata=[(p,q) for p,q in commands if q['stdout']['sha256']==build['metadata_sha256']]
  cargo=[(p,q) for p,q in commands if q['stdout']['sha256']==build['cargo_messages_sha256']]
  need(len(metadata)==len(cargo)==1,'factory metadata/Cargo stream absent or ambiguous')
  features='exec-native-injection,exec-private-init,loader-private-runtime'
  expected=['rustc','--locked','--offline','--manifest-path',str(h/'detcore-liteinst/Cargo.toml'),'--lib','--target','x86_64-unknown-linux-musl','--profile','liteinst-runtime','--features',features,'--crate-type','staticlib','--message-format=json-render-diagnostics','--target-dir',c['target']]
  for path,q in metadata+cargo:
   need(q['program']==c['cargo'] and q['status']==0 and q['cwd']==str(h),'factory command context differs')
   files[str(path)]=sha(path)
   for stream in ('stdout','stderr'):
    fact=q[stream];need(sha(fact['path'])==fact['sha256'],'factory raw stream changed');files[fact['path']]=fact['sha256']
  need(cargo[0][1]['arguments']==expected,'not maintained build_archive command/profile')
  md=json.loads(Path(metadata[0][1]['stdout']['path']).read_text())
  need(md['target_directory']==c['target'],'factory metadata selected another target')
  package=[p for p in md['packages'] if p['name']=='detcore-liteinst' and p['manifest_path']==str(h/'detcore-liteinst/Cargo.toml') and p['source'] is None]
  need(len(package)==1,'actual root package ambiguous')
  records=[json.loads(l) for l in Path(cargo[0][1]['stdout']['path']).read_text().splitlines() if l.strip()]
  need([q['success'] for q in records if q.get('reason')=='build-finished']==[True],'actual Cargo build did not finish exactly once')
  roots=[q for q in records if q.get('reason')=='compiler-artifact' and q.get('package_id')==package[0]['id'] and q.get('manifest_path')==package[0]['manifest_path'] and q['target']['kind']==q['target']['crate_types']==['staticlib']]
  need(len(roots)==1,'actual maintained staticlib selection ambiguous')
  root=roots[0]
  need(root['target']['name']=='detcore_liteinst' and root['target']['src_path']==str(h/'detcore-liteinst/src/lib.rs') and root['target']['edition']=='2024' and root['features']==features.split(',') and root['profile']=={'opt_level':'3','debuginfo':0,'debug_assertions':True,'overflow_checks':True,'test':False} and root['executable'] is None and root['filenames']==[str(archive)],'actual maintained target/feature/codegen result differs')
  # Bind every emitted runner record/log, including factory cache/source close
  # commands. Existing producer already checked their successful lifecycle.
  for path,q in commands:
   need(q['status']==0,'retained factory Runner command failed')
   files[str(path)]=sha(path)
   for stream in ('stdout','stderr'):
    fact=q[stream];need(sha(fact['path'])==fact['sha256'],'retained factory Runner stream changed');files[fact['path']]=fact['sha256']
  toolfile=Path(a.inspection_tools_json)
  need(sha(toolfile)==a.inspection_tools_sha256=='b5b75a7dbf58d0805bbfcfb8d1fc17e55b1c528a2d2d210d5def39ea559d97a0','original actual tool inventory changed')
  files[str(toolfile)]=sha(toolfile);tools=json.loads(toolfile.read_text())['tools']
  for fact in tools.values():files[fact['path']]=fact['sha256']
  prefix=Path(c['private_inputs']['toolchain']['prefix']);inventory=json.loads(Path(c['private_inputs']['toolchain']['input_inventory']).read_text())
  lld=prefix/'lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld'
  files[str(lld)]='eb9b9bc681e57f7ff4166f8fb57fad7336d29a51c85e8c70ba1ca203600e392b'
  def check():
   maint.files_close(c)
   need(maint.private_close(c)==build['private_before']==build['private_after'],'private source/compiler/native inputs changed')
   for path,digest in files.items():need(sha(path)==digest,'bound input changed: '+path)
   for path,fact in produced.items():need(identity(path)==fact,'prepared output changed: '+path)
  guard=check;guard();save(out/'source-before.json',c['source_before'])
  env=maint.declared_child_environment(c)
  def run(name,argv):
   limit=RELOCS if name.endswith('-relocations') else LIMIT
   row={'name':name,'argv':argv,'state':'RUNNING','output_limit_bytes':limit};result['commands'].append(row);save(out/'result.json',result);started=time.monotonic()
   command_cancelled=None
   def close_stream(stream):
    nonlocal command_cancelled
    try:stream.close()
    except BaseException as error:command_cancelled=cancellation(command_cancelled,error);raise
   try:
    guard()
    with ExitStack() as streams:
     try:
      o=(out/(name+'.stdout')).open('xb');streams.callback(close_stream,o)
      e=(out/(name+'.stderr')).open('xb');streams.callback(close_stream,e)
      child=subprocess.run(argv,cwd=h,env=env,stdin=subprocess.DEVNULL,stdout=o,stderr=e,timeout=120,preexec_fn=lambda:resource.setrlimit(resource.RLIMIT_FSIZE,(limit,limit)))
     except BaseException as error:command_cancelled=cancellation(command_cancelled,error);raise
    row['exit']=child.returncode;need(child.returncode==0,'metadata tool failed: '+name);row['state']='PASS'
   except BaseException as error:command_cancelled=cancellation(command_cancelled,error);row.update(state='FAIL',error=repr(error));raise
   finally:
    try:
     row['wall_seconds']=time.monotonic()-started
     for stream in ('stdout','stderr'):
      path=out/(name+'.'+stream)
      if path.exists():
       row[stream]=identity(path);produced[str(path)]=row[stream]
       if row[stream]['bytes']>limit:row.update(state='FAIL',output_error='metadata output cap exceeded')
     try:
      guard()
      need('output_error' not in row,'metadata output cap exceeded')
     except BaseException as error:command_cancelled=cancellation(command_cancelled,error);row.update(state='FAIL',custody_error=repr(error));raise
     finally:save(out/'result.json',result)
    finally:
     if command_cancelled is not None:raise command_cancelled
   return out/(name+'.stdout')
  native=[];archives=[('detcore',archive)]
  for label,name,size,digest in [('libc','libc.a',6406314,'e699c64b0c6b427d89ad5bf767bf78beb30c19930c5dedff635951636e8f9f70'),('libunwind','libunwind.a',201418,'eb16569e611a5d6cdda2e7604fd6402a3ad810817dd8d63ed50baa2ce8070aaa')]:
   relative='lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/'+name;path=prefix/relative
   need(inventory['added_target_inputs'][relative]=={'bytes':size,'sha256':digest} and identity(path)=={'path':str(path),'bytes':size,'sha256':digest},'native provider differs')
   native.append(identity(path));files[str(path)]=digest;archives.append((label,path))
  rows=[];objects=[];symbols=[];helper.ROOT=ROOT
  for label,path in archives:
   prior=helper.symbol_observations(symbols) if symbols else None
   members=run(label+'-members',[tools['ar']['path'],'t',str(path)]).read_text().splitlines()
   run(label+'-headers',[tools['readelf']['path'],'-hW',str(path)])
   run(label+'-defined',[tools['nm']['path'],'-A','--defined-only',str(path)])
   run(label+'-unresolved',[tools['nm']['path'],'-A','-u',str(path)])
   run(label+'-relocations',[tools['readelf']['path'],'-rW',str(path)])
   actual,physical,definitions=helper.allocated_inventory(path,members)
   if prior:need({x['name'] for x in definitions if x['section_index']!=0}&set(prior['additional_native_inputs_required']),'native input not justified by real preceding symbols')
   rows+=actual;objects+=physical;symbols.append((path,physical,definitions));save(out/(label+'-actual-symbols.json'),definitions)
  save(out/'actual-sections.json',rows);save(out/'actual-physical-objects.json',objects);save(out/'closure.json',helper.symbol_observations(symbols))
  text,grouping=script(rows,helper,symbols,objects,archive);scriptpath=out/'runtime.ld';scriptpath.write_text(text);save(out/'grouping.json',grouping)
  paths={'lld':str(lld),'script':str(scriptpath),'libc':native[0]['path'],'supporting_archives':[native[1]['path']],
         'memcpy_source':a.memcpy_source,
         'musl_source_authority':a.musl_source_authority,
         'stack_chk_source':a.stack_chk_source,
         'output_directory':str(h/'target'/('loader-maintained-final-link-'+a.attempt))}
  # These paths remain fallible input coordinates, not a typed owner/proof.
  # Native method authenticates the actual source recipe before role joining.
  for key,digest in [('memcpy_source','790bdd75f7736c83ed7a21f8813b20454ead1ccfc49e806a88692d2a471024eb'),('stack_chk_source','2ea4d8cf6758daddecf8973913ac196ffa76fdb355cea391ca890aeafdc2ac6c')]:
   need(identity(paths[key])['sha256']==digest,'actual native source prerequisite changed: '+key);files[paths[key]]=digest
  authority=Path(paths['musl_source_authority']);native_inventory=authority/'inventory.json'
  need(sha(native_inventory)=='6aeea4469c8ba49c4c475a6694972e6a1ef3974954c9f986f99d7d1bce926275','native source authority inventory differs')
  files[str(native_inventory)]=sha(native_inventory)
  for row in json.loads(native_inventory.read_text())['files']:
   relative=Path(row['path']);need(not relative.is_absolute() and all(p not in ('..','.') for p in relative.parts),'native source path escape')
   source=authority/relative
   need(source.resolve()==source and identity(source)=={'path':str(source),'bytes':row['bytes'],'sha256':row['sha256']},'native source authority changed')
   files[str(source)]=row['sha256']
  files[str(scriptpath)]=sha(scriptpath)
  save(out/'explicit-roots.json',dict(entry=ROOT,existing_ssp_root='__stack_chk_fail',private_abi_symbols=ABI_ROOTS,host_ro_sections=RO_ROOTS,ordinary_gc_groups=['ro','data','bss'],unchanged_keep_groups=['gate','eh','gcc','notes','preinit','init','fini','tdata','tbss']))
  # No link.rsp is executed or accepted as an argv override. The native method
  # owns the exact fixed V12 command and this fresh sole-target output path.
  prepared={'state':'PREPARED_PATH_INPUTS_ONLY_NOT_STATIC_ARCHIVE_OR_QUALIFICATION','actual_maintained_build_result':identity(buildpath),'actual_factory_archive':build['archive'],'metadata_sha256':build['metadata_sha256'],'cargo_messages_sha256':build['cargo_messages_sha256'],'selected_actual_cargo_target':root,'source':{'H':a.h_head,'R':a.r_head},'typed_method_path_inputs':paths,'bound_files_sha256':files,'one_link_owner':'VerifiedStaticSource::link_and_join_initializer_definitions with actual fresh source.build_archive result','link':'NOT_RUN','inspection':'NOT_RUN','qualification':'NOT_PROVEN','startup':'NOT_RUN','guest_release':'NOT_RUN'}
  save(out/'prepared-link-inputs.json',prepared)
  # Bind every generated evidence file, not just the raw command streams.
  # Mutable result.json is the outer transaction journal and is excluded.
  for path in out.iterdir():
   if path.is_file() and path.name!='result.json':produced[str(path)]=identity(path)
  guard()
  result.update(state='PASS_SECTION_SCRIPT_PREPARATION_ONLY',prepared_inputs=identity(out/'prepared-link-inputs.json'),archive=build['archive'],h_head=a.h_head,r_head=a.r_head)
 except BaseException as error:result.update(state='FAIL',error=repr(error));cancelled=cancellation(cancelled,error);raise
 finally:
  try:
   if guard:
    try:guard();save(out/'source-after.json',c['source_before']);result['source_and_inputs_unchanged']=True
    except Exception as error:result.update(state='FAIL',custody_error=repr(error),source_and_inputs_unchanged=False)
    except BaseException as error:
     cancelled=cancellation(cancelled,error)
     result.update(state='FAIL',custody_error=repr(error),source_and_inputs_unchanged=False)
     raise
  finally:
   result['produced_outputs']=produced;result['wall_seconds']=time.monotonic()-began
   try:save(out/'result.json',result)
   finally:
    if cancelled is not None:raise cancelled
 return 0 if result['state']=='PASS_SECTION_SCRIPT_PREPARATION_ONLY' else 1
if __name__=='__main__':raise SystemExit(main())
