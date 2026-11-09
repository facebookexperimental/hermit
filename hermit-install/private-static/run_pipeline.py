#!/usr/bin/env python3
"""Run a prepared private-static recipe through stock dagrun and the PID helper."""

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True

GIB = 1024 ** 3
MEMORY_BYTES = 16 * GIB
DISK_RESERVATION_BYTES = 24 * GIB
DISK_FLOOR_BYTES = 537_000_000_000
CONSOLE_BYTES = 16 * 1024 ** 2
CPU_SECONDS = WALL_SECONDS = 3600
OUTER_SECONDS = 3720
POLL_SECONDS = 2
TERM_SECONDS = 15
PID_HELPER_SHA256 = '3c87bfa780d378b9e8313c904ae6fa91ae3e2e57de12275b340122a41c8cccbb'
EMPTY_SHA256 = hashlib.sha256(b'').hexdigest()
SCOPE = 'PRIVATE_STATIC_BUILD_LINK_DATA_PACKAGE_NOT_EXECUTION_QUALIFICATION'


def need(value, message):
    if not value:
        raise RuntimeError(message)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(path, directory=False):
    path = Path(path)
    need(path.is_absolute() and path.resolve() == path,
         'canonical absolute path required: ' + str(path))
    need(path.is_dir() if directory else path.is_file(),
         'existing directory required: ' + str(path) if directory
         else 'existing regular file required: ' + str(path))
    return path


def file_identity(path, expected=None):
    path = canonical(path)
    digest = sha(path)
    if expected is not None:
        need(isinstance(expected, str) and re.fullmatch('[0-9a-f]{64}', expected)
             and digest == expected, 'file SHA256 differs: ' + str(path)
             + '; expected=' + str(expected) + ', observed=' + digest
             + '; provide the canonical file and its independently measured digest,'
             ' then prepare a fresh binding')
    return dict(path=str(path), bytes=path.stat().st_size, sha256=digest)


def save(path, value):
    with Path(path).open('x') as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write('\n')


def git(source, *args):
    environment = dict(os.environ, GIT_OPTIONAL_LOCKS='0')
    return subprocess.check_output(
        ['git', '-c', 'diff.autoRefreshIndex=false', '-C', str(source), *args],
        env=environment, timeout=30)


def source_snapshot(source, expected_head):
    source = canonical(source, directory=True)
    head = git(source, 'rev-parse', 'HEAD').decode().strip()
    need(re.fullmatch('[0-9a-f]{40}', expected_head) and head == expected_head,
         'source HEAD differs: ' + str(source)
         + '; use the final reviewed checkout/head and regenerate the producer config')
    diff = git(source, 'diff', '--binary', 'HEAD')
    names = git(source, 'ls-files', '--others', '--exclude-standard', '-z').split(b'\0')
    return dict(path=str(source), head=head,
                tree=git(source, 'rev-parse', 'HEAD^{tree}').decode().strip(),
                diff_sha256=hashlib.sha256(diff).hexdigest(),
                untracked={os.fsdecode(name): sha(source / os.fsdecode(name))
                           for name in names if name})


def read_recipe(path, expected):
    identity = file_identity(path, expected)
    recipe = json.loads(Path(path).read_text())
    need(isinstance(recipe, dict), 'recipe must be an object')
    need(isinstance(recipe.get('argv'), list) and recipe['argv']
         and all(isinstance(v, str) and '\0' not in v for v in recipe['argv']),
         'recipe argv must be a nonempty string list')
    canonical(recipe['cwd'], directory=True)
    need(isinstance(recipe.get('env'), dict)
         and all(isinstance(k, str) and isinstance(v, str) and k and '=' not in k
                 and '\0' not in k + v for k, v in recipe['env'].items()),
         'recipe environment must contain string entries')
    need(isinstance(recipe.get('unset'), list)
         and all(isinstance(v, str) and v and '=' not in v and '\0' not in v
                 for v in recipe['unset']), 'recipe unset must contain environment names')
    need(type(recipe.get('memory_gib')) is int and recipe['memory_gib'] == 16
         and type(recipe.get('reservation_gib')) is int
         and recipe['reservation_gib'] == 24, 'unchanged memory16GiB/disk24GiB required')
    need(recipe.get('scope') == SCOPE, 'private-static DATA recipe scope differs')
    return recipe, identity


