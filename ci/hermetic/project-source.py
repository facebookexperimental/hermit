#!/usr/bin/env python3
"""Bind the paired pinned-root source paths; never substitute for prepared Cargo proof."""
import hashlib, json, os, stat, subprocess, sys
from pathlib import Path
LOCATION = ('GIT_DIR','GIT_WORK_TREE','GIT_INDEX_FILE','GIT_COMMON_DIR','GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES','GIT_NAMESPACE','GIT_PREFIX')
def identity(path,limit=65536):
    if path.is_symlink(): raise ValueError('source binding file is a symlink')
    with path.open('rb') as source:
        before=os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size>limit: raise ValueError('source binding input is not bounded regular data')
        data=source.read();after=os.fstat(source.fileno())
    current=path.stat()
    fields=lambda s:[s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns]
    if fields(before)!=fields(after) or fields(after)!=fields(current): raise ValueError('source binding input changed')
    return {'path':str(path),'stat':fields(after),'sha256':hashlib.sha256(data).hexdigest()},data
def git(root,*args):
    env={k:v for k,v in os.environ.items() if k not in LOCATION};env['GIT_OPTIONAL_LOCKS']='0'
    return subprocess.run(['git','-C',str(root),*args],env=env,check=True,capture_output=True,text=True,timeout=10).stdout.strip()
def closed(pairs):
    result={}
    for key,value in pairs:
        if key in result: raise ValueError('duplicate source descriptor field '+key)
        result[key]=value
    return result

def snapshot(core,project,descriptor):
    if core==project or core in project.parents or project in core.parents: raise ValueError('project is not distinct')
    source=[]
    for path,guest in ((core,'/src'),(project,'/project')):
        if Path(git(path,'rev-parse','--show-toplevel')).resolve(strict=True)!=path: raise ValueError('source is not the exact Git workspace root')
        source.append({'host':str(path),'guest':guest,'head':git(path,'rev-parse','HEAD'),'tree':git(path,'rev-parse','HEAD^{tree}')})
    if project not in descriptor.parents: raise ValueError('descriptor is outside project')
    ident,data=identity(descriptor);obj=json.loads(data,object_pairs_hook=closed)
    if set(obj)!={'schema','repository','graph','tools_root','shared_package'} or type(obj['schema']) is not int or obj['schema']!=1 or obj['repository']!='/project' or obj['tools_root']!='/src' or not isinstance(obj['shared_package'],str) or not obj['shared_package']:
        raise ValueError('project descriptor must use the original /src tools and distinct /project root')
    graph_guest=Path(obj['graph'])
    if not graph_guest.is_absolute() or '..' in graph_guest.parts: raise ValueError('invalid project graph path')
    relative=graph_guest.relative_to('/project')
    unresolved=project/relative
    component=project
    for part in relative.parts:
        component=component/part
        if component.is_symlink(): raise ValueError('project graph has a symlink component')
    graph=unresolved.resolve(strict=True)
    if project not in graph.parents: raise ValueError('project graph escapes its source')
    git(project,'ls-files','--error-unmatch','--',str(graph.relative_to(project)))
    graph_ident,_=identity(graph,16*1024*1024)
    return {'schema':1,'sources':source,'descriptor':ident,'descriptor_guest':'/project/'+str(descriptor.relative_to(project)),'graph':graph_ident}
def main():
    if len(sys.argv)!=6 or sys.argv[1] not in ('prepare','check'): raise ValueError('usage: project-source.py prepare|check CORE PROJECT DESCRIPTOR RECEIPT')
    action,core,project,descriptor,receipt=sys.argv[1:]
    obj=snapshot(Path(core).resolve(strict=True),Path(project).resolve(strict=True),Path(descriptor).resolve(strict=True))
    receipt=Path(receipt)
    if action=='prepare':
        fd=os.open(receipt,os.O_WRONLY|os.O_NOFOLLOW|os.O_TRUNC|os.O_CLOEXEC)
        with os.fdopen(fd,'w') as output: json.dump(obj,output,sort_keys=True);output.write('\n');output.flush();os.fsync(output.fileno())
        receipt.chmod(0o400)
    else:
        old,_=identity(receipt)
        if json.loads(receipt.read_bytes())!=obj: raise ValueError('source, descriptor or tracked graph changed during mount preparation')
if __name__=='__main__':
    try: main()
    except (OSError,ValueError,subprocess.SubprocessError) as error: print('project source: '+str(error),file=sys.stderr);sys.exit(2)
