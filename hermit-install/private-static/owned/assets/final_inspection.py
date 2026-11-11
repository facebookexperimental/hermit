"""Existing bounded metadata helpers; no standalone historical CLI."""
import argparse, fnmatch, hashlib, importlib.util, json, os, re, resource, struct, subprocess, sys, time


from pathlib import Path


B = Path(__file__).resolve().parent


ROOT='detcore_liteinst_mapped_entry'


HELPERS={'elf_metadata.py': 'c6fe4a6db783ca4af300e5cfbf595e545b17e746cd8594eccbf2ecb7c81215d5'}


def need(ok,why):
 if not ok: raise RuntimeError(why)


def sha(path):
 h=hashlib.sha256()
 with Path(path).open('rb') as f:
  for part in iter(lambda:f.read(1024*1024),b''): h.update(part)
 return h.hexdigest()


def save(path,value):
 q=path.with_name(path.name+'.pending');q.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n');q.replace(path)


def module(name):
 path=B/name;need(sha(path)==HELPERS[name],'helper changed: '+name)
 spec=importlib.util.spec_from_file_location(name.replace('.','_'),path);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m


def identity(path):
 path=Path(path);need(path.is_file() and not path.is_symlink(),'not regular: '+str(path))
 return {'path':str(path),'bytes':path.stat().st_size,'sha256':sha(path)}