def pipeline_binding(recipe, source, adapter):
    argv = recipe['argv']
    pipeline = source / 'hermit-install/private-static/owned/pipeline.py'
    need(len(argv) == 7 and argv[1] == str(pipeline)
         and argv[2:4] == ['run', '--binding'] and argv[5] == '--binding-sha256',
         'expected the maintained pipeline run recipe')
    binding_identity = file_identity(argv[4], argv[6])
    bound = json.loads(Path(argv[4]).read_text())
    need(bound['state'] == 'PREPARED_NOT_RUN', 'pipeline binding is not prepared')
    config = bound['config']
    paths = config['paths']
    need(paths['H'] == str(source), 'bound H differs from requested source')
    need(paths['runner'] == adapter['path'] and config['digests']['runner'] == adapter['sha256'],
         'config runner must identify this adapter, separately from dagrun')
    need(paths['target'] == str(source / 'target'), 'producer target must remain H/target')
    canonical(paths['target'], directory=True)
    canonical(paths['cargo_home'], directory=True)
    need(recipe['cwd'] == paths['tool_R'], 'recipe cwd differs from bound tool source')
    inputs = {}
    for name, digest in bound['sources'].items():
        inputs[name] = file_identity(name, digest)
    config_origins = []
    for name in inputs:
        if name.endswith('.json'):
            try:
                if json.loads(Path(name).read_text()) == config:
                    config_origins.append(inputs[name])
            except (ValueError, UnicodeError):
                pass
    return bound, binding_identity, inputs, config_origins


def engine_identity(args, source, environment):
    engine = file_identity(args.engine, args.engine_sha256)
    stamp = file_identity(args.engine_provenance, args.engine_provenance_sha256)
    fields = {}
    counts = dict(source_fp=0, binary_sha256=0)
    for line in Path(args.engine_provenance).read_text().splitlines():
        key, separator, value = line.partition('=')
        if separator and key in ('source_fp', 'binary_sha256', 'git_head',
                                 'git_rs_tree', 'toolchain_fp'):
            fields[key] = value
            if key in counts:
                counts[key] += 1
    need(all(v == 1 for v in counts.values()),
         'invalid or duplicate engine provenance source_fp/binary_sha256;'
         ' supply the normal stock launcher stamp for this binary')
    # This is the stock launcher's source/binary stamp, not an adapter-created receipt.
    need(fields.get('binary_sha256') == engine['sha256'],
         'engine does not match its stock provenance stamp')
    utilities = canonical(source / 'agent-utils', directory=True)
    pin = git(source, 'rev-parse', 'HEAD:agent-utils').decode().strip()
    snapshot = source_snapshot(utilities, pin)
    rs_diff = git(utilities, 'diff', '--binary', 'HEAD', '--', 'rs')
    rs_untracked = git(utilities, 'ls-files', '--others', '--exclude-standard', '-z',
                       '--', 'rs', ':!rs/bin', ':!rs/target')
    need(not rs_diff and not rs_untracked,
         'stock engine Rust source differs from the H-pinned checkout;'
         ' use the clean pinned source and its normal built binary/stamp')
    need(fields.get('git_head') == pin
         and fields.get('git_rs_tree') == git(utilities, 'rev-parse', 'HEAD:rs').decode().strip(),
         'stock engine provenance does not name the H-pinned source')
    helper = utilities / 'common/bin/rs-source-fingerprint'
    helper_identity = file_identity(helper)
    tracked = git(utilities, 'show', pin + ':common/bin/rs-source-fingerprint')
    need(helper_identity['sha256'] == hashlib.sha256(tracked).hexdigest(),
         'stock source fingerprint helper changed')
    fingerprint = subprocess.check_output(
        [str(helper)], cwd=utilities, env=environment, timeout=30).decode().strip()
    need(re.fullmatch('[0-9a-f]{64}', fingerprint)
         and fields.get('source_fp') == fingerprint, 'stock engine source fingerprint differs')
    return dict(binary=engine, provenance=stamp, stamp_fields=fields,
                source=snapshot, fingerprint_helper=helper_identity)


def outer_environment(config, tmpdir, cache):
    environment = dict(os.environ)
    for name in ('CARGO_TARGET_DIR', 'RUSTFLAGS', 'RUSTDOCFLAGS', 'LD_PRELOAD',
                 'LD_LIBRARY_PATH', 'HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER',
                 'HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER', 'DAGRUN_CPU_TIMEOUT_MULTIPLIER'):
        environment.pop(name, None)
    overrides = dict(CARGO_HOME=config['paths']['cargo_home'], CARGO_NET_OFFLINE='true',
                     TMPDIR=str(tmpdir), XDG_CACHE_HOME=str(cache),
                     PYTHONDONTWRITEBYTECODE='1', CI_DAG_BUILD_JOBS='8',
                     CARGO_TARGET_DIR=config['paths']['target'], CARGO_INCREMENTAL='0',
                     RUST_MIN_STACK='16777216')
    environment.update(overrides)
    return environment, overrides


