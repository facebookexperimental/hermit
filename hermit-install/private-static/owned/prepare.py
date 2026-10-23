#!/usr/bin/env python3
"""Prepare only by default; run mode belongs inside existing run-host.py.
One serial host-library build, exact Cargo JSON rlib selection, tiny diagnostic.
Actual fresh source-owned static archive and ONE owned diagnostic link; no image loading, guest or qualification.
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
import selectors
import signal

sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent
ASSETS = D / 'assets'
BINDING = ASSETS / 'source_guard.py'
PRIVATE_GUARD = ASSETS / 'native_guard.py'
PRIVATE_INVENTORY_SHA256 = 'f825b66769fb55bee298b0a41277b38767970e5cad4beca8400f02dcdd811fcc'
CODEGEN = '-C relocation-model=pic -C code-model=small -C target-feature=+crt-static -C link-self-contained=yes'
STREAM = 64 * 1024 * 1024
RUNTIME_COHORT = D / 'runtime_cohort.py'
RUNTIME_COHORT_SHA256 = '04168aef6f04f86d469095d2d60aec4b996e78c717a01b503b137553a845b326'
REQUIRED_HANDOFF_SOURCE_SHA256 = '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'


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


def bounded_stream_command(argv, cwd, env, stdout_file, stderr_file, limit, timeout, row):
    """Cap pipes without restricting regular artifact files.

    Keep the original leader unreaped through every group-directed signal.
    Outer containment owns descendants which create their own process groups.
    """
    need(timeout is not None and timeout > 0, 'finite positive command timeout required')
    child = None
    selector = selectors.DefaultSelector()
    counts = {'stdout': 0, 'stderr': 0}
    row['stream_bytes_observed'] = counts
    row['regular_artifact_file_cap'] = 'UNCHANGED_OUTER_CONTAINMENT_NO_STREAM_RLIMIT'
    row['effective_timeout_seconds'] = timeout
    deadline = time.monotonic() + timeout
    drain_deadline = None
    failure = None
    cancelled = None

    def leader_exited_without_reap():
        # Popen.poll()/wait() would release PID custody too early. WNOWAIT
        # observes the same retained child but leaves its identity owned here.
        observed = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        return observed is not None

    def kill_retained_group():
        # Called only before the sole child.wait below; never after a reap.
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
                row['retained_leader_group_kill_sent'] = True
            except ProcessLookupError:
                row['retained_leader_group_absent'] = True

    try:
        child = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
            start_new_session=True)
        row['pid'] = child.pid
        for name, pipe, target in [('stdout', child.stdout, stdout_file),
                                   ('stderr', child.stderr, stderr_file)]:
            os.set_blocking(pipe.fileno(), False)
            selector.register(pipe, selectors.EVENT_READ, (name, target))
        while selector.get_map() or not leader_exited_without_reap():
            now = time.monotonic()
            if failure is None and now >= deadline:
                failure = 'COMMAND_TIMEOUT'
                row['stream_failure'] = failure
                kill_retained_group();drain_deadline = now + 5
            if drain_deadline is not None and now >= drain_deadline:
                row['post_kill_drain_deadline_hit'] = True
                break
            remaining = min(0.25, max(0.0, (deadline if failure is None else drain_deadline)-now))
            for key, _events in selector.select(remaining):
                name, target = key.data
                try:
                    data = os.read(key.fileobj.fileno(), 65536)
                except BlockingIOError:
                    continue
                if not data:
                    selector.unregister(key.fileobj);key.fileobj.close();continue
                before = counts[name];counts[name] += len(data)
                target.write(data[:max(0, limit-before)])
                if failure is None and counts[name] > limit:
                    failure = name.upper() + '_STREAM_LIMIT_EXCEEDED'
                    row['stream_failure'] = failure
                    kill_retained_group();drain_deadline = time.monotonic() + 5
        need(failure is None, str(failure))
    except BaseException as error:
        cancelled = cancellation(cancelled, error)
        raise
    finally:
        # The leader still owns its PID/PGID: no poll, wait, or reaping waitid
        # occurs above. Attempt every close, signal before the sole wait, and
        # retain cancellation even when cleanup itself fails.
        cleanup_errors = []

        def cleanup(action):
            nonlocal cancelled
            try:
                action()
            except BaseException as error:
                cleanup_errors.append(error)
                cancelled = cancellation(cancelled, error)

        cleanup(kill_retained_group)
        cleanup(selector.close)
        if child is not None:
            for pipe in [child.stdout, child.stderr]:
                if pipe is not None:
                    cleanup(pipe.close)

            def reap():
                try:
                    row['exit'] = child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    row['reap_deadline_hit'] = True
                    raise RuntimeError('leader not reaped after bounded group kill')

            cleanup(reap)
        if cleanup_errors:
            row['cleanup_errors'] = [repr(error) for error in cleanup_errors]
        if cancelled is not None:
            raise cancelled
        if cleanup_errors:
            raise cleanup_errors[-1]


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


def runtime_content_preflight(c):
    """Finite reviewed content selection; chosen full Git heads stay external."""
    need(sha(RUNTIME_COHORT) == RUNTIME_COHORT_SHA256, 'runtime cohort declaration changed')
    spec = importlib.util.spec_from_file_location('existing_runtime_content_cohort', RUNTIME_COHORT)
    cohort = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cohort)
    return cohort.bind(Path(c['H']))


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
    return dict(json.loads(child.stdout), runtime_content_cohort=runtime_content_preflight(c))


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


LEGACY = ASSETS / 'final_inspection.py'
LEGACY_SHA = '26a037c4af80f9ade6e7156e2f2adf636dae33d7e64db2e463e55e93bbdb5af9'


def file_fact(path):
    p = Path(path)
    need(p.is_file() and not p.is_symlink() and p.resolve() == p, 'nonregular/indirect evidence: ' + str(p))
    return {'path': str(p), 'bytes': p.stat().st_size, 'sha256': sha(p)}


def runtime_handoff_preflight(c):
    """Check real nested runtime source against the independently declared cohort."""
    path = Path(c['H']) / 'reverie/reverie-loader/src/handoff.rs'
    identity = file_fact(path)
    need(identity['sha256'] == REQUIRED_HANDOFF_SOURCE_SHA256,
         'actual runtime handoff source differs from declared V10 cohort')
    return identity


def bind_prepared(c, x):
    path = Path(x.prepared_result)
    need(path.is_relative_to(Path(c['evidence_root'])) and path.resolve() == path, 'prepared receipt outside owned evidence')
    need(re.fullmatch('[0-9a-f]{64}', x.prepared_result_sha256) and
         sha(path) == x.prepared_result_sha256, 'actual section preparation receipt changed')
    result = json.loads(path.read_text())
    need(result['state'] == 'PASS_SECTION_SCRIPT_PREPARATION_ONLY' and
         result['source_and_inputs_unchanged'] is True and
         result['h_head'] == c['H_head'] and result['r_head'] == c['runtime_R_pin'],
         'not the actual closed maintained runtime section preparation')
    need(result['link'] == 'NOT_RUN' and result['tests_executed'] == result['guests_executed'] == 0,
         'unexpected preparation execution/authority')
    pf = result['prepared_inputs']
    need(file_fact(pf['path']) == pf, 'actual prepared input bytes changed')
    prepared = json.loads(Path(pf['path']).read_text())
    need(prepared['state'] == 'PREPARED_PATH_INPUTS_ONLY_NOT_STATIC_ARCHIVE_OR_QUALIFICATION' and
         prepared['source'] == {'H': c['H_head'], 'R': c['runtime_R_pin']} and
         prepared['actual_factory_archive'] == result['archive'], 'prepared runtime cohort differs')
    # This receipt must be freshly produced for the exact Hbe313/Rb13 cohort.
    # Tool R remains independently bound and source-closed by snapshot(c).
    c['prepared_archive'] = prepared['actual_factory_archive']
    need(c['prepared_archive']['path'] == str(Path(c['target']) / 'x86_64-unknown-linux-musl/liteinst-runtime/libdetcore_liteinst.a'),
         'prepared archive not actual maintained sole-target profile')
    c['files'].update(prepared['bound_files_sha256'])
    c['files'][str(path)] = x.prepared_result_sha256
    c['files'][pf['path']] = pf['sha256']
    for p, expected in result['produced_outputs'].items():
        need(file_fact(p) == expected, 'section preparation evidence changed')
        c['files'][p] = expected['sha256']
    paths = dict(prepared['typed_method_path_inputs'])
    paths['output_directory'] = str(Path(c['target']) / ('loader-owned-final-link-' + Path(c['out']).name))
    need(not Path(paths['output_directory']).exists() and
         not Path(paths['output_directory']).with_suffix('.negative-inputs').exists(), 'fresh sole-target link/control directories required')
    c['link_paths'] = paths
    c['prepared_directory'] = str(path.parent)
    need(re.fullmatch('[0-9a-f]{64}', x.initializer_module_sha256), 'independent actual reviewed module SHA required')
    module = Path(c['R']) / 'reverie-liteinst-artifact/src/private_static/selected_initializers.rs'
    need(file_fact(module)['sha256'] == x.initializer_module_sha256, 'actual compiled initializer module differs')
    leaf_module = Path(c['R']) / 'reverie-liteinst-artifact/src/private_static/selected_initializers/native_leaves.rs'
    need(re.fullmatch('[0-9a-f]{64}', x.native_leaf_module_sha256) and
         file_fact(leaf_module)['sha256'] == x.native_leaf_module_sha256, 'actual native leaf module differs')
    ordinary_module = Path(c['R']) / 'reverie-liteinst-artifact/src/private_static/selected_initializers/ordinary_gate.rs'
    need(re.fullmatch('[0-9a-f]{64}', x.ordinary_gate_module_sha256) and
         file_fact(ordinary_module)['sha256'] == x.ordinary_gate_module_sha256, 'actual ordinary gate module differs')
    c['ordinary_gate_module_sha256'] = x.ordinary_gate_module_sha256
    c['files'][str(ordinary_module)] = x.ordinary_gate_module_sha256
    c['files'][str(D / 'ordinary_controls.rs')] = sha(D / 'ordinary_controls.rs')
    c['files'][str(D / 'structural_observation.rs')] = sha(D / 'structural_observation.rs')
    c['files'][str(D / 'terminal_observation.rs')] = sha(D / 'terminal_observation.rs')
    terminal_module = Path(c['R']) / 'reverie-liteinst-artifact/src/private_static/selected_initializers/terminal_observation.rs'
    need(re.fullmatch('[0-9a-f]{64}', x.terminal_module_sha256) and
         file_fact(terminal_module)['sha256'] == x.terminal_module_sha256,
         'actual reviewed terminal observation module differs')
    for role, expected0 in [('site', x.view_site_module_sha256), ('table', x.view_table_module_sha256)]:
        path0 = Path(c['R']) / ('reverie-liteinst-artifact/src/private_static/selected_initializers/view_' + role + '.rs')
        need(re.fullmatch('[0-9a-f]{64}', expected0) and file_fact(path0)['sha256'] == expected0,
             'actual reviewed View leaf module differs: ' + role)
        c['view_' + role + '_module_sha256'] = expected0
        c['files'][str(path0)] = expected0
    for path0 in [D / 'view_leaf_controls.rs', D / 'view_leaf_protocol.py']:
        c['files'][str(path0)] = sha(path0)
    c['terminal_module_sha256'] = x.terminal_module_sha256
    c['files'][str(terminal_module)] = x.terminal_module_sha256
    c['native_leaf_module_sha256'] = x.native_leaf_module_sha256
    c['files'][str(leaf_module)] = x.native_leaf_module_sha256
    fixtures = Path(c['R']) / 'reverie-liteinst-artifact/tests/fixtures'
    c['native_leaf_sources'] = {}
    for role, name, digest in [
        ('memset', 'musl_memset_source.txt', 'aa843bcfb5b75e232df8d26dab6e5cec4f7123b33bebe655ce957f95f839f2e1'),
        ('set_thread_area', 'musl_set_thread_area_source.txt', '9a6299228be5f8f467652263e8f98c1f9ab2e0937243589b8c5642f0ebe3236d')]:
        path = fixtures / name
        need(file_fact(path)['sha256'] == digest, 'exact release native leaf source differs: ' + role)
        c['native_leaf_sources'][role] = str(path)
        c['files'][str(path)] = digest
    lock = tomllib.loads((Path(c['R']) / 'Cargo.lock').read_text())
    packages = [p for p in lock['package'] if p['name'] == 'serde_json' and
                p.get('source') == 'registry+https://github.com/rust-lang/crates.io-index']
    need(len(packages) == 1, 'current locked serde_json dependency ambiguous')
    c['serde_json_locked'] = packages[0]
    c['files'][str(D / 'native_leaf_controls.rs')] = sha(D / 'native_leaf_controls.rs')
    c['files'][str(D / 'optional_controls.rs')] = sha(D / 'optional_controls.rs')
    musl_archive = Path(x.musl_source_archive)
    need(musl_archive.is_absolute() and musl_archive.resolve() == musl_archive and
         file_fact(musl_archive)['sha256'] == 'a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4',
         'optional musl source archive differs or redirects')
    c['files'][str(musl_archive)] = sha(musl_archive)
    c['optional_sources'] = {}
    for role, supplied, expected in [
        ('std_weak', x.optional_std_weak_source, '0f9c4e63d9f15b2486e8c0a9af89c4c2ad88fcad7c1a5127a95d8031e13ec2d3'),
        ('std_destructors', x.optional_std_destructors_source, '5634b13e8faa9ead0e7dac104abf51a990bc14cbc39c5c6bae22a7b70811e2b3'),
        ('musl_dl_iterate', x.optional_musl_dl_iterate_source, '802a537924c900ff607b65bda465bf66899890b7b0707c0cd55c7b800db20327')]:
        path = Path(supplied)
        need(path.is_absolute() and path.resolve() == path and file_fact(path)['sha256'] == expected,
             'actual optional source differs or redirects: ' + role)
        c['optional_sources'][role] = str(path)
        c['files'][str(path)] = expected
    tls_source_module = Path(c['R']) / 'reverie-liteinst-artifact/src/private_static/tls_sources.rs'
    need(re.fullmatch('[0-9a-f]{64}', x.tls_source_module_sha256) and
         file_fact(tls_source_module)['sha256'] == x.tls_source_module_sha256,
         'actual reviewed closed TLS source-tuple module differs')
    c['tls_source_module_sha256'] = x.tls_source_module_sha256
    c['files'][str(tls_source_module)] = x.tls_source_module_sha256
    c['initializer_module_sha256'] = x.initializer_module_sha256
    c['files'][str(module)] = x.initializer_module_sha256
    need(sha(D / 'factory_controls.rs') == '32f40e1ebd50c2d0273d8d90e91c01cc015717a5e975814f6b7ac8be93da48c3',
         'actual eight-assertion factory controls differ')
    for p in [D / 'factory_controls.rs', LEGACY]:
        c['files'][str(p)] = sha(p)
    need(sha(LEGACY) == LEGACY_SHA, 'existing strict V12 inspection source changed')
    toolsfile = Path(x.inspection_tools_json)
    need(sha(toolsfile) == x.inspection_tools_sha256 == 'b5b75a7dbf58d0805bbfcfb8d1fc17e55b1c528a2d2d210d5def39ea559d97a0', 'actual inspection tool inventory changed')
    c['files'][str(toolsfile)] = sha(toolsfile)
    c['inspection_tools'] = json.loads(toolsfile.read_text())['tools']
    for v in c['inspection_tools'].values():c['files'][v['path']] = v['sha256']
    prefix = Path(c['private_inputs']['toolchain']['prefix'])
    objdump = prefix / 'lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objdump'
    need(file_fact(objdump)['sha256'] == '8f906de30ee713c953d1762f78901b3a5d4ccd99f89c65627b712618aeeddcf4', 'actual private objdump differs')
    c['objdump'] = str(objdump)
    c['files'][str(objdump)] = sha(objdump)
    census_objdump = Path('/usr/bin/objdump')
    census_tool = file_fact(census_objdump)
    need(census_tool['sha256'] == '0a1b558f444ed2bca7fb467dede7416a651bc490b1abc77705ab470d99c527c1', 'actual census GNU objdump differs')
    c['census_objdump'] = str(census_objdump)
    c['census_objdump_root_reported_version'] = '2.35.2-72.el9'
    c['files'][str(census_objdump)] = census_tool['sha256']
    # Existing diagnostic helpers only; their CLI/main is never invoked here.
    for name, digest in {
        'elf_metadata.py': 'c6fe4a6db783ca4af300e5cfbf595e545b17e746cd8594eccbf2ecb7c81215d5',
        'archive_metadata.py': 'd313a47a769e91e6d297c5452edf66b9129370f381c25e228385efdb01d60972',
        'source_guard.py': 'd59d40226b115ce395ded5612ec4ec5f5afabace8bc81d847a4ea51e6020749f',
        'native_guard.py': 'd05e3d63b7c914d2e2794d97dbda4a7e94f5c904aa7cdf8515c37c1c0d4bdda5',
    }.items():
        need(sha(ASSETS / name) == digest, 'existing diagnostic helper changed')
        c['files'][str(ASSETS / name)] = digest
    files_close(c)


def inspect_produced(c, out, result, produced, command):
    target = Path(c['link_paths']['output_directory']) / 'initializer-definitions.not-qualified.elf'
    need(target.exists(), 'owned linker produced no actual ELF; inspection NOT_RUN')
    need(str(target) in produced and produced[str(target)] == file_fact(target),
         'captured owned ELF changed before inspection')
    spec = importlib.util.spec_from_file_location('existing_v12_inspections_only', LEGACY)
    legacy = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(legacy)
    headers = legacy.module('elf_metadata.py')
    tools = c['inspection_tools']
    def run(name, argv, timeout=120):
        command(name, argv, c['H'], inspection=True, timeout=timeout)
        return out / (name + '.stdout')
    raw_errors = []
    for name, argv in [
        ('final-headers', [tools['readelf']['path'], '-hlW', str(target)]),
        ('final-sections', [tools['readelf']['path'], '-SW', str(target)]),
        ('final-relocations', [tools['readelf']['path'], '-rW', str(target)]),
        ('final-symbols', [tools['nm']['path'], '-n', str(target)]),
        ('final-unresolved', [tools['nm']['path'], '-u', str(target)]),
    ]:
        try:run(name, argv)
        except Exception as error:raw_errors.append(name + ': ' + repr(error))
    need(not raw_errors, 'strict raw final inspections failed: ' + repr(raw_errors))
    header = headers.elf_headers(target)
    need(len(header['tls']) == 1, 'actual private-musl TLS segment missing/ambiguous')
    matches = [int(w[0], 16) for line in (out / 'final-symbols.stdout').read_text().splitlines()
               if len(w := line.split()) == 3 and w[1] == 'T' and w[2] == legacy.ROOT]
    need(matches == [header['entry']], 'actual private entry differs from ELF entry')
    result['headers'] = header
    directory = Path(c['prepared_directory'])
    objects = json.loads((directory / 'actual-physical-objects.json').read_text())
    symbol_inputs = []
    for label, archive in [('detcore', c['prepared_archive']['path']), ('libc', c['link_paths']['libc']),
                           ('libunwind', c['link_paths']['supporting_archives'][0])]:
        physical = [o for o in objects if o['archive'] == archive]
        need(physical, 'actual prepared physical object evidence absent')
        symbols = json.loads((directory / (label + '-actual-symbols.json')).read_text())
        symbol_inputs.append((Path(archive), physical, symbols))
    result['diagnostics'] = legacy.final_diagnostics(out, target, header, symbol_inputs, run,
                                                    Path(c['objdump']), produced)
    # Preserve the exact raw strict refusal. Only the separately source-closed
    # Rust structural receipt permits an explicit alternative aggregate route.
    unresolved = (out / 'final-unresolved.stdout').read_text()
    result['default_raw_undefined_inspection'] = {'state':'FAIL_RETAINED' if unresolved.strip() else 'PASS',
                                                'stdout':file_fact(out / 'final-unresolved.stdout')}
    receipt = result.get('explicit_structural_source_closed_receipt')
    need(receipt is not None, 'no actual source-closed explicit structural receipt')
    # This is a reconciliation of the raw tool's output with a private Rust
    # classifier already run on THIS exact full ELF. It is not a name exemption.
    rows = [line.split() for line in unresolved.splitlines()]
    need(rows == [['U','_DYNAMIC'],['w','__cxa_thread_atexit_impl'],['w','gettid']],
         'raw final undefined rows differ from explicit structural observed cohort')
    need(receipt['observation']['artifact'] == {'path':str(target),'sha256':sha(target)},
         'explicit structural artifact differs from independently inspected bytes')
    result['optional_static_zero_guard_proof'] = 'FINITE_SAME_OWNER_OBSERVATION_ONLY_NOT_EXECUTION_PROOF'
    result['explicit_structural_inspection'] = 'PASS_STRUCTURE_ONLY'
    save(out / 'result.json', result)
    need(receipt['observation']['link_map'] == {'path':str(Path(c['link_paths']['output_directory'])/'initializer-definitions.map'),
         'sha256':sha(Path(c['link_paths']['output_directory'])/'initializer-definitions.map')}, 'structural same-owner map differs')
    result['elf'] = file_fact(target)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='mode', required=True)
    a = sub.add_parser('prepare')
    for name in ['H', 'R', 'h-head', 'tool-r-head', 'runtime-r-pin', 'cargo', 'rustc', 'host-cargo', 'host-rustc', 'target', 'cargo-home', 'out']:
        a.add_argument('--' + name, required=True)
    for name in ['evidence-root', 'native-inputs-json', 'native-inputs-sha256',
                 'inspection-tools-json', 'inspection-tools-sha256', 'musl-source-archive',
                 'runner', 'runner-sha256']:
        a.add_argument('--' + name, required=True)
    a.add_argument('--prepared-result', required=True)
    a.add_argument('--prepared-result-sha256', required=True)
    a.add_argument('--initializer-module-sha256', required=True)
    a.add_argument('--tls-source-module-sha256', required=True)
    a.add_argument('--native-leaf-module-sha256', required=True)
    a.add_argument('--optional-std-weak-source', required=True)
    a.add_argument('--optional-std-destructors-source', required=True)
    a.add_argument('--optional-musl-dl-iterate-source', required=True)
    a.add_argument('--ordinary-gate-module-sha256', required=True)
    a.add_argument('--terminal-module-sha256', required=True)
    a.add_argument('--view-site-module-sha256', required=True)
    a.add_argument('--view-table-module-sha256', required=True)
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
        runtime_content_preflight({'H': x.H})
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
        need(Path(c['evidence_root']).is_dir() and out.is_relative_to(Path(c['evidence_root'])) and
             out.resolve() == out and not out.exists(), 'new evidence directory inside explicit evidence root required')
        for path0, digest0 in [(x.native_inputs_json, x.native_inputs_sha256),
                               (x.inspection_tools_json, x.inspection_tools_sha256),
                               (x.runner, x.runner_sha256)]:
            canonical(path0)
            need(re.fullmatch('[0-9a-f]{64}', digest0) and sha(path0) == digest0,
                 'explicit native/tool/runner input differs')
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
        c['runtime_handoff_source'] = runtime_handoff_preflight(c)
        c['source_before'] = snapshot(c)
        c['cargo_configs'] = cargo_config_snapshot(c)
        c['files'] = {str(q): sha(q) for q in [Path(__file__).resolve(), D / 'main.rs', BINDING, RUNTIME_COHORT,
                       Path(c['cargo']), Path(c['rustc']), Path(c['host-cargo']), Path(c['host-rustc']), Path(c['git']), Path(c['python']), Path(c['runner']),
                       *([Path(c['elf']), Path(c['objdump'])] if x.elf else [])]}
        c['files'][str(native_reference)] = x.native_inputs_sha256
        c['files'][c['runtime_handoff_source']['path']] = REQUIRED_HANDOFF_SOURCE_SHA256
        c['files'].update(reference['host_tools_sha256'])
        c['files'].update({str(p): sha(p) for p in [PRIVATE_GUARD, native_reference, inventory, prefix / 'bin/rustdoc']})
        need(all(sha(p) == h for p, h in c['files'].items()), 'known native/tool identity mismatch')
        c['qualifier_source_sha256'] = sha(Path(c['R']) / 'reverie-liteinst-artifact/src/lib.rs')
        bind_prepared(c, x)
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
                      'scope': 'ONE_OWNED_LINK_EXPLICIT_OPTIONAL_STRUCTURE_ROUTE_DEFAULT_STRICT_FAILURE_RETAINED',
                      'binding_sha256': sha(binding), 'runtime_qualification': 'NOT_PROVEN'})
        print(json.dumps({'recipe': str(recipe), 'binding': str(binding), 'binding_sha256': sha(binding),
                          'run_host_cpu_seconds': 3600, 'run_host_wall_seconds': 3600}))
        return

    binding = Path(x.binding)
    need(sha(binding) == x.binding_sha256, 'prepared binding changed')
    c = json.loads(binding.read_text())
    need(c['state'] == 'PREPARED_NOT_RUN', 'wrong binding state')
    runtime_content_preflight(c)
    for name in ['prepare.py','main.rs','factory_controls.rs','structural_observation.rs',
                 'native_leaf_controls.rs','optional_controls.rs','ordinary_controls.rs','terminal_observation.rs',
                 'view_leaf_controls.rs','view_leaf_protocol.py']:
        need(c['files'].get(str(D/name)) == sha(D/name), 'V10 run binding does not bind this exact driver: '+name)
    out = Path(c['out'])
    out.mkdir()
    result = {'state': 'RUNNING', 'qualification': 'NOT_PROVEN', 'readiness': 'NOT_PROVEN',
              'H_head': c['H_head'], 'tool_R_head': c['tool_R_head'], 'runtime_R_pin': c['runtime_R_pin'],
              'binding_sha256': sha(binding), 'commands': [], 'tests_executed': 0, 'guests_executed': 0,
              'profile': 'liteinst-runtime', 'target': 'x86_64-unknown-linux-musl', 'produced_outputs': {}}
    if c.get('elf'):
        result['retained_ELF_declared_context'] = c['retained_ELF_declared_context']
    save(out / 'result.json', result)
    produced = result['produced_outputs']
    def command(name, argv, cwd, host=False, allow_failure=False, inspection=False, timeout=None):
        files_close(c)
        for path, identity in produced.items():
            need(file_fact(path) == identity, 'produced output changed: ' + path)
        env = declared_child_environment(c, host=host)
        limit = (1024 ** 3 if name.endswith('-relocations') else 128 * 1024 ** 2) if inspection else STREAM
        row = {'name': name, 'argv': argv, 'cwd': str(cwd), 'compiler_context': 'GNU_HOST_TOOL' if host else 'PRIVATE_SOURCE_EXPECTATION', 'output_limit_bytes': limit}
        result['commands'].append(row);save(out / 'result.json', result)
        declared_timecap = {'tool-metadata': 120, 'tool-build': 600, 'compile-driver': 120, 'owned-link': 2460}.get(name, 120)
        effective_timeout = min(declared_timecap, timeout) if timeout is not None else declared_timecap
        row['declared_command_timecap_seconds'] = declared_timecap
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
                    bounded_stream_command(argv, cwd, env, o, e, limit, effective_timeout, row)
                except BaseException as error:
                    # Capture before buffered stream __exit__ can fail.
                    command_cancelled = cancellation(command_cancelled, error)
                    raise
        except BaseException as error:
            command_cancelled = cancellation(command_cancelled, error)
            row['error'] = repr(error);raise
        finally:
            try:
                row['wall_seconds'] = time.monotonic() - started
                for stream in ['stdout', 'stderr']:
                    p = out / (name + '.' + stream)
                    if p.exists():
                        row[stream] = file_fact(p);produced[str(p)] = row[stream]
                # The typed method may return Err after preserving partial outputs.
                if name == 'owned-link':
                    for name0 in ['initializer-definitions.not-qualified.elf', 'initializer-definitions.map']:
                        p = Path(c['link_paths']['output_directory']) / name0
                        if p.exists():produced[str(p)] = file_fact(p)
                try: files_close(c)
                except BaseException as error:
                    command_cancelled = cancellation(command_cancelled, error)
                    raise
                finally: save(out / 'result.json', result)
            finally:
                if command_cancelled is not None:
                    raise command_cancelled
        need(all(row[s]['bytes'] <= limit for s in ['stdout', 'stderr']), name + ' retained stream exceeds guard')
        need(allow_failure or row['exit'] == 0, name + ' failed')
        return (out / (name + '.stdout')).read_text()
    cancelled = None
    try:
        handoff_source = runtime_handoff_preflight(c)
        need(handoff_source == c['runtime_handoff_source'] and
             c['files'].get(handoff_source['path']) == REQUIRED_HANDOFF_SOURCE_SHA256,
             'bound runtime handoff preflight identity differs')
        result['runtime_handoff_source_preflight'] = handoff_source
        files_close(c)
        result['private_before'] = private_close(c)
        save(out / 'result.json', result)
        declared_child_environment(c)
        census_version = command('census-objdump-version', [c['census_objdump'], '--version'], c['R'], host=True, timeout=10)
        census_first_line = census_version.splitlines()[0] if census_version.splitlines() else ''
        need('GNU objdump' in census_first_line and '2.35.2' in census_first_line, 'bound census tool version output differs')
        result['census_objdump_version'] = {'actual_first_line': census_first_line, 'root_reported_release': c['census_objdump_root_reported_version'], 'binary': file_fact(c['census_objdump'])}
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
        # --no-deps metadata intentionally omits dependencies, so derive the
        # exact dependency path through the emitted artifacts, not a glob.
        serde_records = [q for q in records if q.get('reason') == 'compiler-artifact' and
                         q.get('target', {}).get('name') == 'serde_json' and
                         q.get('target', {}).get('kind') == ['lib'] and
                         q.get('target', {}).get('crate_types') == ['lib'] and
                         q.get('profile', {}).get('test') is False and q.get('executable') is None]
        need(len(serde_records) == 1, 'current host serde_json library output ambiguous')
        serde_record = serde_records[0]
        locked_serde = c['serde_json_locked']
        need(serde_record['package_id'] == locked_serde['source'] + '#serde_json@' + locked_serde['version'],
             'serde_json not the actual current locked registry dependency')
        serde_manifest = Path(serde_record['manifest_path'])
        need(serde_manifest.is_relative_to(Path(c['cargo_home']) / 'registry/src') and
             serde_manifest.parent.name == 'serde_json-' + locked_serde['version'] and
             serde_manifest.resolve() == serde_manifest and serde_manifest.is_file() and
             serde_record['target']['src_path'] == str(serde_manifest.parent / 'src/lib.rs'),
             'serde_json actual source target/manifest differs')
        serde_rlibs = [Path(p) for p in serde_record['filenames'] if p.endswith('.rlib')]
        need(len(serde_rlibs) == 1, 'current serde_json rlib absent/ambiguous')
        for p in map(Path, serde_record['filenames']):
            need(p.is_absolute() and p.resolve() == p and p.is_file() and
                 p.is_relative_to(Path(c['target']) / 'debug'), 'serde_json output outside sole target/debug')
        serde_rlib = serde_rlibs[0]
        result['selected_serde_json_artifact'] = serde_record
        result['selected_serde_json_outputs_sha256'] = {p: sha(p) for p in serde_record['filenames']}
        for p in serde_record['filenames']: produced[p] = file_fact(p)
        save(out / 'selected-serde-json.json', serde_record)
        files_close(c)
        binary = Path(c['target']) / 'loader-static-diagnostic' / out.name / 'diagnostic'
        need(not binary.parent.exists(), 'fresh helper output directory required')
        need(binary.parent.resolve() == binary.parent, 'helper output redirects through symlink')
        binary.parent.mkdir(parents=True)
        deps = Path(c['target']) / 'debug/deps'
        need(deps.is_dir() and deps.resolve() == deps, 'actual host dependency directory absent/indirect')
        command('compile-driver', [c['host-rustc'], '--edition=2021', '--crate-name', 'static_source_diagnostic', str(D / 'main.rs'), '--extern', 'reverie_liteinst_artifact=' + str(rlib), '--extern', 'serde_json=' + str(serde_rlib), '-L', 'dependency=' + str(deps), '-o', str(binary)], c['R'], host=True)
        result['driver_binary'] = {'path': str(binary), 'sha256': sha(binary)}
        paths = c['link_paths']
        argv = [str(binary), c['H'], c['H_head'], c['runtime_R_pin'], c['cargo'], c['rustc'], c['target'],
                str(out / 'runner'), c['tool_R_head'], c['qualifier_source_sha256'], c['initializer_module_sha256'],
                paths['lld'], paths['script'], paths['libc'], paths['supporting_archives'][0], paths['memcpy_source'],
                paths['musl_source_authority'], paths['stack_chk_source'], paths['output_directory'],
                c['prepared_archive']['sha256'], str(c['prepared_archive']['bytes']), c['census_objdump'],
                c['native_leaf_sources']['memset'], c['native_leaf_sources']['set_thread_area'], c['native_leaf_module_sha256'],
                c['optional_sources']['std_weak'], c['optional_sources']['std_destructors'], c['optional_sources']['musl_dl_iterate'], c['ordinary_gate_module_sha256'],
                c['view_site_module_sha256'], c['view_table_module_sha256']]
        owned_error = None
        view_leaf_error = None
        diagnostic = ''
        try:
            diagnostic = command('owned-link', argv, c['H'], allow_failure=True)
            row = result['commands'][-1]
            result['owned_driver_exit'] = row['exit']
            result['owned_source_footer'] = diagnostic.splitlines().count('SOURCE_STATE=SOURCE_CLOSED_LINK_OBSERVATION_ONLY') == 1
            if row['exit'] != 0 or not result['owned_source_footer']:
                owned_error = 'owned source/build/link/selected-definition transaction refused'
            for role,name0 in [('ELF','initializer-definitions.not-qualified.elf'),('MAP','initializer-definitions.map')]:
                target0=Path(paths['output_directory'])/name0
                values=[line.split('=',1)[1] for line in diagnostic.splitlines() if line.startswith('ACTUAL_'+role+'_SHA256=')]
                if values:
                    need(len(values)==1 and str(target0) in produced and produced[str(target0)]['sha256']==values[0], 'actual owned output differs from post-source-close identity')
                elif row['exit']==0:raise RuntimeError('successful owned source omitted output identity '+role)
            # Reconcile any emitted terminal identity even if another independent
            # owned control refused. Do not silently lose initial-byte custody.
            terminal_path = out / 'runner/terminal-observer.not-proven.json'
            terminal_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines()
                               if line.startswith('TERMINAL_OBSERVER_JSON_SHA256=')]
            if terminal_hashes:
                need(len(terminal_hashes) == 1 and result['owned_source_footer'] and
                     terminal_path.stat().st_size <= 128 * 1024 * 1024 and
                     sha(terminal_path) == terminal_hashes[0],
                     'retained terminal initial/source-closed identity differs')
                produced[str(terminal_path)] = file_fact(terminal_path)
                result['terminal_serialized_file'] = produced[str(terminal_path)]
            # Parse leaf observations independently, including on owned failure.
            # A refused/malformed leaf protocol cannot suppress old inspections.
            try:
                # Seal any actual partial leaf evidence before parsing can refuse.
                # This finite path set is custody only, never a positive record.
                leaf_paths0 = [out / 'runner/view-leaf-controls.not-qualified.json']
                for role0 in ['site', 'table']:
                    leaf_paths0.append(out / ('runner/view-' + role0 + '-leaf.not-qualified.json'))
                    leaf_paths0.extend(out / ('runner/view-' + role0 + '-script.' + suffix0)
                                       for suffix0 in ['original.bin', 'journal.json'])
                for path0 in leaf_paths0:
                    if path0.exists(): produced[str(path0)] = file_fact(path0)
                result['view_leaf_partial_files'] = {str(path0): produced[str(path0)]
                    for path0 in leaf_paths0 if str(path0) in produced}
                parser_path = D / 'view_leaf_protocol.py'
                need(c['files'][str(parser_path)] == sha(parser_path), 'View parser source differs')
                spec0 = importlib.util.spec_from_file_location('owned_view_leaf_protocol', parser_path)
                parser0 = importlib.util.module_from_spec(spec0)
                spec0.loader.exec_module(parser0)
                slim = lambda f: {'path': f['path'], 'sha256': f['sha256']}
                output0 = Path(paths['output_directory'])
                parsed0 = parser0.parse(diagnostic, out / 'runner',
                    source_closed=result['owned_source_footer'], tool_head=c['tool_R_head'],
                    h_head=c['H_head'], runtime_pin=c['runtime_R_pin'],
                    site_sha=c['view_site_module_sha256'], table_sha=c['view_table_module_sha256'],
                    helper_sha=c['files'][str(D / 'view_leaf_controls.rs')],
                    archive_identity=slim(c['prepared_archive']),
                    elf_identity=slim(file_fact(output0/'initializer-definitions.not-qualified.elf')),
                    map_identity=slim(file_fact(output0/'initializer-definitions.map')))
                result['view_leaf_controls'] = parsed0
                for identity0 in parsed0['files']:
                    captured0 = file_fact(identity0['path'])
                    need(slim(captured0) == identity0, 'View parser retained evidence differs')
                    produced[identity0['path']] = captured0
                if parsed0['state'] != 'ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED':
                    view_leaf_error = 'actual View leaf controls refused'
            except Exception as error0:
                view_leaf_error = repr(error0)
                result['view_leaf_controls'] = {'state': 'FAIL_OR_NOT_REACHED', 'error': view_leaf_error,
                    'qualification': 'NOT_PROVEN', 'readiness': 'NOT_PROVEN'}
            structural_path = out / 'runner/explicit-structural.source-closed.json'
            structural_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines()
                                 if line.startswith('EXPLICIT_STRUCTURAL_SOURCE_CLOSED_SHA256=')]
            if row['exit'] == 0:
                need(len(structural_hashes) == 1 and sha(structural_path) == structural_hashes[0] and
                     result['owned_source_footer'], 'explicit structural source-closed receipt absent/changed')
                need(structural_path.stat().st_size <= 128 * 1024 * 1024, 'structural receipt exceeds bound')
                receipt = json.loads(structural_path.read_text())
                need(receipt['schema'] == 1 and receipt['state'] == 'SOURCE_CLOSED_EXPLICIT_STRUCTURAL_OBSERVATION_ONLY',
                     'structural receipt schema/state differs')
                for key,expected0 in [('tool_R_head',c['tool_R_head']),('runtime_H_head',c['H_head']),
                    ('runtime_R_pin',c['runtime_R_pin']),('qualifier_sha256',c['qualifier_source_sha256']),
                    ('initializer_module_sha256',c['initializer_module_sha256']),
                    ('native_leaf_module_sha256',c['native_leaf_module_sha256']),
                    ('ordinary_gate_module_sha256',c['ordinary_gate_module_sha256']),
                    ('main_source_sha256',sha(D/'main.rs')),
                    ('structural_source_sha256',sha(D/'structural_observation.rs')),
                     ('terminal_source_sha256',sha(D/'terminal_observation.rs'))]:
                    need(receipt[key] == expected0, 'structural receipt binding differs: '+key)
                need(receipt['driver_executable'] == result['driver_binary'] and
                     receipt['source_factory_close'] == receipt['archive_close'] == 'PASS',
                     'structural receipt executable/owner close differs')
                for value in [receipt,receipt['observation']]:
                    need(value['qualification'] == value['pre_fs_closure'] == value['readiness'] == 'NOT_PROVEN' and
                         value['guest_release'] is False and value['guests_executed'] == 0,
                         'structural receipt grants execution authority')
                terminal = receipt['terminal_observation']
                need(terminal['scope'] == 'FINITE_TERMINAL_OBSERVER_STATIC_OBSERVATIONS_ONLY' and
                     terminal['remaining'] == [
                         'GENERIC_PREPARATION_REJECTION_UNCHANGED',
                         'UD2_UNREACHABILITY_REQUIRES_EXCEPTION_PREDECESSOR_PROOF',
                         'PANIC_CANNOT_UNWIND_AND_OTHER_CALLEES_NOT_PROVEN',
                         'HOST_GATE_FILTER_SIGNAL_WAIT_AND_CANCEL_NOT_PROVEN',
                         'OBSERVER_SUCCESS_PREDECESSOR_NOT_PROVEN',
                         'PRE_FS_CLOSURE_AND_ENTRY_ADMISSION_NOT_PROVEN'],
                     'terminal observation erased open obligations or changed scope')
                terminal_path = out / 'runner/terminal-observer.not-proven.json'
                terminal_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines()
                                   if line.startswith('TERMINAL_OBSERVER_JSON_SHA256=')]
                need(len(terminal_hashes) == 1 and terminal_path.stat().st_size <= 128 * 1024 * 1024 and
                     sha(terminal_path) == terminal_hashes[0] and
                     json.loads(terminal_path.read_text()) == terminal,
                     'terminal initial serialized/source-closed identity differs')
                disassembly = terminal['disassembly']
                need(sha(disassembly['path']) == disassembly['sha256'], 'terminal disassembly custody differs')
                produced[str(terminal_path)] = file_fact(terminal_path)
                produced[disassembly['path']] = file_fact(disassembly['path'])
                result['terminal_observer'] = {'state':'FINITE_OBSERVATION_ONLY_NOT_PROVEN',
                    'observation':terminal,'file':produced[str(terminal_path)],
                    'module_sha256':c['terminal_module_sha256'],'qualification':'NOT_PROVEN'}
                obs = receipt['observation']
                need(obs['default_strict'] == {'state':'FAIL_RETAINED','error':'static final ELF has unresolved symbols'} and
                     obs['selected_binding_api'] == 'SAME_SOURCE_SELECTED_AND_OPTIONAL_ZERO' and
                     obs['structural_error'] is None and obs['combined_error'] is None and obs['close_errors'] == [] and
                     obs['explicit_structural'] is not None and obs['combined_entry_transfer_diagnostic'] is not None,
                     'structural/combined actual Rust result refused')
                combined=obs['combined_entry_transfer_diagnostic']
                need(combined['artifact'] == obs['artifact'] and
                     combined['objdump_tool'] == {'path':c['census_objdump'],'sha256':sha(c['census_objdump'])} and
                     combined['native_memcpy'] is not None, 'combined diagnostic actual roles differ')
                needed=[combined['artifact'],combined['objdump_tool'],combined['disassembly']]
                needed.extend(combined['native_memcpy'][key] for key in ['archive','source','source_inventory','link_map'])
                for identity0 in needed:
                    need(sha(identity0['path']) == identity0['sha256'], 'combined diagnostic custody changed')
                    produced[identity0['path']]=file_fact(identity0['path'])
                need(combined['native_memcpy']['link_map'] == obs['link_map'] and
                     combined['native_memcpy']['archive']['path'] == paths['libc'] and
                     combined['native_memcpy']['source']['path'] == paths['memcpy_source'] and
                     combined['native_memcpy']['source_inventory'] == {
                         'path':str(Path(paths['musl_source_authority']) / 'inventory.json'),
                         'sha256':c['files'][str(Path(paths['musl_source_authority']) / 'inventory.json')]},
                     'combined native inputs differ from actual bound link inputs')
                need(obs['runtime_archive'] == {'path':c['prepared_archive']['path'],'sha256':c['prepared_archive']['sha256']} and
                     obs['explicit_structural']['archive'] == obs['runtime_archive'] and
                     obs['explicit_structural']['artifact'] == obs['artifact'], 'structural actual archive/artifact binding differs')
                produced[str(structural_path)] = file_fact(structural_path)
                result['explicit_structural_source_closed_receipt'] = receipt
                result['explicit_structural_receipt_file'] = produced[str(structural_path)]
            records = []
            for p in sorted((out / 'runner').rglob('*')):
                if p.is_file():produced[str(p)] = file_fact(p)
                if p.name.endswith('.command.json'):records.append(json.loads(p.read_text()))
            links = [q for q in records if q['program'] == paths['lld']]
            result['actual_lld_invocations'] = len(links)
            need(len(links) == 1, 'actual owned transaction did not perform exactly one LLD invocation')
            census_path = str(out / 'runner/startup-census.not-proven.json')
            census_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines() if line.startswith('STARTUP_CENSUS_JSON_SHA256=')]
            if census_hashes:
                need(len(census_hashes) == 1 and census_path in produced and produced[census_path]['sha256'] == census_hashes[0], 'census JSON differs from Rust serialized/source-closed identity')
                result['startup_census_source_closed_identity'] = produced[census_path]
            elif row['exit'] == 0:
                raise RuntimeError('successful owned transaction omitted census JSON identity')
            leaf_rows = [line.split('=',1)[1].split('\t',1) for line in diagnostic.splitlines()
                         if line.startswith('NATIVE_LEAF_RETAINED_FILE=')]
            need(len({p for digest,p in leaf_rows}) == len(leaf_rows), 'duplicate native leaf retained path')
            for digest, path in leaf_rows:
                p = Path(path)
                if path not in produced and p.is_relative_to(Path(c['target'])):
                    produced[path] = file_fact(p)
                need(re.fullmatch('[0-9a-f]{64}',digest) and path in produced and
                     produced[path]['sha256'] == digest, 'native leaf retained identity changed')
            result['native_leaf_retained_source_closed_files'] = {p: produced[p] for digest,p in leaf_rows}
            result['native_leaf_status_lines'] = [line for line in diagnostic.splitlines()
                if line.startswith(('NATIVE_LEAVES=', 'NATIVE_LEAF_SUCCESS_CLOSE=',
                    'NATIVE_LEAF_CALLBACK_ERROR_CLOSE=', 'NATIVE_LEAF_ARCHIVE_CLOSE='))]
            optional_rows = [line.split('=',1)[1].split('\t',1) for line in diagnostic.splitlines()
                             if line.startswith('OPTIONAL_ZERO_RETAINED_FILE=')]
            need(len({path for digest,path in optional_rows}) == len(optional_rows), 'duplicate optional retained path')
            for digest, path in optional_rows:
                target = Path(path)
                need(path in c['files'] or target.is_relative_to(out) or target.is_relative_to(Path(c['target'])),
                     'optional retained path outside bound inputs/outputs')
                if path not in produced:
                    produced[path] = file_fact(path)
                need(re.fullmatch('[0-9a-f]{64}',digest) and produced[path]['sha256'] == digest,
                     'optional retained identity differs')
            result['optional_zero_retained_source_closed_files'] = {path:produced[path] for digest,path in optional_rows}
            result['optional_zero_status_lines'] = [line for line in diagnostic.splitlines() if line.startswith('OPTIONAL_ZERO_') or line.startswith('OPTIONAL_ZERO=')]
            optional_debug = str(out / 'runner/optional-zero.not-proven.debug')
            optional_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines() if line.startswith('OPTIONAL_ZERO_DEBUG_SHA256=')]
            if optional_hashes:
                need(len(optional_hashes) == 1 and optional_debug in produced and
                     produced[optional_debug]['sha256'] == optional_hashes[0], 'optional debug differs from source-closed Rust identity')
                result['optional_zero_observation'] = {'state':'OBSERVED_NOT_PROVEN','file':produced[optional_debug],
                    'qualification':'NOT_PROVEN','control_scope':'RETURNED_LEDGER_VERIFICATION_NOT_CALLBACK_CLOSE'}
            ordinary_path = str(out / 'runner/ordinary-gate.not-proven.json')
            ordinary_hashes = [line.split('=',1)[1] for line in diagnostic.splitlines() if line.startswith('ORDINARY_GATE_JSON_SHA256=')]
            ordinary_observation = None
            ordinary_identity_files = {}
            if ordinary_hashes:
                need(len(ordinary_hashes) == 1 and ordinary_path in produced and
                     produced[ordinary_path]['sha256'] == ordinary_hashes[0], 'ordinary JSON differs from source-closed Rust identity')
                ordinary_observation = json.loads(Path(ordinary_path).read_text())
                need(ordinary_observation['qualification'] == ordinary_observation['readiness'] == 'NOT_PROVEN' and
                     ordinary_observation['guest_release'] is False, 'ordinary observation changes authority scope')
                provider = ordinary_observation['provider']
                need(provider['identity_policy'] == 'LIVE_SOURCE_ARC_CARGO_ARCHIVE_DERIVED_COMPLETE_MEMBER_IDENTITY',
                     'ordinary provider did not use actual live-derived member identity')
                physical_members = {}
                for label,prefix in [('ordinary',''),('page','page_')]:
                    name = provider[prefix + 'member_name']
                    digest = provider[prefix + 'member_sha256']
                    ordinal = provider[prefix + 'member_ordinal']
                    header = provider[prefix + 'member_header']
                    need(isinstance(name,str) and name not in ('','.','..') and Path(name).name == name and
                         not any(ch in name for ch in '\x00\r\n/\\'), 'ordinary physical member name invalid')
                    need(isinstance(digest,str) and re.fullmatch('[0-9a-f]{64}',digest) and
                         type(ordinal) is int and ordinal >= 0 and type(header) is int and header >= 0,
                         'ordinary complete derived member identity invalid')
                    physical_members[label] = {'name':name,'sha256':digest,'ordinal':ordinal,'header':header}
                expected_elf = str(Path(paths['output_directory'])/'initializer-definitions.not-qualified.elf')
                expected_map = str(Path(paths['output_directory'])/'initializer-definitions.map')
                need(provider['runtime_archive'] == {'path':c['prepared_archive']['path'],'sha256':c['prepared_archive']['sha256']} and
                     provider['artifact'] == {'path':expected_elf,'sha256':produced[expected_elf]['sha256']} and
                     provider['link_map'] == {'path':expected_map,'sha256':produced[expected_map]['sha256']},
                     'ordinary actual runtime archive/ELF/map identity differs')
                result['ordinary_gate_physical_members'] = {'identity_policy':provider['identity_policy'], **physical_members}
                source_roles = [
                    ('reverie-inguest/src/trap.rs','5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
                    ('reverie-inguest/src/trap/fixed_gates.rs','ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54')]
                need(len(provider['sources']) == 2, 'ordinary actual source roles incomplete')
                for value,(suffix,digest) in zip(provider['sources'],source_roles):
                    need(Path(value['path']).is_absolute() and value['path'].endswith('/'+suffix) and value['sha256'] == digest,
                         'ordinary source role differs from actual factory source')
                for value in [provider['runtime_archive'],provider['artifact'],provider['link_map'],*provider['sources']]:
                    ordinary_identity_files[value['path']] = value['sha256']
                transfer = ordinary_observation['entry_transfer_diagnostic']
                if transfer is not None:
                    need(ordinary_observation['transfer_error'] is None and
                         ordinary_observation['transfer_refusal'] == transfer['transfer_refusal'], 'ordinary transfer refusal was omitted/changed')
                    for key in ['artifact','objdump_tool','disassembly']:
                        value = transfer[key];ordinary_identity_files[value['path']] = value['sha256']
                result['ordinary_gate_observation'] = {'state':ordinary_observation['state'],
                    'file':produced[ordinary_path], 'transfer_refusal':ordinary_observation['transfer_refusal'],
                    'transfer_error':ordinary_observation['transfer_error'], 'qualification':'NOT_PROVEN'}
            ordinary_rows = [line.split('=',1)[1].split('\t',1) for line in diagnostic.splitlines()
                             if line.startswith('ORDINARY_GATE_RETAINED_FILE=')]
            need(len({path for digest,path in ordinary_rows}) == len(ordinary_rows), 'duplicate ordinary retained path')
            for digest,path in ordinary_rows:
                need(path in c['files'] or path in ordinary_identity_files or Path(path).is_relative_to(out) or
                     Path(path).is_relative_to(Path(c['target'])), 'ordinary retained path outside actual recorded source/output roles')
                if path not in produced:produced[path] = file_fact(path)
                need(re.fullmatch('[0-9a-f]{64}',digest) and produced[path]['sha256'] == digest,
                     'ordinary retained file identity changed')
            result['ordinary_gate_retained_source_closed_files'] = {path:produced[path] for digest,path in ordinary_rows}
            result['ordinary_gate_status_lines'] = [line for line in diagnostic.splitlines() if line.startswith('ORDINARY_GATE')]
            if row['exit'] == 0:
                for marker in ['ORDINARY_GATE=OBSERVED_NOT_PROVEN',
                    'ORDINARY_GATE_SUCCESS_CLOSE=OBSERVED_NOT_PROVEN',
                    'ORDINARY_GATE_CALLBACK_ERROR_CLOSE=EXACT_CALLBACK_ERROR_AND_SUCCESSFUL_CLOSE']:
                    need(diagnostic.splitlines().count(marker) == 1, 'missing ordinary same-owner observation/control: '+marker)
                need(len(ordinary_hashes) == 1 and ordinary_observation['state'] == 'OBSERVED_NOT_PROVEN' and
                     ordinary_observation['entry_transfer_diagnostic'] is not None,
                     'successful owned driver omitted complete ordinary caller diagnostic')
                expected_ordinary = set(ordinary_identity_files) | {ordinary_path,str(out / 'runner/ordinary-gate-controls.debug')}
                need({path for digest,path in ordinary_rows} == expected_ordinary, 'ordinary retained file inventory incomplete')
                for path,digest in ordinary_identity_files.items():
                    need(path in produced and produced[path]['sha256'] == digest, 'ordinary source/tool/ELF/disassembly custody differs')
                for marker in [
                    'OPTIONAL_ZERO=OBSERVED_WITH_TWO_RETURNED_LEDGER_CONTROLS_NOT_PROVEN',
                    'OPTIONAL_ZERO_FULL_PRODUCER_LEDGER=OBSERVED_NOT_PROVEN',
                    'OPTIONAL_ZERO_SOURCE_LEDGER_CLOSE=EXACT_RETURNED_FILE_REFUSAL_RESTORED',
                    'OPTIONAL_ZERO_EXTRA_LINK_LEDGER_CLOSE=EXACT_RETURNED_FILE_REFUSAL_RESTORED',
                    'OPTIONAL_ZERO_CALLBACK_CLOSE=NOT_TESTED_NO_CALLBACK_API']:
                    need(diagnostic.splitlines().count(marker) == 1, 'missing actual optional observation/control: ' + marker)
                expected_optional = set(c['optional_sources'].values()) | {paths['script'], optional_debug}
                copied_names = ['selected-link-script.ld','std-weak-syscall.rs','std-destructors.rs','musl-dl-iterate-phdr.c']
                expected_optional.update(str(out / 'runner/optional-inputs' / name) for name in copied_names)
                for role in ['SOURCE_LEDGER','EXTRA_LINK_LEDGER']:
                    expected_optional.update(str(out / ('runner/optional-' + role + suffix))
                        for suffix in ['.original.bin','.journal.json','.restore.debug'])
                need({path for digest,path in optional_rows} == expected_optional,
                     'optional actual source/copy/backup/journal/receipt inventory differs')
                need(len(optional_hashes) == 1, 'successful owned transaction omitted optional observation identity')
                for marker in ['NATIVE_LEAVES=OBSERVED_WITH_THREE_CLOSE_CONTROLS_NOT_PROVEN',
                    'NATIVE_LEAF_SUCCESS_CLOSE=OBSERVED_NOT_PROVEN',
                    'NATIVE_LEAF_CALLBACK_ERROR_CLOSE=EXACT_ERROR_RESTORED_INPUTS',
                    'NATIVE_LEAF_ARCHIVE_CLOSE=EXACT_SELECTED_SCRIPT_CHANGE_REFUSAL_RESTORED']:
                    need(diagnostic.splitlines().count(marker) == 1, 'missing actual native leaf control: ' + marker)
                need(len(leaf_rows) >= 8, 'native leaf closed identity evidence incomplete')
                need(diagnostic.splitlines().count('OWNED_JOIN=SELECTED_DEFINITIONS_ONLY') == 1, 'missing selected definition outcome')
                need(diagnostic.splitlines().count('TERMINAL_OBSERVER=FINITE_OBSERVATION_ONLY_NOT_PROVEN') == 1, 'finite same-owner terminal observation missing/refused')
                need(sum(line.startswith('FACTORY_NEGATIVE_') for line in diagnostic.splitlines()) == 5, 'actual five refusal controls incomplete')
                need(diagnostic.splitlines().count('FACTORY_NEGATIVE_OWNER=EXACT_REFUSAL_LINK_INSPECT_CENSUS_AND_ORDINARY SAME_ACTUAL_METADATA CONSUMER_ASSERTIONS=8') == 1, 'actual eight consumer assertions incomplete')
                need(diagnostic.splitlines().count('STARTUP_CENSUS=OBSERVED_NOT_PROVEN') == 1, 'same-owner startup census missing/refused')
                census_path = str(out / 'runner/startup-census.not-proven.json')
                need(census_path in produced and produced[census_path]['bytes'] > 0, 'same-source census JSON not retained')
                result['startup_census'] = {'state': 'OBSERVED_NOT_PROVEN', 'file': produced[census_path], 'qualification': 'NOT_PROVEN'}
                for key,expected in [('ARCHIVE_PATH',c['prepared_archive']['path']),('ARCHIVE_SHA256',c['prepared_archive']['sha256']),('ARCHIVE_BYTES',str(c['prepared_archive']['bytes']))]:
                    need(diagnostic.splitlines().count(key+'='+expected) == 1, 'fresh owned archive observation differs: '+key)
                digests = {}
                for key in ['METADATA_SHA256','CARGO_MESSAGES_SHA256']:
                    values=[line.split('=',1)[1] for line in diagnostic.splitlines() if line.startswith(key+'=')]
                    need(len(values)==1 and re.fullmatch('[0-9a-f]{64}',values[0]),'missing fresh factory digest '+key)
                    digests[key]=values[0];result[key.lower()]=values[0]
                manifest = str(Path(c['H']) / 'detcore-liteinst/Cargo.toml')
                features = 'exec-native-injection,exec-private-init,loader-private-runtime'
                metadata_args = ['metadata','--locked','--offline','--format-version=1','--manifest-path',manifest,
                                 '--filter-platform','x86_64-unknown-linux-musl','--features',features]
                archive_args = ['rustc','--locked','--offline','--manifest-path',manifest,'--lib','--target',
                                'x86_64-unknown-linux-musl','--profile','liteinst-runtime','--features',features,
                                '--crate-type','staticlib','--message-format=json-render-diagnostics','--target-dir',c['target']]
                # Original outer and nested owner controls resolve before the link.
                # The shared View foreign-owner context resolves after old consumers.
                # Require all three complete identities and the sole archive build.
                role_indices = {}
                for role,args,count,key in [('metadata',metadata_args,3,'METADATA_SHA256'),
                                            ('archive_build',archive_args,1,'CARGO_MESSAGES_SHA256')]:
                    exact = [i for i,q in enumerate(records) if q['program']==c['cargo'] and
                             q['arguments']==args and q['cwd']==c['H']]
                    matching = [i for i,q in enumerate(records) if q['stdout']['sha256']==digests[key]]
                    need(len(exact)==count and exact==matching, 'fresh '+role+' command/hash identities missing or ambiguous')
                    for i in exact:
                        q=records[i];need(q['status']==0,'fresh '+role+' command did not succeed')
                        for stream in ['stdout','stderr']:
                            fact=q[stream]
                            need(fact['path'] in produced and produced[fact['path']]['sha256']==fact['sha256'],
                                 'fresh '+role+' raw stream identity changed')
                    role_indices[role]=exact
                need(role_indices['metadata'][0] < role_indices['archive_build'][0] <
                     role_indices['metadata'][1] < records.index(links[0]),
                     'actual outer metadata/build/nested source/link order differs')
                need(records.index(links[0]) < role_indices['metadata'][2],
                     'actual View foreign-owner source did not resolve after the sole link')
                result['factory_stream_roles']={role:[{'runner_record_index':i,'command':records[i]} for i in indices]
                                                for role,indices in role_indices.items()}

        except Exception as error:
            owned_error = repr(error)
        if view_leaf_error is not None:
            owned_error = repr({'original_owned_error': owned_error, 'view_leaf_error': view_leaf_error})
        result['owned_transaction_error'] = owned_error
        save(out / 'result.json', result)
        # Always attempt the unchanged strict V12 inspection on any preserved
        # actual ELF after a join refusal. Custody failures remain terminal.
        inspection_error = None
        try:
            inspect_produced(c, out, result, produced, command)
            result['final_inspection'] = 'PASS_EXPLICIT_OPTIONAL_STRUCTURE_ONLY_DEFAULT_STRICT_FAIL_RETAINED'
        except Exception as error:
            inspection_error = repr(error)
            result.update(final_inspection='FAIL_OR_NOT_RUN', final_inspection_error=inspection_error)
        need(sha(binary) == result['driver_binary']['sha256'], 'driver changed')
        need(all(sha(q) == h for q,h in result['selected_outputs_sha256'].items()), 'selected host outputs changed')
        need(owned_error is None and inspection_error is None,
             'owned link and strict inspection aggregate refused: ' + repr([owned_error,inspection_error]))
        result['state'] = 'PASS_OWNED_FINAL_LINK_INSPECTION_ONLY'
        result['optional_static_zero_guard_proof'] = 'FINITE_SAME_OWNER_OBSERVATION_ONLY_NOT_EXECUTION_PROOF'
        result['pre_fs_closure'] = 'NOT_PROVEN'
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
        for path, expected in produced.items():
            try: need(file_fact(path) == expected, 'produced evidence changed: ' + path)
            except BaseException as error:
                close_errors.append('output custody: ' + repr(error))
                cancelled = cancellation(cancelled, error)
        result['source_tools_closed'] = not close_errors
        if close_errors:
            result.update(state='FAIL', close_errors=close_errors)
        try:
            save(out / 'result.json', result)
        finally:
            if cancelled is not None:
                raise cancelled
    need(result['state'] == 'PASS_OWNED_FINAL_LINK_INSPECTION_ONLY', 'owned link/inspection/source close failed')


if __name__ == '__main__':
    main()