class ElfEvidence:
 def __init__(self,data):
  self.data=data
  need(len(data)>=64 and data[:7]==b'\x7fELF\x02\x01\x01','diagnostic requires ELF64LE')
  need(struct.unpack_from('<H',data,18)[0]==62,'diagnostic requires x86-64')
  self.kind=struct.unpack_from('<H',data,16)[0]
  shoff=struct.unpack_from('<Q',data,40)[0];entsize,count,names=struct.unpack_from('<HHH',data,58)
  need(entsize==64 and shoff+64<=len(data),'diagnostic section header shape')
  zero=struct.unpack_from('<IIQQQQIIQQ',data,shoff)
  if count==0:count=zero[5]
  if names==0xffff:names=zero[6]
  need(0<count<=1000000 and names<count and shoff+count*64<=len(data),'diagnostic section bounds')
  raw=[struct.unpack_from('<IIQQQQIIQQ',data,shoff+i*64) for i in range(count)]
  def span(offset,size):
   need(offset+size<=len(data),'diagnostic data span outside actual ELF')
   return data[offset:offset+size]
  self.span=span
  strings=span(raw[names][4],raw[names][5])
  self.sections=[]
  for index,s in enumerate(raw):
   if s[1]!=8:span(s[4],s[5])
   self.sections.append(dict(index=index,name=self.string(strings,s[0]),type=s[1],flags=s[2],address=s[3],offset=s[4],size=s[5],link=s[6],info=s[7],align=s[8],entsize=s[9]))
  self.tables={};self.symbols=[]
  for section in self.sections:
   if section['type']!=2:continue
   need(section['entsize']==24 and section['size']%24==0 and section['link']<count,'diagnostic symtab shape')
   st=self.sections[section['link']];strings=span(st['offset'],st['size']);symbols=[]
   extended=[s for s in self.sections if s['type']==18 and s['link']==section['index']]
   need(len(extended)<=1,'diagnostic ambiguous extended symbol sections')
   for index in range(section['size']//24):
    name,info,other,si,value,size=struct.unpack_from('<IBBHQQ',data,section['offset']+index*24)
    if si==0xffff:
     need(len(extended)==1 and index*4+4<=extended[0]['size'],'diagnostic missing extended symbol index')
     si=struct.unpack_from('<I',data,extended[0]['offset']+index*4)[0]
    need(si<count or si in (0xfff1,0xfff2),'diagnostic invalid symbol section')
    symbol=dict(name=self.string(strings,name),binding=info>>4,type=info&15,visibility=other,section_index=si,value=value,size=size,table_index=section['index'],symbol_index=index)
    symbols.append(symbol)
   self.tables[section['index']]=symbols;self.symbols+=symbols
  need(len(self.tables)==1,'diagnostic requires one actual full static symbol table')
 @staticmethod
 def string(data,offset):
  need(offset<len(data),'diagnostic string offset')
  end=data.find(b'\0',offset);need(end>=0,'diagnostic unterminated string')
  return data[offset:end].decode('utf-8','strict')
 def relocations(self):
  for section in self.sections:
   if section['type']!=4:continue
   need(section['entsize']==24 and section['size']%24==0 and section['link'] in self.tables,'diagnostic RELA shape')
   table=self.tables[section['link']]
   for index in range(section['size']//24):
    offset,info,addend=struct.unpack_from('<QQq',self.data,section['offset']+index*24)
    need(info>>32<len(table),'diagnostic RELA symbol index')
    symbol=table[info>>32]
    yield dict(offset=offset,type=info&0xffffffff,addend=addend,symbol=symbol,relocation_section=section['name'],target_section=section['info'])
 def function_bytes(self,symbol):
  need(0<symbol['size']<=256*1024 and symbol['type']==2 and 0<symbol['section_index']<len(self.sections),'diagnostic bounded actual function required')
  section=self.sections[symbol['section_index']]
  need(section['flags']&4 and not section['flags']&1,'diagnostic function not immutable RX section')
  relative=symbol['value']-(section['address'] if self.kind==2 else 0)
  need(0<=relative and relative+symbol['size']<=section['size'],'diagnostic function outside section')
  return self.span(section['offset']+relative,symbol['size'])


def member_evidence(identity):
 path=Path(identity['archive']);offset=identity['header_offset']
 with path.open('rb') as f:
  need(f.read(8)==b'!<arch>\n','diagnostic requires physical ordinary archive')
  f.seek(offset);header=f.read(60);need(len(header)==60 and header[58:]==b'`\n','diagnostic archive header identity')
  size=int(header[48:58]);need(0<size<=512*1024*1024,'diagnostic member bound')
  body=f.read(size);need(len(body)==size,'diagnostic truncated physical member')
  if header[:16].strip().startswith(b'#1/'):
   length=int(header[:16].strip()[3:]);need(length<size,'diagnostic BSD member name bound');body=body[length:]
 return ElfEvidence(body)


def final_diagnostics(out,target,header,symbol_inputs,run,objdump,produced):
 need(target.stat().st_size<=512*1024*1024,'diagnostic final ELF exceeds original bound')
 elf=ElfEvidence(target.read_bytes())
 need(elf.kind==2,'diagnostic requires ET_EXEC')
 def emit(name,value):
  p=out/name;save(p,value);produced[str(p)]=identity(p);return p
 def raw(name,bytes):
  p=out/name;p.write_bytes(bytes);produced[str(p)]=identity(p);return identity(p)
 def mapped(address,length):
  matches=[s for s in header['segments'] if s['type']==1 and s['vaddr']<=address and address+length<=s['vaddr']+s['filesz']]
  need(len(matches)==1,'diagnostic final file address ambiguous or absent')
  s=matches[0];return elf.span(s['offset']+address-s['vaddr'],length),s
 got=[]
 for section in elf.sections:
  if section['name'] in ('.got','.got.plt') and section['size']:
   _,load=mapped(section['address'],section['size']);got.append(dict(section=section,load=load))
 emit('got-segment-diagnostics.json',got)
 need(got and all(row['load']['flags']==4 for row in got),'final GOT is not actual immutable RO PT_LOAD')
 roles=['__init_tls','__init_ssp','__stack_chk_fail','__copy_tls','__init_tp']
 inputs=[record for _,_,records in symbol_inputs for record in records]
 functions=[s for s in elf.symbols if s['type']==2 and s['section_index']!=0 and s['size']]
 final_relocations=list(elf.relocations())
 role_evidence=[];code_ranges={};member_cache={}
 for name in roles:
  candidates=[s for s in inputs if s['name']==name and s['section_index']!=0]
  finals=[s for s in elf.symbols if s['name']==name and s['section_index']!=0]
  record=dict(name=name,input_definitions=candidates,final_definitions=finals,status='DIAGNOSTIC_NOT_SOURCE_OR_MACHINE_QUALIFICATION',source_functions=[])
  for n,definition in enumerate(candidates):
   if definition['type']!=2 or not definition['size']:continue
   owner=definition['object'];key=(owner['archive'],owner['ordinal'],owner['header_offset'])
   if key not in member_cache:member_cache[key]=member_evidence(owner)
   source=member_cache[key];exact=[s for s in source.symbols if all(s[k]==definition[k] for k in ('name','binding','type','visibility','section_index','value','size','table_index','symbol_index'))]
   need(len(exact)==1,'diagnostic source member symbol join differs')
   definition_bytes=source.function_bytes(exact[0]);start=definition['value'];end=start+definition['size']
   relocs=[rel for rel in source.relocations() if rel['target_section']==definition['section_index'] and start<=rel['offset']<end]
   body=raw('role-'+name+'-input-'+str(n)+'.bin',definition_bytes)
   record['source_functions'].append(dict(object=owner,symbol=exact[0],body=body,relocations=relocs))
  for n,symbol in enumerate(finals):
   if symbol['type']!=2 or not symbol['size']:continue
   body=raw('role-'+name+'-final-'+str(n)+'.bin',elf.function_bytes(symbol))
   start=symbol['value'];end=start+symbol['size'];key=(start,end)
   code_ranges.setdefault(key,[]).append(name)
   record.setdefault('final_functions',[]).append(dict(symbol=symbol,body=body,relocations=[rel for rel in final_relocations if start<=rel['offset']<end]))
  role_evidence.append(record)
 emit('native-role-source-member-final-diagnostics.json',role_evidence)
 # These are selector/source-provider checks, not final-machine equivalence.
 for name in ('__init_tls','__init_ssp','__stack_chk_fail'):
  row=next(r for r in role_evidence if r['name']==name)
  strong=[s for s in row['input_definitions'] if s['binding']==1 and s['type']==2]
  final=[s for s in row['final_definitions'] if s['type']==2]
  need(len(strong)==1 and len(final)==1 and final[0]['size']==strong[0]['size'],'actual strong role selection/size not established: '+name)
 ssp=next(r for r in role_evidence if r['name']=='__init_ssp')
 fail=next(r for r in role_evidence if r['name']=='__stack_chk_fail')
 need([s['object'] for s in ssp['input_definitions'] if s['binding']==1]==[s['object'] for s in fail['input_definitions'] if s['binding']==1],'SSP strong providers not same actual libc member')
 undefined=[s for s in elf.symbols if s['name'] and s['section_index']==0]
 referenced=[]
 for rel in final_relocations:
  if rel['symbol']['section_index']!=0 or not rel['symbol']['name']:continue
  row=dict(relocation=rel)
  owners=[s for s in functions if s['value']<=rel['offset']<s['value']+s['size']]
  row['actual_function_owners']=owners
  for owner in owners:code_ranges.setdefault((owner['value'],owner['value']+owner['size']),[]).append(owner['name'])
  if rel['type'] in (9,41,42):
   displacement,load=mapped(rel['offset'],4)
   slot=rel['offset']+int.from_bytes(displacement,'little',signed=True)-rel['addend']
   content,slot_load=mapped(slot,8)
   row.update(actual_got_address=slot,actual_got_value=int.from_bytes(content,'little'),slot_load=slot_load,operand_load=load)
  referenced.append(row)
 emit('actual-undefined-relocation-owner-diagnostics.json',dict(undefined_symbols=undefined,references=referenced,guard_proof='NOT_PROVEN',policy='No name allowlist; even weak-zero observations do not satisfy final unresolved gate.'))
 # This includes the actual _DYNAMIC owner derived above, not a guessed TLS caller.
 need(len(code_ranges)<=128 and sum(hi-lo for lo,hi in code_ranges)<=4*1024*1024,'bounded disassembly plan exceeds diagnostic cap')
 plan=[]
 for index,((lo,hi),names) in enumerate(sorted(code_ranges.items())):
  need(0<hi-lo<=256*1024,'actual diagnostic function exceeds per-role cap')
  name='machine-role-'+str(index).zfill(3)
  run(name,[str(objdump),'-d','--start-address='+hex(lo),'--stop-address='+hex(hi),str(target)],60)
  plan.append(dict(command=name,names=sorted(set(names)),start=lo,end=hi))
 emit('bounded-disassembly-plan.json',plan)
 return dict(roles='DIAGNOSTICS_CAPTURED_NOT_QUALIFIED',got='RO_PT_LOAD',optional_static_zero_guard_proof='NOT_PROVEN',undefined_count=len(undefined),disassembly_ranges=len(plan))