def make_dag(name, output, adapter, python):
    command = [python, str(output / 'pid-bound-v2.py'), '--evidence',
               str(output / 'a6-pids.json'), '--limit', '512', '--reserved-cpus', '8',
               '--memory-bytes', str(MEMORY_BYTES), '--quota-cpus', '8', '--',
               python, adapter, '_payload', '--output', str(output),
               '--recipe-sha256', sha(output / 'recipe.json')]
    return dict(description='Private-static host build/link/DATA package; unchanged resource bounds',
                steps=[dict(group='private-static', job=name, cmd=shlex.join(command), deps=[],
                            jobs_flag='', jobs_env='',
                            timeout=WALL_SECONDS, cpu_timeout=CPU_SECONDS,
                            env=dict(CARGO_INCREMENTAL='0', CARGO_BUILD_JOBS='4',
                                     CI_DAG_BUILD_JOBS='4', PYTHONDONTWRITEBYTECODE='1'),
                            hint=dict(classification='cpu-bound', est_duration_s=300,
                                      hard_mem_max_bytes=MEMORY_BYTES,
                                      rss_baseline_bytes=10 * GIB, preferred_inner_jobs=4))])


def engine_command(engine, output):
    return [engine, 'run', '--dag', str(output / 'dag.json'), '-s', '1', '-j', '8',
            '--max-mem', '16G', '--cores', '8', '--run-timeout', str(OUTER_SECONDS),
            '--cpu-timeout-multiplier', '1', '--perf-dir', str(output / 'perf'),
            '--profile', '--no-profile-feedback', '-v']


def available_bytes(path):
    status = os.statvfs(path)
    return status.f_bavail * status.f_frsize


def projected_disk(available):
    need(available - DISK_RESERVATION_BYTES >= DISK_FLOOR_BYTES,
         'projected disk floor refuses: ' + str(available)
         + '; choose an authorized filesystem with at least 562769803776 available bytes'
         ' or wait for space to be recovered; do not lower the floor or reservation')


def guard_breach(available, console_bytes):
    if available < DISK_FLOOR_BYTES:
        return 'disk available ' + str(available) + ' below 537000000000'
    if console_bytes > CONSOLE_BYTES:
        return 'console exceeds 16 MiB'
    return None


def native_status(returncode):
    if returncode is None:
        return None
    return dict(returncode=returncode, exit_code=returncode if returncode >= 0 else None,
                signal=-returncode if returncode < 0 else None,
                invocation_failure=returncode in (126, 127))


def interrupted(signum, _frame):
    raise InterruptedError('received signal ' + str(signum))


def invocation_failure(error):
    if isinstance(error, OSError):
        if error.errno == errno.ENOENT:
            return dict(exit_code=127, error=str(error),
                        remedy='provide an existing executable at the recorded command path')
        if error.errno in (errno.EACCES, errno.ENOEXEC):
            return dict(exit_code=126, error=str(error),
                        remedy='provide an executable command with its supported interpreter')
    return None


def payload_exit(result):
    status = result['native']
    if status is not None and not result['errors']:
        return status['returncode'] if status['returncode'] >= 0 else 128 - status['returncode']
    if result['invocation_failure'] is not None:
        return result['invocation_failure']['exit_code']
    return 1


def join_child_receipts(child, pid):
    need(child['native'] is not None and not child['errors'],
         'payload did not retain a completed native child status')
    # The helper waits for the Python payload, which reports a signalled native
    # child as 128+signal; retain both observations rather than confusing them.
    need(pid.get('child_exit') == payload_exit(child)
         and 'memory_event_assessment' in pid, 'unchanged PID helper did not complete')


def terminate(process, grace=TERM_SECONDS):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return process.wait()
    try:
        return process.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        return process.wait()


