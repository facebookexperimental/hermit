#!/usr/bin/env python3
"""One maintained private-static build/section/link/DATA-package pipeline."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent


def need(value, why):
    if not value:
        raise RuntimeError(why)


def cancellation(pending, error):
    """Keep the first KeyboardInterrupt/SystemExit to re-raise after every close."""
    return pending if pending is not None or isinstance(error, Exception) else error


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def save(path, value):
    with path.open('x') as output:
        json.dump(value,output,indent=2,sort_keys=True)
        output.write('\n')


def source_files(expected):
    path = D / 'source-files.json'
    need(sha(path) == expected, 'independently supplied producer source manifest differs')
    rows = json.loads(path.read_text())
    result = {str(path):expected}
    for row in rows:
        p = Path(row['path'])
        need(not p.is_absolute() and p.parts and all(v not in ('.','..') for v in p.parts),
             'producer source manifest path escape')
        full = D / p
        need(full.resolve() == full and sha(full) == row['sha256'], 'producer source changed: '+str(p))
        need(str(full) not in result, 'producer source manifest duplicate')
        result[str(full)] = row['sha256']
    return result


def module(relative, sources):
    path = D / relative
    need(sources.get(str(path)) == sha(path), 'unbound maintained module: '+relative)
    spec = importlib.util.spec_from_file_location(relative.replace('/','_').replace('.','_'),path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def invoke(value, args):
    previous = sys.argv
    try:
        sys.argv = [str(value.__file__),*args]
        status = value.main()
    finally:
        sys.argv = previous
    # A stage reports failure by an exit status as well as by raising.
    need(status is None or status == 0, 'pipeline stage returned failure status: '+repr(status))


def stage_output(root, stage):
    # Existing stage helpers require a fresh basename below the sole Cargo
    # target. Bind that basename to this complete evidence root, so independent
    # attempts never share the old fixed "archive" or "owned" helper directory.
    need(stage in ('archive', 'owned'), 'unknown pipeline stage')
    return root / (stage + '-' + hashlib.sha256(str(root).encode()).hexdigest())


def validate_config(c):
    need(set(c) == {'schema','paths','heads','digests','data_roles'} and
         c['schema'] == 'hermit.private-static-producer-inputs.v1', 'closed producer config shape')
    need(set(c['heads']) == {'H','runtime_R','tool_R'} and
         all(re.fullmatch('[0-9a-f]{40}',v) for v in c['heads'].values()), 'full source heads required')
    names = {'H','runtime_R','tool_R','cargo','rustc','host_cargo','host_rustc','target','cargo_home',
             'runner','native_inputs_json','inspection_tools_json','musl_source_archive',
             'memcpy_source','musl_source_authority','stack_chk_source',
             'optional_std_weak_source','optional_std_destructors_source','optional_musl_dl_iterate_source'}
    need(set(c['paths']) == names, 'closed producer path config shape')
    for name,value in c['paths'].items():
        path = Path(value)
        need(path.is_absolute() and path.resolve() == path and path.exists(), 'canonical existing input: '+name)
    needed = {'native_inputs','inspection_tools','runner','initializer_module','tls_source_module',
              'native_leaf_module','ordinary_gate_module','terminal_module','view_site_module','view_table_module'}
    need(set(c['digests']) == needed and
         all(re.fullmatch('[0-9a-f]{64}',v) for v in c['digests'].values()), 'closed expected digest config')
    for role,name in [('native_inputs','native_inputs_json'),('inspection_tools','inspection_tools_json'),('runner','runner')]:
        need(sha(c['paths'][name]) == c['digests'][role], 'explicit native/tool input differs: '+role)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='mode',required=True)
    p = sub.add_parser('prepare')
    for name in ['config','config-sha256','source-manifest-sha256','evidence-root','name','publish-directory']:
        p.add_argument('--'+name,required=True)
    p = sub.add_parser('run')
    p.add_argument('--binding',required=True)
    p.add_argument('--binding-sha256',required=True)
    args = parser.parse_args()
    if args.mode == 'prepare':
        sources = source_files(args.source_manifest_sha256)
        config_path = Path(args.config)
        need(config_path.resolve() == config_path and config_path.is_absolute() and
             sha(config_path) == args.config_sha256, 'explicit input config differs')
        c = json.loads(config_path.read_text())
        validate_config(c)
        owned = module('prepare.py',sources)
        owned.runtime_content_preflight({'H': c['paths']['H']})
        root = Path(args.evidence_root)
        need(root.is_absolute() and root.resolve() == root and root.is_dir() and
             re.fullmatch('[a-z0-9][a-z0-9-]{0,31}',args.name), 'canonical evidence root and bounded name required')
        out = root / args.name
        publish = Path(args.publish_directory)
        need(out.resolve() == out and not out.exists() and publish.is_absolute() and
             publish.resolve() == publish and not publish.exists() and publish.parent.is_dir(),
             'fresh evidence and package destinations required')
        sources[str(config_path)] = args.config_sha256
        binding = root / (args.name+'.pipeline.binding.json')
        recipe = root / (args.name+'.pipeline.recipe.json')
        save(binding,dict(state='PREPARED_NOT_RUN',config=c,sources=sources,
                          out=str(out),publish=str(publish)))
        save(recipe,dict(argv=[str(Path(sys.executable).resolve()),str(D/'pipeline.py'),'run',
                               '--binding',str(binding),'--binding-sha256',sha(binding)],
                         cwd=c['paths']['tool_R'],env={'HOME':os.environ['HOME'],'PATH':'/usr/bin:/bin',
                         'LANG':'C.UTF-8','LC_ALL':'C.UTF-8','TZ':'UTC'},unset=[],
                         memory_gib=16,reservation_gib=24,
                         scope='PRIVATE_STATIC_BUILD_LINK_DATA_PACKAGE_NOT_EXECUTION_QUALIFICATION'))
        print(json.dumps(dict(binding=str(binding),binding_sha256=sha(binding),recipe=str(recipe),
                              run_host_cpu_seconds=3600,run_host_wall_seconds=3600)))
        return
    binding = Path(args.binding)
    need(sha(binding) == args.binding_sha256, 'pipeline binding changed')
    bound = json.loads(binding.read_text())
    need(bound['state'] == 'PREPARED_NOT_RUN', 'wrong pipeline binding state')
    c = bound['config']; sources = bound['sources']; validate_config(c)
    def close_sources():
        need(sha(binding) == args.binding_sha256, 'pipeline binding changed at close')
        for path,digest in sources.items(): need(sha(path) == digest, 'pipeline source/config changed: '+path)
    close_sources()
    archive = module('archive/prepare.py',sources)
    section = module('section/prepare.py',sources)
    owned = module('prepare.py',sources)
    observer = module('observe_coordinates.py',sources)
    publisher = module('publish.py',sources)
    owned.runtime_content_preflight({'H': c['paths']['H']})
    out = Path(bound['out']); out.mkdir()
    paths,heads,digests = c['paths'],c['heads'],c['digests']
    journal = dict(state='RUNNING',qualification='NOT_PROVEN',readiness='NOT_PROVEN',
                   binding_sha256=args.binding_sha256,stages=[])
    closes=[]
    ac = lc = None
    body_error = None
    cancelled = None
    def record(stage,path):
        value = owned.file_fact(path)
        journal['stages'].append(dict(stage=stage,result=value))
        # A stage result is immutable once its stage returns.
        closes.append((path,value))
    def close_all():
        close_sources()
        for path,fact in closes: need(owned.file_fact(path) == fact, 'closed stage evidence changed')
    def common(runtime):
        result = ['--H',paths['H'],'--R',paths['runtime_R'] if runtime else paths['tool_R'],
                  '--h-head',heads['H'],'--tool-r-head',heads['runtime_R'] if runtime else heads['tool_R'],
                  '--runtime-r-pin',heads['runtime_R']]
        for name in ['cargo','rustc','host_cargo','host_rustc','target','cargo_home']:
            result += ['--'+name.replace('_','-'),paths[name]]
        result += ['--evidence-root',str(out),'--native-inputs-json',paths['native_inputs_json'],
                   '--native-inputs-sha256',digests['native_inputs'], '--runner',paths['runner'],
                   '--runner-sha256',digests['runner']]
        return result
    try:
        archive_out = stage_output(out, 'archive')
        invoke(archive,['prepare',*common(True),'--out',str(archive_out)])
        ab = out/(archive_out.name+'.binding.json'); ar = out/(archive_out.name+'.recipe.json')
        invoke(archive,['run','--binding',str(ab),'--binding-sha256',sha(ab)])
        build = archive_out/'result.json'; record('archive',build)
        ac = json.loads(ab.read_text())
        invoke(section,['--build-result',str(build),'--build-sha256',sha(build),
                        '--build-binding',str(ab),'--build-recipe',str(ar),
                        '--h-head',heads['H'],'--r-head',heads['runtime_R'],'--attempt','package',
                        '--evidence-root',str(out),'--inspection-tools-json',paths['inspection_tools_json'],
                        '--inspection-tools-sha256',digests['inspection_tools'],
                        '--memcpy-source',paths['memcpy_source'],'--musl-source-authority',paths['musl_source_authority'],
                        '--stack-chk-source',paths['stack_chk_source']])
        prepared = out/'loader-maintained-final-link-inputs-package/result.json'
        record('section',prepared)
        owned_out = stage_output(out, 'owned')
        link_args = ['prepare',*common(False),'--out',str(owned_out),
                     '--prepared-result',str(prepared),'--prepared-result-sha256',sha(prepared),
                     '--inspection-tools-json',paths['inspection_tools_json'],
                     '--inspection-tools-sha256',digests['inspection_tools'],
                     '--musl-source-archive',paths['musl_source_archive']]
        for name in ['initializer_module','tls_source_module','native_leaf_module','ordinary_gate_module',
                     'terminal_module','view_site_module','view_table_module']:
            link_args += ['--'+name.replace('_','-')+'-sha256',digests[name]]
        for name in ['optional_std_weak_source','optional_std_destructors_source','optional_musl_dl_iterate_source']:
            link_args += ['--'+name.replace('_','-'),paths[name]]
        invoke(owned,link_args)
        lb = out/(owned_out.name+'.binding.json')
        invoke(owned,['run','--binding',str(lb),'--binding-sha256',sha(lb)])
        linked = owned_out/'result.json'; record('owned',linked)
        lc = json.loads(lb.read_text())
        coordinates = out/'coordinates'
        invoke(observer,['--result',str(linked),'--result-sha256',sha(linked),
                         '--binding',str(lb),'--binding-sha256',sha(lb),'--out',str(coordinates)])
        cr = coordinates/'result.json'; record('coordinates',cr)
        def final_close():
            close_all()
            archive.files_close(ac)
            owned.files_close(lc)
            need(sha(ab) == json.loads(build.read_text())['binding_sha256'] and
                 sha(lb) == json.loads(linked.read_text())['binding_sha256'],
                 'actual archive/link binding changed after stage close')
            need(ar.is_file(), 'required archive recipe missing')
            need(ac['private_inputs'] == lc['private_inputs'], 'archive/link private input declarations differ')
            actual_private = owned.private_close(lc)
            for receipt in [json.loads(build.read_text()),json.loads(linked.read_text())]:
                need(receipt['source_tools_closed'] is True and
                     receipt['private_before'] == receipt['private_after'] == actual_private,
                     'actual stage/private source closure absent or changed')
            # Coordinate collector closed all output streams. Recheck every
            # retained actual link output before descriptor publication.
            for path,fact in json.loads(linked.read_text())['produced_outputs'].items():
                need(owned.file_fact(path) == fact,'actual owned output changed before publication')
            for row in json.loads(cr.read_text())['commands']:
                for stream in ['stdout','stderr']:
                    fact=row[stream];need(owned.file_fact(fact['path']) == fact,'coordinate stream changed before publication')
        journal['publication'] = publisher.publish(owned,lc,linked,cr,c['data_roles'],bound['publish'],final_close)
        journal['state'] = 'PUBLISHED_PRIVATE_STATIC_DATA_NOT_QUALIFIED'
    except BaseException as error:
        body_error = error
        journal.update(state='FAIL',error=repr(error))
        cancelled = cancellation(cancelled, error)
    finally:
        errors = []
        # Successful publication already executed final_close immediately
        # before its descriptor commit. Error paths still attempt every close.
        remaining_closes = [] if journal['state'] == 'PUBLISHED_PRIVATE_STATIC_DATA_NOT_QUALIFIED' else [
            close_all, *([lambda: archive.files_close(ac)] if ac is not None else []),
            *([lambda: owned.files_close(lc),lambda: owned.private_close(lc)] if lc is not None else [])]
        for close in remaining_closes:
            try: close()
            except BaseException as error:
                errors.append(repr(error))
                cancelled = cancellation(cancelled, error)
        if errors: journal.update(state='FAIL',close_errors=errors)
        try:
            save(out/'pipeline-result.json',journal)
        finally:
            if cancelled is not None:
                raise cancelled
    need(journal['state'] == 'PUBLISHED_PRIVATE_STATIC_DATA_NOT_QUALIFIED',
         repr({'body_error':repr(body_error),'close_errors':journal.get('close_errors',[])}))


if __name__ == '__main__':
    main()
