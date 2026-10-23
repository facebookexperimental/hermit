#!/usr/bin/env python3
"""Prepare only by default; run mode belongs inside existing run-host.py.
One serial host-library build, exact Cargo JSON rlib selection, tiny diagnostic.
Actual maintained static archive build; no tests, final link/loading, image launch or qualification.
"""
import argparse
from contextlib import ExitStack
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import stat
import tomllib
import time

sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent
ASSETS = D.parent / 'assets'
BINDING = ASSETS / 'source_guard.py'
PRIVATE_GUARD = ASSETS / 'native_guard.py'
PRIVATE_INVENTORY_SHA256 = 'f825b66769fb55bee298b0a41277b38767970e5cad4beca8400f02dcdd811fcc'
CODEGEN = '-C relocation-model=pic -C code-model=small -C target-feature=+crt-static -C link-self-contained=yes'
STREAM = 64 * 1024 * 1024


def need(ok, why):
    if not ok:
        raise RuntimeError(why)


def cancellation(pending, error):
    """Keep the first KeyboardInterrupt/SystemExit to re-raise after every close."""
    return pending if pending is not None or isinstance(error, Exception) else error


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def producer_environment(c):
    # Named cut: only these declared values reach a child. Ambient environment
    # is context for preparation, never an inherited producer input.
    return {
        'HOME': c['home'], 'TMPDIR': '/tmp',
        'PATH': str(Path(c['cargo']).parent) + ':/usr/bin:/bin',
        'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8', 'TZ': 'UTC',
        'CARGO': c['cargo'], 'RUSTC': c['rustc'],
        'RUSTDOC': str(Path(c['cargo']).parent / 'rustdoc'),
        'CARGO_HOME': c['cargo_home'], 'CARGO_TARGET_DIR': c['target'],
        'CARGO_INCREMENTAL': '0', 'CARGO_BUILD_JOBS': '4',
        'CARGO_NET_OFFLINE': 'true',
        'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS': CODEGEN,
        'CC': c['host_tool_names']['cc'], 'CXX': c['host_tool_names']['c++'],
        'AR': c['host_tool_names']['ar'], 'LD': c['host_tool_names']['ld'],
        'GIT_NO_LAZY_FETCH': '1', 'GIT_OPTIONAL_LOCKS': '0',
        'GIT_TERMINAL_PROMPT': '0', 'GIT_CONFIG_NOSYSTEM': '1',
        'GIT_CONFIG_GLOBAL': '/dev/null',
    }


def validate_environment(actual, expected):
    # Do not enumerate every possible compiler knob. Unlisted variables are
    # absent, even if a compiler starts recognizing a new variable tomorrow.
    need(dict(actual) == dict(expected), 'child environment differs from declared map')
    need(all(type(k) is str and type(v) is str and k and '=' not in k and
             '\0' not in k and '\0' not in v for k, v in actual.items()),
         'invalid declared child environment')


def declared_child_environment(c, host=False):
    expected = producer_environment(c)
    need(expected == c['effective_environment'], 'bound environment differs from declared producer policy')
    if host:
        expected.pop('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS')
        expected.pop('RUSTDOC')
        expected.update(CARGO=c['host-cargo'], RUSTC=c['host-rustc'],
                        PATH=str(Path(c['host-cargo']).parent) + ':/usr/bin:/bin')
    child = dict(expected)
    validate_environment(child, expected)
    return child


def snapshot(c):
    # Existing tracked_source invokes git with its process environment. Put
    # that existing helper inside an exact-environment Python child as well;
    # do not mutate or forward the original caller's environment.
    code = """import importlib.util,json,sys
from pathlib import Path
sys.dont_write_bytecode=True
c=json.load(sys.stdin)
spec=importlib.util.spec_from_file_location('existing_source_guard',c['helper'])
g=importlib.util.module_from_spec(spec);spec.loader.exec_module(g)
print(json.dumps({'H':g.tracked_source(c['H'],c['H_head']),
 'H_reverie':g.tracked_source(Path(c['H'])/'reverie',c['runtime_R_pin']),
 'active_pins':g.full_pins(c['H'],c['runtime_R_pin']),
 'tool_R':g.tracked_source(c['R'],c['tool_R_head'])},sort_keys=True))
"""
    inputs = {k: c[k] for k in ['H', 'H_head', 'R', 'tool_R_head', 'runtime_R_pin']}
    inputs['helper'] = str(BINDING)
    child = subprocess.run([c['python'], '-c', code], cwd=c['R'],
                           input=json.dumps(inputs).encode(), stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, env=declared_child_environment(c))
    need(len(child.stdout) <= STREAM and len(child.stderr) <= STREAM, 'source guard output exceeds bound')
    need(child.returncode == 0, 'source guard failed: ' + child.stderr.decode(errors='replace')[:2000])
    return json.loads(child.stdout)