def monitor_engine(command, source, environment, output, *, free=available_bytes,
                   sleep=time.sleep, poll_seconds=POLL_SECONDS, term_seconds=TERM_SECONDS,
                   disk_path=None):
    """The original disk/console poll and process-group TERM/KILL policy."""
    console = output / 'console.log'
    started = time.monotonic()
    process = None
    invocation = None
    errors = []
    available = None
    disk_path = source if disk_path is None else disk_path
    with console.open('xb') as stream:
        try:
            process = subprocess.Popen(command, cwd=source, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=stream,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            while process.poll() is None:
                available = free(disk_path)
                breach = guard_breach(available, console.stat().st_size)
                if breach:
                    errors.append(breach)
                    break
                sleep(poll_seconds)
        except BaseException as error:
            errors.append(type(error).__name__ + ': ' + str(error))
            if process is None:
                invocation = invocation_failure(error)
        finally:
            if process is not None:
                try:
                    if process.poll() is None:
                        terminate(process, term_seconds)
                    process.wait()
                except BaseException as error:
                    errors.append('cleanup ' + type(error).__name__ + ': ' + str(error))
    # Fast completion must not turn the very same floor/cap breach into a pass.
    try:
        available = free(disk_path)
        breach = guard_breach(available, console.stat().st_size)
        if breach and breach not in errors:
            errors.append(breach)
    except Exception as error:
        errors.append(type(error).__name__ + ': ' + str(error))
    return dict(native=native_status(process.returncode if process is not None else None),
                invocation_failure=invocation,
                wall_seconds=time.monotonic() - started, guard_breaches=errors,
                disk_guard_path=str(disk_path), disk_available_after_bytes=available,
                console_bytes=console.stat().st_size)


def payload(output, expected):
    """Internal DAG payload; public launches use the full adapter and PID helper."""
    started = time.monotonic()
    process = None
    invocation = None
    errors = []
    try:
        recipe, _ = read_recipe(output / 'recipe.json', expected)
        environment = dict(os.environ)
        for name in recipe['unset']:
            environment.pop(name, None)
        environment.update(recipe['env'])
        with (output / 'stdout').open('xb') as stdout, (output / 'stderr').open('xb') as stderr:
            process = subprocess.Popen(recipe['argv'], cwd=recipe['cwd'], env=environment,
                                       stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
            process.wait()
    except BaseException as error:
        errors.append(type(error).__name__ + ': ' + str(error))
        if process is None:
            invocation = invocation_failure(error)
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=TERM_SECONDS)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    result = dict(native=native_status(process.returncode if process is not None else None),
                  invocation_failure=invocation, wall_seconds=time.monotonic() - started,
                  errors=errors)
    save(output / 'child.json', result)
    return result


def read_record(path):
    identity = file_identity(path)
    return dict(identity=identity, value=json.loads(Path(path).read_text()))


def execute(args, output):
    started = time.monotonic()
    stamp = dict(adapter=file_identity(Path(__file__).resolve()), scope=SCOPE,
                 started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                 product_tests_claimed=0)
    errors = []
    source_before = {}
    bound_files = {}
    environment = None
    engine_before = None
    try:
        need(args.cpu == CPU_SECONDS and args.wall == WALL_SECONDS,
             'private pipeline retains CPU3600/wall3600')
        source = canonical(args.source, directory=True)
        tmpdir = canonical(args.tmpdir, directory=True)
        cache = canonical(args.xdg_cache_home, directory=True)
        recipe, recipe_identity = read_recipe(args.recipe, args.recipe_sha256)
        bound, binding_identity, inputs, config_origins = pipeline_binding(
            recipe, source, stamp['adapter'])
        config = bound['config']
        need(config['heads']['H'] == args.source_head, 'independent H head differs from config')
        environment, overrides = outer_environment(config, tmpdir, cache)
        for role in ('H', 'runtime_R', 'tool_R'):
            source_before[role] = source_snapshot(config['paths'][role], config['heads'][role])
        engine_before = engine_identity(args, source, environment)
        helper = file_identity(args.pid_helper, args.pid_helper_sha256)
        need(args.pid_helper_sha256 == PID_HELPER_SHA256, 'unchanged PID helper source required')
        bound_files = dict(inputs)
        for identity in (stamp['adapter'], recipe_identity, binding_identity, helper,
                         engine_before['binary'], engine_before['provenance'],
                         engine_before['fingerprint_helper']):
            bound_files[identity['path']] = identity
        shutil.copyfile(args.pid_helper, output / 'pid-bound-v2.py')
        file_identity(output / 'pid-bound-v2.py', PID_HELPER_SHA256)
        shutil.copyfile(args.recipe, output / 'recipe.json')
        file_identity(output / 'recipe.json', args.recipe_sha256)
        save(output / 'config.json', config)
        shutil.copyfile(recipe['argv'][4], output / 'binding.json')
        shutil.copyfile(args.engine_provenance, output / 'engine.provenance')
        dag = make_dag(args.name, output, stamp['adapter']['path'], str(Path(sys.executable).resolve()))
        save(output / 'dag.json', dag)
        command = engine_command(args.engine, output)
        disk_path = Path(config['paths']['target'])
        before = available_bytes(disk_path)
        disk_device = disk_path.stat().st_dev
        evidence_device = output.stat().st_dev
        stamp.update(recipe=recipe_identity, binding=binding_identity,
                     config=dict(snapshot=file_identity(output / 'config.json'),
                                 original_files=config_origins),
                     sources_before=source_before, engine_before=engine_before,
                     pid_helper_origin=helper, pid_helper_snapshot=file_identity(output / 'pid-bound-v2.py'),
                     input_files=bound_files, cache=dict(cargo_home=config['paths']['cargo_home'],
                                                        tmpdir=str(tmpdir), xdg_cache_home=str(cache),
                                                        cargo_target_dir=config['paths']['target']),
                     command=command, environment_overrides=overrides,
                     dag=file_identity(output / 'dag.json'), disk_guard_path=str(disk_path),
                     disk_guard_device=disk_device, evidence_device=evidence_device,
                     disk_guard_evidence_same_filesystem=disk_device == evidence_device,
                     disk_available_before_bytes=before,
                     limits=dict(cpu_seconds=CPU_SECONDS, wall_seconds=WALL_SECONDS,
                                 outer_seconds=OUTER_SECONDS, cpus=8, pids=512,
                                 memory_bytes=MEMORY_BYTES, swap_bytes=0,
                                 projected_disk_reservation_bytes=DISK_RESERVATION_BYTES,
                                 disk_floor_bytes=DISK_FLOOR_BYTES, console_bytes=CONSOLE_BYTES,
                                 poll_seconds=POLL_SECONDS, term_seconds=TERM_SECONDS))
        save(output / 'launch.json', stamp)
        projected_disk(before)
        stamp['engine_result'] = monitor_engine(command, source, environment, output,
                                                disk_path=disk_path)
        errors.extend(stamp['engine_result']['guard_breaches'])
        for name in ('child.json', 'a6-pids.json'):
            stamp[name] = read_record(output / name)
        child = stamp['child.json']['value']
        pid = stamp['a6-pids.json']['value']
        join_child_receipts(child, pid)
    except BaseException as error:
        errors.append(type(error).__name__ + ': ' + str(error))
    finally:
        # Close every available identity even when a launch, capture or guard failed.
        after = {}
        for role, before in source_before.items():
            try:
                after[role] = source_snapshot(before['path'], before['head'])
                need(after[role] == before, 'source changed during run: ' + role)
            except Exception as error:
                errors.append(type(error).__name__ + ': ' + str(error))
        stamp['sources_after'] = after
        files_after = {}
        for path, before in bound_files.items():
            try:
                files_after[path] = file_identity(path, before['sha256'])
                need(files_after[path] == before, 'input file identity changed: ' + path)
            except Exception as error:
                errors.append(type(error).__name__ + ': ' + str(error))
        stamp['input_files_after'] = files_after
        if engine_before is not None:
            try:
                stamp['engine_after'] = engine_identity(args, Path(args.source), environment)
                need(stamp['engine_after'] == engine_before, 'stock engine identity changed during run')
            except Exception as error:
                errors.append(type(error).__name__ + ': ' + str(error))
        for name in ('child.json', 'a6-pids.json'):
            if name not in stamp and (output / name).is_file():
                try:
                    stamp[name] = read_record(output / name)
                except Exception as error:
                    errors.append(type(error).__name__ + ': ' + str(error))
        stamp['retained_files'] = {}
        for path in sorted(output.rglob('*')):
            if path.is_file():
                try:
                    stamp['retained_files'][str(path.relative_to(output))] = file_identity(path)
                except Exception as error:
                    errors.append('retained file ' + type(error).__name__ + ': ' + str(error))
        engine_status = stamp.get('engine_result', {}).get('native')
        child_status = stamp.get('child.json', {}).get('value', {}).get('native')
        success = (not errors and engine_status is not None
                   and engine_status['returncode'] == 0 and child_status is not None
                   and child_status['returncode'] == 0)
        stamp.update(finished_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                     wall_seconds=time.monotonic() - started, errors=errors,
                     success=success)
        save(output / 'receipt.json', stamp)
    if success:
        return 0
    if engine_status is not None and engine_status['returncode'] > 0:
        return engine_status['returncode']
    invocation = stamp.get('engine_result', {}).get('invocation_failure')
    if invocation is not None:
        return invocation['exit_code']
    return 1


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == '_payload':
        parser = argparse.ArgumentParser(description='Internal PID-helper payload')
        parser.add_argument('--output', type=Path, required=True, help='fresh adapter evidence directory')
        parser.add_argument('--recipe-sha256', required=True, help='digest of its retained recipe.json')
        args = parser.parse_args(argv[1:])
        result = payload(canonical(args.output, directory=True), args.recipe_sha256)
        status = result['native']
        if status is not None and not result['errors']:
            if status['invocation_failure']:
                print('Command returned invocation failure; verify its executable/interpreter'
                      ' and the recorded recipe paths, then prepare a fresh run.', file=sys.stderr)
            return payload_exit(result)
        if result['invocation_failure'] is not None:
            print(result['invocation_failure']['remedy'], file=sys.stderr)
            return payload_exit(result)
        print('Payload failed; inspect child.json, correct the recorded recipe input,'
              ' and prepare a fresh run.', file=sys.stderr)
        return 1
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog='Supply real final source and stock engine identities. This command does not'
               ' build dagrun or grant guest/Ready qualification. Use _payload --help only'
               ' when inspecting the internal DAG command; normal launches use these flags.')
    parser.add_argument('--name', required=True, help='fresh lower-case evidence name (up to 32 characters)')
    parser.add_argument('--recipe', required=True, help='recipe emitted by owned/pipeline.py prepare')
    parser.add_argument('--cpu-seconds', dest='cpu', type=int, default=CPU_SECONDS,
                        help='original product CPU budget; must remain 3600 (default: 3600)')
    parser.add_argument('--wall-seconds', dest='wall', type=int, default=WALL_SECONDS,
                        help='original product wall budget; must remain 3600 (default: 3600)')
    descriptions = {
        'recipe-sha256': 'independently observed SHA256 of the emitted recipe',
        'source': 'canonical final H checkout named by the producer config',
        'source-head': 'independently supplied full final H commit',
        'engine': 'canonical existing stock dagrun binary; never a building launcher',
        'engine-sha256': 'independently supplied engine binary SHA256',
        'engine-provenance': 'existing normal stock .provenance source/binary stamp',
        'engine-provenance-sha256': 'independently observed SHA256 of that normal stamp',
        'pid-helper': 'canonical unchanged existing pid-bound-v2.py origin; bytes are snapshotted',
        'pid-helper-sha256': 'independently supplied unchanged helper digest: ' + PID_HELPER_SHA256,
        'tmpdir': 'canonical existing scratch directory for the original TMPDIR binding',
        'xdg-cache-home': 'canonical existing private XDG cache directory',
        'output-root': 'canonical existing evidence directory; --name must be fresh',
    }
    for name, description in descriptions.items():
        parser.add_argument('--' + name, required=True, help=description)
    args = parser.parse_args(argv)
    try:
        need(re.fullmatch('[a-z0-9][a-z0-9-]{0,31}', args.name),
             'invalid --name; choose 1–32 lower-case letters/digits/hyphens starting with a letter/digit')
        output = canonical(args.output_root, directory=True) / args.name
        output.mkdir()
    except Exception as error:
        print(str(error) + '; choose a fresh --name under an existing canonical --output-root.'
              ' See --help for the supported flags.', file=sys.stderr)
        return 2
    old_signals = {number: signal.signal(number, interrupted)
                   for number in (signal.SIGTERM, signal.SIGINT)}
    try:
        status = execute(args, output)
    finally:
        for number, handler in old_signals.items():
            signal.signal(number, handler)
    receipt = json.loads((output / 'receipt.json').read_text())
    print(json.dumps(dict(success=receipt['success'], receipt=str(output / 'receipt.json'),
                          engine_native=receipt.get('engine_result', {}).get('native'))))
    if status != 0:
        detail = receipt['errors'][0] if receipt['errors'] else 'recorded native command failed'
        print(detail + '; inspect ' + str(output / 'receipt.json') + ' and retained stdout/stderr,'
              ' correct the source/input or invocation, and prepare a fresh run.'
              ' Keep the existing bounds. See --help for the working interface.', file=sys.stderr)
    return status


if __name__ == '__main__':
    raise SystemExit(main())
