"""Existing bounded metadata helpers; no standalone historical CLI."""
import hashlib,json,os,subprocess


from pathlib import Path


def need(ok,message):
    if not ok: raise RuntimeError(message)


def git(path,*args):
    return subprocess.check_output(['git','-c','diff.autoRefreshIndex=false','-C',str(path),*args],env=dict(os.environ,GIT_OPTIONAL_LOCKS='0',GIT_NO_LAZY_FETCH='1'))


def tracked_source(path,expected_head,cargo_cache=False):
    path=Path(path)
    need(not path.is_symlink(),'source root symlink: '+str(path))
    need(Path(git(path,'rev-parse','--show-toplevel').decode().strip()).resolve()==path.resolve(),'source is not own tree: '+str(path))
    need(git(path,'rev-parse','HEAD').decode().strip()==expected_head,'source head changed: '+str(path))
    # Also reject staged/index changes; cache Cargo markers do not affect index.
    need(git(path,'diff','--cached','--name-only','HEAD')==b'','source index differs from HEAD: '+str(path))
    entries=[]
    for entry in git(path,'ls-tree','-rz','--full-tree','HEAD').split(b'\0'):
        if not entry: continue
        header,raw=entry.split(b'\t',1);mode,kind,oid=header.split();name=os.fsdecode(raw);child=path/name
        if kind==b'commit':
            need(not child.is_symlink(),'gitlink symlink: '+str(child))
            if (child/'.git').exists(): binding=tracked_source(child,oid.decode(),cargo_cache)
            else:
                need(not child.exists() or (child.is_dir() and not any(child.iterdir())),'nonempty uninitialized gitlink: '+str(child))
                binding={'uninitialized_gitlink':oid.decode()}
            entries.append([name,mode.decode(),binding]);continue
        if mode==b'120000':
            need(child.is_symlink(),'tracked symlink changed: '+str(child));payload=os.fsencode(os.readlink(child))
        else:
            need(mode in (b'100644',b'100755') and child.is_file() and not child.is_symlink(),'tracked file missing/indirect: '+str(child))
            need(bool(child.stat().st_mode&0o111)==(mode==b'100755'),'tracked executable mode changed: '+str(child))
            payload=child.read_bytes()
        actual=hashlib.sha1(b'blob '+str(len(payload)).encode()+b'\0'+payload).hexdigest()
        need(actual==oid.decode(),'tracked blob changed: '+str(child))
        entries.append([name,mode.decode(),len(payload),hashlib.sha256(payload).hexdigest()])
    extras=[os.fsdecode(x) for x in git(path,'ls-files','--others','--exclude-standard','-z').split(b'\0') if x]
    if cargo_cache and extras==['.cargo-ok']:
        marker=path/'.cargo-ok'
        need(marker.is_file() and not marker.is_symlink() and marker.read_bytes()==b'','invalid Cargo completion marker')
    else: need(extras==[],'extra source files: '+str(path)+' '+repr(extras))
    return {'head':expected_head,'tree':git(path,'rev-parse','HEAD^{tree}').decode().strip(),'files':entries,'untracked_empty_cargo_marker':extras==['.cargo-ok']}


def full_pins(h,rpin):
    # Active textual pin occurrences in every tracked manifest/lock/BUCK and
    # budget binding; historical CARRY comments are deliberately not rewritten.
    h=Path(h);rows=[]
    import re
    for raw in git(h,'ls-tree','-rz','--full-tree','HEAD').split(b'\0'):
        if not raw: continue
        header,name=raw.split(b'\t',1);mode,kind,oid=header.split();name=os.fsdecode(name)
        if kind!=b'blob' or mode==b'120000': continue
        text=(h/name).read_bytes()
        if b'reverie' not in text.lower(): continue
        for n,line in enumerate(text.decode('utf-8',errors='replace').splitlines(),1):
            s=line.strip()
            if s.startswith(('#','//','/*','*','<!--')): continue
            # Files outside manifests may contain code literals or historical
            # receipts. Restrict authority to active input formats.
            active=(name.endswith(('Cargo.toml','Cargo.lock')) or Path(name).name=='BUCK' or 'budget' in name.lower() and name.endswith(('.toml','.json','.sh')))
            if not active: continue
            if 'budget' in name.lower() and re.match(r'expected_pin\s*=',s):
                pins=re.findall(r'[0-9a-f]{40}',s)
                need(pins==[rpin],'budget pin differs: '+name+':'+str(n));rows.append([name,n,line])
            if 'github.com/rrnewton/reverie' in line:
                pins=re.findall(r'(?:rev[=\s"\x27:]+|#)([0-9a-f]{40})',line)
                need(pins and all(p==rpin for p in pins),'active Reverie pin differs/missing: '+name+':'+str(n))
                rows.append([name,n,line])
    need(any(x[0].endswith('Cargo.toml') for x in rows) and any(x[0]=='Cargo.lock' for x in rows),'root manifest/lock pin authority missing')
    return rows