def private_close(c):
    spec = importlib.util.spec_from_file_location('existing_private_inventory_guard', PRIVATE_GUARD)
    guard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(guard)
    observed = guard.private_guard(c['private_inputs'])
    need(observed['inventory_sha256'] == PRIVATE_INVENTORY_SHA256, 'private inventory changed')
    return observed


def cargo_config_snapshot(c):
    paths = set()
    for cwd in [Path(c['H']), Path(c['R'])]:
        for base in [cwd, *cwd.parents]:
            paths.update(base / '.cargo' / n for n in ['config', 'config.toml'])
    paths.update(Path(c['cargo_home']) / n for n in ['config', 'config.toml'])
    paths.update(Path(c['home']) / '.cargo' / n for n in ['config', 'config.toml'])
    ledger = {}
    for path in sorted(paths):
        need(path.parent.resolve() == path.parent, 'Cargo config parent is indirect: ' + str(path))
        try:
            info = path.lstat()
        except FileNotFoundError:
            ledger[str(path)] = {'state': 'ABSENT'}
            continue
        need(stat.S_ISREG(info.st_mode) and info.st_size <= 64 * 1024,
             'Cargo config not a bounded regular file: ' + str(path))
        raw = path.read_bytes()
        need(len(raw) == info.st_size, 'Cargo config changed during read')
        config = tomllib.loads(raw.decode('utf-8'))
        # These are the only active settings in actual discovered config files.
        # They govern offline networking, never codegen/native tool selection.
        # Refuse env/include/target/build/profile/linker/wrapper and all unknown
        # tables, rather than assuming a hash makes their semantics acceptable.
        need(set(config) <= {'net', 'http'}, 'unapproved Cargo config table: ' + str(path))
        net = config.get('net', {})
        http = config.get('http', {})
        need(isinstance(net, dict) and set(net) <= {'git-fetch-with-cli'} and
             all(type(v) is bool for v in net.values()), 'unapproved Cargo net config: ' + str(path))
        need(isinstance(http, dict) and set(http) <= {'proxy'} and
             all(type(v) is str for v in http.values()), 'unapproved Cargo http config: ' + str(path))
        ledger[str(path)] = {'state': 'REGULAR_FILE', 'bytes': len(raw),
                             'sha256': hashlib.sha256(raw).hexdigest(),
                             'mode': stat.S_IMODE(info.st_mode)}
    return ledger


def files_close(c):
    declared_child_environment(c)
    need(all(sha(p) == h for p, h in c['files'].items()), 'bound tool/driver/input changed')
    need(str(Path(shutil.which('git', path=declared_child_environment(c)['PATH'])).resolve()) == c['git'], 'Runner git search changed')
    need(snapshot(c) == c['source_before'], 'tracked H/tool-R source changed')
    need(cargo_config_snapshot(c) == c['cargo_configs'], 'Cargo config content/absence changed')
    for name, selected in c['host_tool_names'].items():
        need(shutil.which(name, path=declared_child_environment(c)['PATH']) == selected, 'native tool search changed: ' + name)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='mode', required=True)
    a = sub.add_parser('prepare')
    for name in ['H', 'R', 'h-head', 'tool-r-head', 'runtime-r-pin', 'cargo', 'rustc', 'host-cargo', 'host-rustc', 'target', 'cargo-home', 'out']:
        a.add_argument('--' + name, required=True)
    for name in ['evidence-root', 'native-inputs-json', 'native-inputs-sha256', 'runner', 'runner-sha256']:
        a.add_argument('--' + name, required=True)
    a.add_argument('--elf')
    a.add_argument('--objdump')
    a.add_argument('--elf-h-head')
    a.add_argument('--elf-runtime-r-pin')
    r = sub.add_parser('run')
    r.add_argument('--binding', required=True)
    r.add_argument('--binding-sha256', required=True)
    x = p.parse_args()
    if x.mode == 'prepare':
        for head in [x.h_head, x.tool_r_head, x.runtime_r_pin]:
            need(re.fullmatch('[0-9a-f]{40}', head), 'full independent expected heads required')
        need(bool(x.elf) == bool(x.objdump), 'ELF and objdump must be supplied together')
        need(bool(x.elf) == bool(x.elf_h_head) == bool(x.elf_runtime_r_pin), 'optional ELF requires independent declared retained H/R context')
        need(not x.elf, 'archive-build driver does not accept an ELF/root diagnostic')
        if x.elf:
            need(all(re.fullmatch('[0-9a-f]{40}', h) for h in [x.elf_h_head, x.elf_runtime_r_pin]), 'full declared ELF context heads required')
        def canonical(value):
            q = Path(value)
            need(q.is_absolute() and q.resolve() == q and q.exists(), 'canonical existing path required: ' + value)
            return str(q)
        c = {k: canonical(getattr(x, k.replace('-', '_'))) for k in ['H', 'R', 'cargo', 'rustc', 'host-cargo', 'host-rustc', 'target']}
        c.update(H_head=x.h_head, tool_R_head=x.tool_r_head, runtime_R_pin=x.runtime_r_pin,
                 cargo_home=str(Path(x.cargo_home).resolve()), out=str(Path(x.out).absolute()),
                 git=str(Path(shutil.which('git', path='/usr/bin:/bin')).resolve()), python=str(Path(sys.executable).resolve()),
                 home=canonical(os.environ['HOME']))
        need(Path(c['cargo_home']).is_dir(), 'existing offline Cargo home required')
        need(Path(c['target']) == Path(c['H']) / 'target', 'sole existing H/target required')
        need(all(Path(c[k]).is_file() and os.access(c[k], os.X_OK) for k in ['cargo', 'rustc', 'host-cargo', 'host-rustc', 'git']), 'executable compiler/tool required')
        out = Path(c['out'])
        c['evidence_root'] = canonical(x.evidence_root)
        need(Path(c['evidence_root']).is_dir() and out.is_relative_to(Path(c['evidence_root'])) and out.resolve() == out and not out.exists(), 'new evidence directory inside explicit root required')
        for path0, digest0 in [(x.native_inputs_json,x.native_inputs_sha256),(x.runner,x.runner_sha256)]:
            canonical(path0)
            need(re.fullmatch('[0-9a-f]{64}',digest0) and sha(path0)==digest0, 'explicit native/runner input differs')
        c['runner'] = canonical(x.runner)
        if x.elf:
            c.update(elf=canonical(x.elf), objdump=canonical(x.objdump),
                     retained_ELF_declared_context={'H_head': x.elf_h_head, 'runtime_R_pin': x.elf_runtime_r_pin,
                                                    'source_object_join': 'NOT_PROVEN_INPUT_DECLARATION_ONLY'})
            need(os.access(c['objdump'], os.X_OK), 'objdump not executable')
        native_reference = Path(x.native_inputs_json)
        reference = json.loads(native_reference.read_text())
        toolchain = reference['toolchain']
        inventory = Path(toolchain['input_inventory'])
        need(sha(inventory) == PRIVATE_INVENTORY_SHA256, 'known private inventory changed')
        prefix = Path(toolchain['prefix'])
        need(c['cargo'] == str(prefix / 'bin/cargo') and c['rustc'] == str(prefix / 'bin/rustc'), 'exact private July12 compiler required')
        need(sha(c['cargo']) == toolchain['cargo_sha256'] and sha(c['rustc']) == toolchain['rustc_sha256'], 'private compiler hash mismatch')
        need(sha(prefix / 'bin/rustdoc') == toolchain['rustdoc_sha256'], 'private rustdoc mismatch')
        need(c['host-cargo'] != c['cargo'] and c['host-rustc'] != c['rustc'], 'separate canonical GNU host compiler required')
        c['private_inputs'] = {'toolchain': toolchain}
        c['private_reference_scope'] = 'COMPILER_NATIVE_INPUTS_ONLY_NOT_RUNTIME_SOURCE_OR_BUILD_RECEIPT'
        c['host_tool_names'] = reference['host_tool_names']
        c['native_env'] = {'RUSTDOC': str(prefix / 'bin/rustdoc'), 'CARGO_NET_OFFLINE': 'true', 'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS': CODEGEN}
        c['effective_environment'] = producer_environment(c)
        validate_environment(c['effective_environment'], producer_environment(c))
        c['source_before'] = snapshot(c)
        c['cargo_configs'] = cargo_config_snapshot(c)
        c['files'] = {str(q): sha(q) for q in [Path(__file__).resolve(), D / 'main.rs', BINDING,
                       Path(c['cargo']), Path(c['rustc']), Path(c['host-cargo']), Path(c['host-rustc']), Path(c['git']), Path(c['python']), Path(c['runner']),
                       *([Path(c['elf']), Path(c['objdump'])] if x.elf else [])]}
        c['files'].update(reference['host_tools_sha256'])
        c['files'].update({str(p): sha(p) for p in [PRIVATE_GUARD, native_reference, inventory, prefix / 'bin/rustdoc']})
        need(all(sha(p) == h for p, h in c['files'].items()), 'known native/tool identity mismatch')
        c['qualifier_source_sha256'] = sha(Path(c['R']) / 'reverie-liteinst-artifact/src/lib.rs')
        c['state'] = 'PREPARED_NOT_RUN'
        binding = Path(c['evidence_root']) / (out.name + '.binding.json')
        recipe = Path(c['evidence_root']) / (out.name + '.recipe.json')
        need(not binding.exists() and not recipe.exists(), 'fresh binding/recipe names required')
        save(binding, c)
        # run-host may retain ambient values in this Python driver's process;
        # none is forwarded by the source/metadata/compiler child launchers.
        env = dict(c['effective_environment'])
        validate_environment(env, producer_environment(c))
        unset = []
        save(binding, c)
        save(recipe, {'argv': [c['python'], str(Path(__file__).resolve()), 'run', '--binding', str(binding), '--binding-sha256', sha(binding)],
                      'cwd': c['R'], 'unset': sorted(set(unset)), 'env': env,
                      'memory_gib': 16, 'reservation_gib': 24,
                      'scope': 'MAINTAINED_STATIC_ARCHIVE_BUILD_ONLY_NOT_QUALIFIED',
                      'binding_sha256': sha(binding), 'runtime_qualification': 'NOT_PROVEN'})
        print(json.dumps({'recipe': str(recipe), 'binding': str(binding), 'binding_sha256': sha(binding),
                          'run_host_cpu_seconds': 1800, 'run_host_wall_seconds': 900}))
        return

    binding = Path(x.binding)
    need(sha(binding) == x.binding_sha256, 'prepared binding changed')
    c = json.loads(binding.read_text())
    need(c['state'] == 'PREPARED_NOT_RUN', 'wrong binding state')
    out = Path(c['out'])
    out.mkdir()
    result = {'state': 'RUNNING', 'qualification': 'NOT_PROVEN', 'readiness': 'NOT_PROVEN',
              'H_head': c['H_head'], 'tool_R_head': c['tool_R_head'], 'runtime_R_pin': c['runtime_R_pin'],
              'binding_sha256': sha(binding), 'commands': [], 'tests_executed': 0, 'guests_executed': 0,
              'profile': 'liteinst-runtime', 'target': 'x86_64-unknown-linux-musl'}
    if c.get('elf'):
        result['retained_ELF_declared_context'] = c['retained_ELF_declared_context']
    save(out / 'result.json', result)
    def command(name, argv, cwd, host=False):
        # Existing enclosing run-host.py owns CPU/wall/RSS/pids/disk/descendants.
        # These raw files are retained, never piped through another command.
        env = declared_child_environment(c, host=host)
        row = {'name': name, 'argv': argv, 'cwd': str(cwd), 'compiler_context': 'GNU_HOST_TOOL' if host else 'PRIVATE_SOURCE_EXPECTATION', 'RUSTC': env.get('RUSTC')}
        result['commands'].append(row)
        save(out / 'result.json', result)
        started = time.monotonic()
        command_cancelled = None
        def close_stream(stream):
            nonlocal command_cancelled
            try:
                stream.close()
            except BaseException as error:
                command_cancelled = cancellation(command_cancelled, error)
                raise
        try:
            with ExitStack() as streams:
                try:
                    o = (out / (name + '.stdout')).open('xb')
                    streams.callback(close_stream, o)
                    e = (out / (name + '.stderr')).open('xb')
                    streams.callback(close_stream, e)
                    child = subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=o, stderr=e, env=env)
                except BaseException as error:
                    command_cancelled = cancellation(command_cancelled, error)
                    raise
        finally:
            if command_cancelled is not None:
                raise command_cancelled
        row.update(exit=child.returncode, wall_seconds=time.monotonic() - started)
        for stream in ['stdout', 'stderr']:
            q = out / (name + '.' + stream)
            row[stream] = {'path': str(q), 'bytes': q.stat().st_size, 'sha256': sha(q)}
        save(out / 'result.json', result)
        need(child.returncode == 0, name + ' failed')
        need(all(row[s]['bytes'] <= STREAM for s in ['stdout', 'stderr']), name + ' retained stream exceeds guard')
        return (out / (name + '.stdout')).read_text()
    cancelled = None
    try:
        files_close(c)
        result['private_before'] = private_close(c)
        save(out / 'result.json', result)
        declared_child_environment(c)
        manifest = str(Path(c['R']) / 'reverie-liteinst-artifact/Cargo.toml')
        metadata = json.loads(command('tool-metadata', [c['host-cargo'], 'metadata', '--locked', '--offline', '--format-version=1', '--no-deps', '--manifest-path', manifest], c['R'], host=True))
        need(metadata['target_directory'] == c['target'], 'tool metadata selected another target directory')
        packages = [q for q in metadata['packages'] if q['name'] == 'reverie-liteinst-artifact' and q['manifest_path'] == manifest and q['source'] is None]
        need(len(packages) == 1, 'current tool package metadata ambiguous')
        package = packages[0]
        targets = [q for q in package['targets'] if q['name'] == 'reverie_liteinst_artifact' and q['kind'] == ['lib'] and q['crate_types'] == ['lib'] and q['src_path'] == str(Path(c['R']) / 'reverie-liteinst-artifact/src/lib.rs')]
        need(len(targets) == 1, 'tool library target ambiguous')
        raw = command('tool-build', [c['host-cargo'], 'build', '--locked', '--offline', '--manifest-path', manifest, '-p', 'reverie-liteinst-artifact', '--lib', '-j4', '--target-dir', c['target'], '--message-format=json-render-diagnostics'], c['R'], host=True)
        records = [json.loads(line) for line in raw.splitlines() if line.strip()]
        need([q['success'] for q in records if q.get('reason') == 'build-finished'] == [True], 'Cargo did not complete one successful build')
        roots = [q for q in records if q.get('reason') == 'compiler-artifact' and (q.get('package_id') == package['id'] or q.get('manifest_path') == manifest)]
        need(len(roots) == 1, 'unexpected/duplicate same-package artifact; no broad build-script exemption')
        selected = roots[0]
        need(selected['target'] == targets[0] and selected['package_id'] == package['id'] and selected['manifest_path'] == manifest, 'complete target/package identity differs')
        need(selected['features'] == [] and selected['profile']['test'] is False and selected['executable'] is None, 'unexpected tool features/test/executable')
        rlibs = [Path(q) for q in selected['filenames'] if q.endswith('.rlib')]
        need(len(rlibs) == 1, 'exact rlib absent/ambiguous')
        for q in map(Path, selected['filenames']):
            need(q.is_absolute() and q.resolve() == q and q.is_file() and q.is_relative_to(Path(c['target']) / 'debug'), 'tool output outside expected sole target/debug')
        rlib = rlibs[0]
        result['selected_tool_artifact'] = selected
        result['selected_outputs_sha256'] = {q: sha(q) for q in selected['filenames']}
        save(out / 'selected-tool.json', result['selected_tool_artifact'])
        files_close(c)
        binary = Path(c['target']) / 'loader-static-diagnostic' / out.name / 'diagnostic'
        need(not binary.parent.exists(), 'fresh helper output directory required')
        need(binary.parent.resolve() == binary.parent, 'helper output redirects through symlink')
        binary.parent.mkdir(parents=True)
        deps = Path(c['target']) / 'debug/deps'
        need(deps.is_dir() and deps.resolve() == deps, 'actual host dependency directory absent/indirect')
        command('compile-driver', [c['host-rustc'], '--edition=2021', '--crate-name', 'static_source_diagnostic', str(D / 'main.rs'), '--extern', 'reverie_liteinst_artifact=' + str(rlib), '-L', 'dependency=' + str(deps), '-o', str(binary)], c['R'], host=True)
        result['driver_binary'] = {'path': str(binary), 'sha256': sha(binary)}
        argv = [str(binary), c['H'], c['H_head'], c['runtime_R_pin'], c['cargo'], c['rustc'], c['target'], str(out / 'runner'), c['tool_R_head'], c['qualifier_source_sha256']]
        if c.get('elf'):
            argv += [c['elf'], c['objdump']]
        diagnostic = command('diagnostic', argv, c['H'])
        need(diagnostic.splitlines().count('SOURCE_STATE=ARCHIVE_BUILD_ONLY_NOT_QUALIFIED') == 1, 'missing closed maintained archive result')
        def field(name):
            values = [line[len(name) + 1:] for line in diagnostic.splitlines() if line.startswith(name + '=')]
            need(len(values) == 1, 'missing/duplicate archive observation: ' + name)
            return values[0]
        archive = Path(field('ARCHIVE_PATH'))
        need(archive.is_absolute() and archive.resolve() == archive and archive.is_file(), 'selected archive absent/indirect')
        need(archive.is_relative_to(Path(c['target']) / 'x86_64-unknown-linux-musl/liteinst-runtime'), 'wrong maintained profile output')
        need(re.fullmatch('[0-9a-f]{64}', field('ARCHIVE_SHA256')) and sha(archive) == field('ARCHIVE_SHA256'), 'selected archive changed')
        result['archive'] = {'path': str(archive), 'bytes': archive.stat().st_size, 'sha256': sha(archive)}
        result['metadata_sha256'] = field('METADATA_SHA256')
        result['cargo_messages_sha256'] = field('CARGO_MESSAGES_SHA256')
        need(all(re.fullmatch('[0-9a-f]{64}', result[k]) for k in ['metadata_sha256', 'cargo_messages_sha256']), 'invalid factory digest observation')
        need('QUALIFICATION=NOT_PROVEN' in diagnostic.splitlines(), 'scope marker missing')
        need(sha(binary) == result['driver_binary']['sha256'], 'driver changed')
        need(all(sha(q) == h for q, h in result['selected_outputs_sha256'].items()), 'selected host outputs changed')
        result['state'] = 'ARCHIVE_BUILD_ONLY_NOT_QUALIFIED'
        result['root_diagnostic'] = 'ROOT_TRANSFER_DIAGNOSTIC_ONLY_NOT_PROVEN' if c.get('elf') else 'NOT_RUN'
    except BaseException as error:
        result.update(state='FAIL', error=repr(error))
        cancelled = cancellation(cancelled, error)
        raise
    finally:
        close_errors = []
        try:
            files_close(c)
        except BaseException as error:
            close_errors.append('source/tools: ' + repr(error))
            cancelled = cancellation(cancelled, error)
        try:
            result['private_after'] = private_close(c)
            need(result.get('private_before') == result['private_after'], 'private inventory close mismatch')
        except BaseException as error:
            close_errors.append('private inventory: ' + repr(error))
            cancelled = cancellation(cancelled, error)
        try:
            if result.get('archive'):
                need(sha(result['archive']['path']) == result['archive']['sha256'], 'archive changed at close')
        except BaseException as error:
            close_errors.append('selected archive: ' + repr(error))
            cancelled = cancellation(cancelled, error)
        result['source_tools_closed'] = not close_errors
        if close_errors:
            result.update(state='FAIL', close_errors=close_errors)
        try:
            save(out / 'result.json', result)
        finally:
            if cancelled is not None:
                raise cancelled
    need(result['state'] == 'ARCHIVE_BUILD_ONLY_NOT_QUALIFIED', 'archive/source/private-input close failed')


if __name__ == '__main__':
    main()
