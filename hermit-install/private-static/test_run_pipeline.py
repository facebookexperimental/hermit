"""Host controls; no stock dagrun, PID helper or producer is invoked.

The pinned AU pure Python model requires Python 3.10 or newer.
"""

import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import signal
import shlex
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().with_name('run_pipeline.py')
SPEC = importlib.util.spec_from_file_location('private_static_run_pipeline', SCRIPT)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)
PYTHON = str(Path(sys.executable).resolve())
# Arbitrary lines for these controls, deliberately not any host's policy values:
# the adapter must apply whatever the policy supplies.
LINE = 123_456_789_012
STOP = 98_765_432_109
RESERVATION = 25_769_803_776


class AdapterControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='private-static-adapter-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.environment = dict(os.environ, PYTHONDONTWRITEBYTECODE='1')

    def recipe(self, argv, **changes):
        value = dict(argv=argv, cwd=str(self.root),
                     env={'LANG': 'C', 'PRIVATE_STATIC_SET': 'recipe-value'},
                     unset=['PRIVATE_STATIC_REMOVE'], memory_gib=16, reservation_gib=24,
                     scope=RUNNER.SCOPE)
        value.update(changes)
        return value

    def retain_recipe(self, value):
        path = self.root / 'recipe.json'
        path.write_text(json.dumps(value))
        return RUNNER.sha(path)

    def internal(self, value, expected=None, environment=None):
        digest = self.retain_recipe(value)
        result = subprocess.run(
            [PYTHON, '-B', str(SCRIPT), '_payload', '--output', str(self.root),
             '--recipe-sha256', expected or digest],
            env=environment or self.environment, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        return result, json.loads((self.root / 'child.json').read_text())

    def engine(self, program, **options):
        return RUNNER.monitor_engine(
            [PYTHON, '-c', program], self.root, self.environment, self.root,
            free=options.pop('free', lambda _: STOP + 100),
            guarded=options.pop('guarded', [self.root]), stop_line=STOP,
            poll_seconds=0.01, term_seconds=0.05, **options)

    def binding_fixture(self, target=None, runner_digest=None):
        source = self.root / 'source'
        source.mkdir()
        (source / 'target').mkdir()
        (source / 'cache').mkdir()
        adapter = RUNNER.file_identity(SCRIPT)
        config = dict(
            paths=dict(H=str(source), target=str(target or source / 'target'),
                       tool_R=str(source), runtime_R=str(source),
                       cargo_home=str(source / 'cache'), runner=adapter['path']),
            heads=dict(H='a' * 40, runtime_R='b' * 40, tool_R='b' * 40),
            digests=dict(runner=runner_digest or adapter['sha256']))
        config_path = self.root / 'config.json'
        config_path.write_text(json.dumps(config))
        binding = self.root / 'original.binding.json'
        binding.write_text(json.dumps(dict(
            state='PREPARED_NOT_RUN', config=config,
            sources={str(config_path): RUNNER.sha(config_path)})))
        recipe = self.recipe(
            [PYTHON, str(source / 'hermit-install/private-static/owned/pipeline.py'),
             'run', '--binding', str(binding), '--binding-sha256', RUNNER.sha(binding)],
            cwd=str(source))
        return source, adapter, config, recipe

    def test_both_help_forms_on_both_surfaces(self):
        for surface in ([], ['_payload']):
            for flag in ('-h', '--help'):
                with self.subTest(surface=surface, flag=flag):
                    result = subprocess.run(
                        [PYTHON, '-B', str(SCRIPT), *surface, flag], env=self.environment,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(b'--recipe-sha256', result.stdout)
                    if not surface:
                        self.assertIn(b'--name', result.stdout)
                        self.assertIn(b'--engine-provenance', result.stdout)
                        self.assertIn(b'--cpu-seconds', result.stdout)
                        self.assertIn(b'--admission-line-bytes', result.stdout)
                        self.assertIn(b'--running-stop-bytes', result.stdout)
                    else:
                        self.assertIn(b'--output', result.stdout)

    def test_user_inputs_use_flags(self):
        result = subprocess.run(
            [PYTHON, '-B', str(SCRIPT), 'legacy-name', '3600', '3600', 'recipe.json'],
            env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn(b'--name', result.stderr)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_payload_applies_exact_cwd_environment_unset_and_eof(self):
        program = (
            "import json,os,sys; "
            "print(json.dumps([os.getcwd(),os.environ['PRIVATE_STATIC_SET'],"
            "'PRIVATE_STATIC_REMOVE' in os.environ,sys.stdin.read()])); "
            "print('retained-stderr',file=sys.stderr)"
        )
        environment = dict(self.environment, PRIVATE_STATIC_REMOVE='must-disappear',
                           PRIVATE_STATIC_SET='inherited')
        result, child = self.internal(self.recipe([PYTHON, '-c', program]), environment=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((self.root / 'stdout').read_text()),
                         [str(self.root), 'recipe-value', False, ''])
        self.assertEqual((self.root / 'stderr').read_bytes(), b'retained-stderr\n')
        self.assertEqual(child['native']['exit_code'], 0)
        self.assertEqual(child['errors'], [])
        self.assertGreaterEqual(child['wall_seconds'], 0)

    def test_payload_retains_nonzero_exit(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'raise SystemExit(23)']))
        self.assertEqual(result.returncode, 23)
        self.assertEqual(child['native']['returncode'], 23)
        self.assertEqual(child['errors'], [])

    def test_payload_exit126_remains_invocation_failure(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'raise SystemExit(126)']))
        self.assertEqual(result.returncode, 126)
        self.assertTrue(child['native']['invocation_failure'])
        self.assertIn(b'verify its executable/interpreter', result.stderr)

    def test_payload_exit127_remains_invocation_failure(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'raise SystemExit(127)']))
        self.assertEqual(result.returncode, 127)
        self.assertTrue(child['native']['invocation_failure'])
        self.assertIn(b'verify its executable/interpreter', result.stderr)

    def test_payload_retains_real_signal(self):
        result, child = self.internal(self.recipe(
            [PYTHON, '-c', 'import os,signal;os.kill(os.getpid(),signal.SIGTERM)']))
        self.assertEqual(result.returncode, 128 + signal.SIGTERM)
        self.assertEqual(child['native']['returncode'], -signal.SIGTERM)
        self.assertEqual(child['native']['signal'], signal.SIGTERM)
        self.assertIsNone(child['native']['exit_code'])
        # The helper observes the payload exit143, while this retained record
        # observes the native child's actual SIGTERM. This is a receipt join,
        # not evidence that the real PID helper or cgroups ran in this test.
        RUNNER.join_child_receipts(
            child, dict(child_exit=result.returncode, memory_event_assessment={}))

    def test_inconsistent_helper_receipt_cannot_change_native_exit(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'raise SystemExit(23)']))
        self.assertEqual(result.returncode, 23)
        with self.assertRaisesRegex(RuntimeError, 'PID helper did not complete'):
            RUNNER.join_child_receipts(child, dict(child_exit=0, memory_event_assessment={}))

    def test_missing_helper_memory_event_close_cannot_complete(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'pass']))
        self.assertEqual(result.returncode, 0)
        with self.assertRaisesRegex(RuntimeError, 'PID helper did not complete'):
            RUNNER.join_child_receipts(child, dict(child_exit=0))

    def test_missing_command_is127_with_remedy_and_no_invented_native_status(self):
        result, child = self.internal(self.recipe([str(self.root / 'missing-command')]))
        self.assertEqual(result.returncode, 127)
        self.assertIsNone(child['native'])
        self.assertEqual(child['invocation_failure']['exit_code'], 127)
        self.assertIn(b'provide an existing executable', result.stderr)
        self.assertTrue(child['errors'])

    def test_unexecutable_command_is126_with_remedy(self):
        command = self.root / 'non-executable'
        command.write_text('#!/bin/sh\nexit 0\n')
        command.chmod(0o644)
        result, child = self.internal(self.recipe([str(command)]))
        self.assertEqual(result.returncode, 126)
        self.assertIsNone(child['native'])
        self.assertIn(b'supported interpreter', result.stderr)

    def test_changed_recipe_refuses_before_a_child_and_redirects(self):
        result, child = self.internal(self.recipe([PYTHON, '-c', 'print("must-not-run")']),
                                      expected='0' * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(child['native'])
        self.assertFalse((self.root / 'stdout').exists())
        self.assertIn('independently measured digest', child['errors'][0])
        self.assertIn(b'prepare a fresh run', result.stderr)

    def test_recipe_keeps_original_memory_and_disk_reservation(self):
        path = self.root / 'recipe.json'
        for changes in (dict(memory_gib=32), dict(reservation_gib=16),
                        dict(memory_gib=True), dict(reservation_gib=32)):
            with self.subTest(changes=changes):
                path.write_text(json.dumps(self.recipe([PYTHON], **changes)))
                with self.assertRaisesRegex(RuntimeError, 'memory16GiB/disk24GiB'):
                    RUNNER.read_recipe(path, RUNNER.sha(path))

    def test_config_binds_adapter_separately_from_engine(self):
        source, adapter, config, recipe = self.binding_fixture()
        bound, identity, inputs, origins = RUNNER.pipeline_binding(recipe, source, adapter)
        self.assertEqual(bound['config'], config)
        self.assertEqual(identity['sha256'], recipe['argv'][6])
        self.assertEqual(len(origins), 1)
        self.assertEqual(inputs[origins[0]['path']], origins[0])

    def test_config_rejects_an_engine_hash_in_the_adapter_slot(self):
        source, adapter, _, recipe = self.binding_fixture(runner_digest='0' * 64)
        with self.assertRaisesRegex(RuntimeError, 'adapter, separately from dagrun'):
            RUNNER.pipeline_binding(recipe, source, adapter)

    def test_config_cannot_redirect_the_canonical_h_target(self):
        other = self.root / 'other-target'
        other.mkdir()
        source, adapter, _, recipe = self.binding_fixture(target=other)
        with self.assertRaisesRegex(RuntimeError, 'target must remain H/target'):
            RUNNER.pipeline_binding(recipe, source, adapter)

    def test_admission_and_stop_lines_are_the_supplied_policy_values(self):
        self.assertEqual(RUNNER.DISK_RESERVATION_BYTES, RESERVATION)
        for line, stop in ((LINE, STOP), (LINE * 3 + 1, LINE * 2 + 1)):
            with self.subTest(line=line, stop=stop):
                def chain(available):
                    return [dict(path='/fs', mount_point='/fs', source='/dev/fs',
                                 available_bytes=available)]
                RUNNER.projected_disk(chain(line + RESERVATION), line)
                with self.assertRaisesRegex(
                        RuntimeError, 'below the host admission line ' + str(line)
                        + '; recover space on that filesystem'):
                    RUNNER.projected_disk(chain(line + RESERVATION - 1), line)
                self.assertIsNone(RUNNER.guard_breach(stop, 16_777_216, stop))
                self.assertEqual(RUNNER.guard_breach(stop - 1, 0, stop),
                                 'disk available ' + str(stop - 1)
                                 + ' below the host running stop ' + str(stop))

    def test_dag_uses_full_separate_budgets_and_unchanged_pid_arguments(self):
        self.retain_recipe(self.recipe([PYTHON, '-c', 'pass']))
        dag = RUNNER.make_dag('control', self.root, str(SCRIPT), PYTHON)
        self.assertEqual(len(dag['steps']), 1)
        step = dag['steps'][0]
        self.assertEqual((step['cpu_timeout'], step['timeout']), (3600, 3600))
        self.assertEqual(step['hint']['hard_mem_max_bytes'], 17_179_869_184)
        import shlex
        command = shlex.split(step['cmd'])
        for flag, value in (('--limit', '512'), ('--reserved-cpus', '8'),
                            ('--memory-bytes', '17179869184'), ('--quota-cpus', '8')):
            self.assertEqual(command[command.index(flag) + 1], value)
        engine = RUNNER.engine_command('/observed/stock-dagrun', self.root)
        self.assertEqual(engine[1], 'run')
        self.assertNotIn('box', engine)
        self.assertEqual(engine[engine.index('--run-timeout') + 1], '3720')
        self.assertEqual(engine[engine.index('--cpu-timeout-multiplier') + 1], '1')
        self.assertNotIn('--allow-cgroup-failure', engine)
        self.assertNotIn('--unsafe-no-cgroups', engine)

    def test_stock_width_rendering_keeps_payload_argv_and_parser_strict(self):
        self.retain_recipe(self.recipe([PYTHON, '-c', 'print("stock-width-control")']))
        row = RUNNER.make_dag('control', self.root, str(SCRIPT), PYTHON)['steps'][0]
        # Load the pinned stock pure model, without importing its scheduler or
        # running dagrun/PID helpers. Its Rust counterpart uses the same empty
        # jobs_flag/jobs_env contract in model.rs/io.rs.
        model_path = SCRIPT.parents[2] / 'agent-utils/py/dagrun/model.py'
        spec = importlib.util.spec_from_file_location('private_static_stock_model', model_path)
        model = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = model
        self.addCleanup(sys.modules.pop, spec.name)
        spec.loader.exec_module(model)
        step = model.Step(row['group'], row['job'], '', row['cmd'],
                          jobs_flag=row.get('jobs_flag'), jobs_env=row.get('jobs_env'))
        width = row['hint']['preferred_inner_jobs']
        rendered = model.command_with_inner_jobs(step, model.DEFAULT_JOBS_FLAG, width)
        self.assertEqual(rendered, row['cmd'])
        self.assertEqual(model.env_with_inner_jobs(step, 'CARGO_BUILD_JOBS', width), {})
        command = shlex.split(rendered)
        payload = command[command.index('--') + 1:]
        result = subprocess.run(payload, env=self.environment, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'stdout').read_bytes(), b'stock-width-control\n')
        self.assertEqual(json.loads((self.root / 'child.json').read_text())['native']['exit_code'], 0)
        before = (self.root / 'child.json').read_bytes()
        # Omitting the original empty channel really does trigger stock '-j 4';
        # keep argparse's refusal rather than teaching this payload to ignore it.
        step.jobs_flag = None
        inherited = shlex.split(model.command_with_inner_jobs(step, model.DEFAULT_JOBS_FLAG, width))
        self.assertEqual(inherited[-2:], ['-j', '4'])
        refused = subprocess.run(inherited[inherited.index('--') + 1:],
                                 env=self.environment, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(refused.returncode, 2)
        self.assertIn(b'unrecognized arguments: -j 4', refused.stderr)
        self.assertEqual((self.root / 'child.json').read_bytes(), before)

    def test_disk_poll_measures_every_guarded_filesystem_and_the_fullest_decides(self):
        target = self.root / 'producer-target'
        image = self.root / 'backing.img'
        observed = []
        values = {target: STOP + RESERVATION, image: STOP - 1, self.root: STOP + LINE}
        def available(path):
            observed.append(path)
            return values[path]
        result = self.engine('pass', guarded=[target, image],
                             free=lambda paths: RUNNER.minimum_available(paths, available))
        self.assertTrue(observed)
        self.assertEqual(set(observed), {target, image})
        self.assertEqual(result['disk_guard_paths'], [str(target), str(image)])
        self.assertEqual(result['disk_available_after_bytes'], STOP - 1)
        self.assertIn('disk available ' + str(STOP - 1) + ' below the host running stop '
                      + str(STOP), result['guard_breaches'])

    def test_environment_keeps_canonical_cache_and_target_without_timeout_overrides(self):
        inherited = dict(CARGO_TARGET_DIR='/wrong', RUSTFLAGS='wrong',
                         HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER='9',
                         DAGRUN_CPU_TIMEOUT_MULTIPLIER='9', LD_PRELOAD='/wrong')
        config = dict(paths=dict(cargo_home='/recorded/cache', target='/recorded/H/target'))
        with patch.dict(os.environ, inherited, clear=True):
            environment, overrides = RUNNER.outer_environment(config, self.root, self.root)
        self.assertEqual(environment['CARGO_HOME'], '/recorded/cache')
        self.assertEqual(environment['CARGO_TARGET_DIR'], '/recorded/H/target')
        self.assertEqual(environment['CARGO_NET_OFFLINE'], 'true')
        for name in ('RUSTFLAGS', 'HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER',
                     'DAGRUN_CPU_TIMEOUT_MULTIPLIER', 'LD_PRELOAD'):
            self.assertNotIn(name, environment)
        self.assertEqual(overrides['TMPDIR'], str(self.root))

    def test_engine_retains_actual_exit_and_console(self):
        result = self.engine('print("engine-output");raise SystemExit(23)')
        self.assertEqual(result['native']['returncode'], 23)
        self.assertEqual(result['guard_breaches'], [])
        self.assertEqual((self.root / 'console.log').read_bytes(), b'engine-output\n')

    def test_engine_retains_actual_signal(self):
        result = self.engine('import os,signal;os.kill(os.getpid(),signal.SIGTERM)')
        self.assertEqual(result['native']['signal'], signal.SIGTERM)
        self.assertEqual(result['guard_breaches'], [])

    def test_fast_console_overflow_cannot_be_a_zero_exit_success(self):
        ready = self.root / 'completed-output'
        program = ('import os;from pathlib import Path;os.write(1,b"x"*16777217);'
                   f'Path({str(ready)!r}).write_text("done");os._exit(0)')
        def free(_):
            deadline = time.monotonic() + 2
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.005)
            self.assertTrue(ready.exists())
            time.sleep(0.05)
            return STOP
        result = self.engine(program, free=free)
        self.assertEqual(result['native']['returncode'], 0)
        self.assertEqual(result['console_bytes'], 16_777_217)
        self.assertIn('console exceeds 16 MiB', result['guard_breaches'])

    def test_floor_breach_terminates_and_retains_native_status_and_time(self):
        result = self.engine('import time;time.sleep(60)', free=lambda _: STOP - 1)
        self.assertEqual(result['native']['signal'], signal.SIGTERM)
        self.assertTrue(result['guard_breaches'])
        self.assertGreaterEqual(result['wall_seconds'], 0)
        self.assertLess(result['wall_seconds'], 5)

    def test_guard_escalates_to_kill_when_term_is_ignored(self):
        ready = self.root / 'ready'
        # The guard may fire as soon as ready exists, so publish it complete.
        program = ("import os,signal,time;from pathlib import Path;"
                   "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                   f"ready=Path({str(ready)!r});pending=ready.with_suffix('.tmp');"
                   "pending.write_text('ready');os.replace(pending,ready);time.sleep(60)")
        def free(_):
            return STOP - 1 if ready.exists() else STOP
        result = self.engine(program, free=free)
        self.assertEqual(result['native']['signal'], signal.SIGKILL)
        self.assertEqual(ready.read_text(), 'ready')
        self.assertTrue(result['guard_breaches'])
        self.assertLess(result['wall_seconds'], 5)

    def test_guard_stops_the_owned_child_process_group(self):
        pidfile = self.root / 'descendant'
        # A final pathname must not become visible before the PID is written.
        program = ("import os,subprocess,time;from pathlib import Path;"
                   f"p=subprocess.Popen([{PYTHON!r},'-c','import time;time.sleep(60)']);"
                   f"pidfile=Path({str(pidfile)!r});pending=pidfile.with_suffix('.tmp');"
                   "pending.write_text(str(p.pid));os.replace(pending,pidfile);time.sleep(60)")
        def free(_):
            return STOP - 1 if pidfile.exists() else STOP
        result = self.engine(program, free=free)
        self.assertEqual(result['native']['signal'], signal.SIGTERM)
        pid = int(pidfile.read_text())
        deadline = time.monotonic() + 2
        while Path(f'/proc/{pid}/stat').exists() and time.monotonic() < deadline:
            try:
                state = Path(f'/proc/{pid}/stat').read_text().split(') ', 1)[1].split()[0]
            except FileNotFoundError:
                state = None
                break
            if state == 'Z':
                break
            time.sleep(0.01)
        else:
            state = None if not Path(f'/proc/{pid}/stat').exists() else 'still-running'
        self.assertIn(state, (None, 'Z'), 'owned descendant must have exited, even before init reaps it')

    def test_poll_read_failure_still_records_real_status(self):
        def failed(_):
            raise OSError('injected statvfs failure')
        result = self.engine('import time;time.sleep(60)', free=failed)
        self.assertEqual(result['native']['signal'], signal.SIGTERM)
        self.assertTrue(any('injected statvfs failure' in v for v in result['guard_breaches']))
        self.assertGreaterEqual(result['wall_seconds'], 0)

    def test_cancellation_still_reaps_and_records_the_actual_engine(self):
        def cancelled(_):
            raise InterruptedError('operator cancellation')
        result = self.engine('import time;time.sleep(60)', sleep=cancelled)
        self.assertEqual(result['native']['signal'], signal.SIGTERM)
        self.assertTrue(any('operator cancellation' in v for v in result['guard_breaches']))
        self.assertGreaterEqual(result['wall_seconds'], 0)

    def test_engine_invocation_failure_is_not_a_native_success(self):
        result = RUNNER.monitor_engine(
            [str(self.root / 'missing-engine')], self.root, self.environment, self.root,
            guarded=[self.root], stop_line=STOP, free=lambda _: STOP)
        self.assertIsNone(result['native'])
        self.assertEqual(result['invocation_failure']['exit_code'], 127)
        self.assertIn('existing executable', result['invocation_failure']['remedy'])
        self.assertTrue(result['guard_breaches'])

    def test_bound_file_changed_is_not_relabelled_as_current(self):
        path = self.root / 'bound'
        path.write_bytes(b'initial')
        identity = RUNNER.file_identity(path)
        path.write_bytes(b'changed')
        with self.assertRaisesRegex(RuntimeError, 'independently measured digest'):
            RUNNER.file_identity(path, identity['sha256'])

    def test_duplicate_stock_stamp_is_not_an_engine_build_receipt(self):
        engine = self.root / 'engine'
        engine.write_text('not executed')
        stamp = self.root / 'engine.provenance'
        stamp.write_text('source_fp=' + 'a' * 64 + '\nsource_fp=' + 'b' * 64
                         + '\nbinary_sha256=' + RUNNER.sha(engine) + '\n')
        args = argparse.Namespace(engine=str(engine), engine_sha256=RUNNER.sha(engine),
                                  engine_provenance=str(stamp),
                                  engine_provenance_sha256=RUNNER.sha(stamp))
        with self.assertRaisesRegex(RuntimeError, 'duplicate engine provenance'):
            RUNNER.engine_identity(args, self.root, self.environment)

    def test_stock_stamp_cannot_bind_another_binary(self):
        engine = self.root / 'engine'
        engine.write_text('not executed')
        stamp = self.root / 'engine.provenance'
        stamp.write_text('source_fp=' + 'a' * 64 + '\nbinary_sha256=' + '0' * 64 + '\n')
        args = argparse.Namespace(engine=str(engine), engine_sha256=RUNNER.sha(engine),
                                  engine_provenance=str(stamp),
                                  engine_provenance_sha256=RUNNER.sha(stamp))
        with self.assertRaisesRegex(RuntimeError, 'match its stock provenance'):
            RUNNER.engine_identity(args, self.root, self.environment)

    def test_preflight_refusal_retains_a_failed_receipt_without_launch(self):
        self.retain_recipe(self.recipe([PYTHON, '-c', 'print("must-not-run")']))
        output = self.root / 'evidence'
        output.mkdir()
        args = argparse.Namespace(cpu=3600, wall=3600, source=str(self.root),
                                  tmpdir=str(self.root), xdg_cache_home=str(self.root),
                                  recipe=str(self.root / 'recipe.json'), recipe_sha256='0' * 64)
        with patch.object(RUNNER, 'monitor_engine', side_effect=AssertionError('must not launch')):
            status = RUNNER.execute(args, output)
        receipt = json.loads((output / 'receipt.json').read_text())
        self.assertNotEqual(status, 0)
        self.assertFalse(receipt['success'])
        self.assertNotIn('engine_result', receipt)
        self.assertIn('independently measured digest', receipt['errors'][0])
        self.assertGreaterEqual(receipt['wall_seconds'], 0)
        self.assertEqual(receipt['product_tests_claimed'], 0)

    def cli_flags(self, name, **policy):
        flags = ['--name', name, '--recipe', str(self.root / 'recipe.json'),
                 '--recipe-sha256', '0' * 64, '--source', str(self.root),
                 '--source-head', 'a' * 40, '--engine', PYTHON, '--engine-sha256', '0' * 64,
                 '--engine-provenance', str(self.root / 'unused.provenance'),
                 '--engine-provenance-sha256', '0' * 64, '--pid-helper', str(self.root / 'unused.py'),
                 '--pid-helper-sha256', RUNNER.PID_HELPER_SHA256, '--tmpdir', str(self.root),
                 '--xdg-cache-home', str(self.root), '--output-root', str(self.root)]
        values = dict(admission_line_bytes=str(LINE), running_stop_bytes=str(STOP))
        values.update(policy)
        for key, value in values.items():
            if value is not None:
                flags.append('--' + key.replace('_', '-') + '=' + value)
        return flags

    def test_missing_or_broken_policy_lines_refuse_by_flag_before_any_evidence(self):
        before = sorted(self.root.iterdir())
        cases = (
            (dict(admission_line_bytes=None), '--admission-line-bytes'),
            (dict(running_stop_bytes=None), '--running-stop-bytes'),
            # What a failed `host-policy get` substitutes into the documented launch.
            (dict(admission_line_bytes=''), '--admission-line-bytes'),
            (dict(running_stop_bytes=''), '--running-stop-bytes'),
            (dict(admission_line_bytes='330e9'), '--admission-line-bytes'),
            (dict(admission_line_bytes='330GB'), '--admission-line-bytes'),
            (dict(running_stop_bytes=' 310000000000'), '--running-stop-bytes'),
            (dict(running_stop_bytes='-1'), '--running-stop-bytes'),
            (dict(running_stop_bytes='0'), '--running-stop-bytes'),
            # Swapped or equal lines would admit below the stop.
            (dict(admission_line_bytes=str(STOP), running_stop_bytes=str(LINE)),
             '--running-stop-bytes ' + str(LINE) + ' must be below --admission-line-bytes'),
            (dict(running_stop_bytes=str(LINE)), 'must be below --admission-line-bytes'),
        )
        for policy, named in cases:
            with self.subTest(policy=policy):
                stderr = io.StringIO()
                with self.assertRaises(SystemExit) as stopped, contextlib.redirect_stderr(stderr):
                    RUNNER.main(self.cli_flags('policy-control', **policy))
                self.assertEqual(stopped.exception.code, 2)
                self.assertIn(named, stderr.getvalue())
                self.assertEqual(sorted(self.root.iterdir()), before)

    def test_execute_admits_and_stops_on_the_supplied_policy_lines(self):
        source = self.root / 'source'
        (source / 'target').mkdir(parents=True)
        (source / 'cache').mkdir()
        files = {}
        for name in ('recipe.json', 'binding.json', 'helper.py', 'engine.provenance'):
            files[name] = self.root / name
            files[name].write_text('{}')
        config = dict(paths=dict(H=str(source), runtime_R=str(source), tool_R=str(source),
                                 target=str(source / 'target'), cargo_home=str(source / 'cache')),
                      heads=dict(H='a' * 40, runtime_R='b' * 40, tool_R='b' * 40))
        recipe = dict(argv=[PYTHON, 'pipeline.py', 'run', '--binding', str(files['binding.json']),
                            '--binding-sha256', '0' * 64])
        def identity(path, expected=None):
            return dict(path=str(path), bytes=0, sha256='0' * 64)
        def launch(name, chain, engine):
            args = argparse.Namespace(
                cpu=3600, wall=3600, source=str(source), source_head='a' * 40,
                tmpdir=str(self.root), xdg_cache_home=str(self.root), name=name,
                recipe=str(files['recipe.json']), recipe_sha256='0' * 64, engine=PYTHON,
                engine_provenance=str(files['engine.provenance']), pid_helper=str(files['helper.py']),
                pid_helper_sha256=RUNNER.PID_HELPER_SHA256,
                admission_line_bytes=LINE, running_stop_bytes=STOP)
            output = self.root / name
            output.mkdir()
            with patch.object(RUNNER, 'read_recipe', return_value=(recipe, identity('recipe'))), \
                    patch.object(RUNNER, 'pipeline_binding',
                                 return_value=(dict(config=config), identity('binding'), {}, [])), \
                    patch.object(RUNNER, 'source_snapshot',
                                 side_effect=lambda path, head: dict(path=str(path), head=head)), \
                    patch.object(RUNNER, 'engine_identity', return_value=dict(
                        binary=identity('engine'), provenance=identity('stamp'),
                        fingerprint_helper=identity('fingerprint'))), \
                    patch.object(RUNNER, 'file_identity', side_effect=identity), \
                    patch.object(RUNNER, 'storage_chain', return_value=chain) as resolved, \
                    patch.object(RUNNER, 'monitor_engine', side_effect=engine):
                RUNNER.execute(args, output)
            resolved.assert_called_once_with(source / 'target')
            return json.loads((output / 'receipt.json').read_text())
        def chain(loop_free, host_free):
            return [dict(path=str(source / 'target'), mount_point='/slots', source='/dev/loop0',
                         available_bytes=loop_free),
                    dict(path=str(files['binding.json']), mount_point='/data', source='/dev/md0',
                         available_bytes=host_free)]
        captured = {}
        def engine(*_, **options):
            captured.update(options)
            raise RuntimeError('engine control stops here')
        receipt = launch('admitted', chain(LINE * 2, LINE + RESERVATION), engine)
        self.assertEqual(captured['stop_line'], STOP)
        self.assertEqual(captured['guarded'], [source / 'target', files['binding.json']])
        self.assertEqual(receipt['limits']['disk_admission_line_bytes'], LINE)
        self.assertEqual(receipt['limits']['disk_running_stop_bytes'], STOP)
        self.assertEqual(receipt['disk_available_before_bytes'], LINE + RESERVATION)
        self.assertEqual(receipt['disk_storage_chain'], chain(LINE * 2, LINE + RESERVATION))
        # st_dev differs per btrfs subvolume, so st_dev comparisons are not recorded.
        for key in ('disk_guard_device', 'evidence_device', 'disk_guard_evidence_same_filesystem'):
            self.assertNotIn(key, receipt)
        self.assertIn('engine control stops here', receipt['errors'][0])
        # One byte less on the backing filesystem refuses before any engine starts,
        # although the loop filesystem alone has room.
        receipt = launch('refused', chain(LINE * 2, LINE + RESERVATION - 1),
                         AssertionError('must not launch'))
        self.assertNotIn('engine_result', receipt)
        self.assertIn('/data (/dev/md0) has ' + str(LINE + RESERVATION - 1), receipt['errors'][0])
        self.assertIn('below the host admission line ' + str(LINE), receipt['errors'][0])

    def test_cli_failed_preflight_names_receipt_and_working_remedy(self):
        self.retain_recipe(self.recipe([PYTHON, '-c', 'print("must-not-run")']))
        args = self.cli_flags('preflight-control')
        result = subprocess.run([PYTHON, '-B', str(SCRIPT), *args], env=self.environment,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 1)
        report = json.loads(result.stdout)
        self.assertFalse(report['success'])
        self.assertIsNone(report['engine_native'])
        receipt = json.loads(Path(report['receipt']).read_text())
        self.assertIn('independently measured digest', receipt['errors'][0])
        self.assertIn(b'prepare a fresh run', result.stderr)
        self.assertIn(b'Keep the existing bounds', result.stderr)
        self.assertFalse((self.root / 'preflight-control' / 'stdout').exists())


class FakeHost:
    """A fake sysfs tree with an injected mount table, device nodes and free space.

    Nothing here reads the real /sys, /dev or mount table.
    """

    PCI = 'pci0000:00/0000:00:01.0/nvme/nvme0'

    def __init__(self, root):
        self.sys = root / 'sys'
        self.mounts = {}
        self.nodes = {}
        self.free = {}
        self.directories = {}

    def block(self, name, number, *, parent=None, backing=None, slaves=(), under='virtual/block',
              md=False, dm=False):
        """A sysfs block device. UNDER is its place below sys/devices; a partition sits
        inside its disk; MD and DM add the md/ or dm/ directory those drivers create."""
        base = self.directories[parent] if parent else self.sys / 'devices' / under
        directory = base / name
        directory.mkdir(parents=True)
        self.directories[name] = directory
        if parent:
            (directory / 'partition').write_text('1\n')
        if backing is not None:
            (directory / 'loop').mkdir()
            if backing is not False:
                (directory / 'loop' / 'backing_file').write_text(str(backing) + '\n')
        for flag, marker in ((md, 'md'), (dm, 'dm')):
            if flag:
                (directory / marker).mkdir()
        (directory / 'slaves').mkdir()
        for lower in slaves:
            (directory / 'slaves' / lower.name).symlink_to(lower)
        link = self.sys / 'dev' / 'block' / number
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(directory)
        return directory

    def disk(self, name, number):
        """A disk on the local PCI bus."""
        return self.block(name, number, under=self.PCI)

    def btrfs(self, uuid, *members):
        devices = self.sys / 'fs' / 'btrfs' / uuid / 'devices'
        devices.mkdir(parents=True)
        for member in members:
            (devices / member.name).symlink_to(member)

    def listed(self, device):
        return any(member.resolve() == device
                   for member in (self.sys / 'fs' / 'btrfs').glob('*/devices/*'))

    def mount(self, path, device, source, mount_point, free, fstype='btrfs', options='rw'):
        self.mounts[Path(path)] = dict(device=device, source=source, mount_point=mount_point,
                                       fstype=fstype, options=options)
        self.free[Path(path)] = free
        # The kernel lists every btrfs member; a single-device btrfs lists its one device.
        if fstype == 'btrfs' and source in self.nodes:
            member = (self.sys / 'dev' / 'block' / self.nodes[source]).resolve()
            if member.exists() and not self.listed(member):
                self.btrfs('single-' + member.name, member)

    def device_number(self, source):
        if source not in self.nodes:
            raise RuntimeError('mount source ' + repr(source) + ' is not a block device')
        return self.nodes[source]

    def chain(self, path):
        return RUNNER.storage_chain(
            path, sys_root=self.sys, mount=lambda here: self.mounts[Path(here)],
            device_number=self.device_number, available=lambda here: self.free[Path(here)])


class StorageChainControls(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='private-static-storage-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.host = FakeHost(self.root)
        self.target = self.root / 'slot' / 'target'
        self.target.mkdir(parents=True)
        self.image = self.root / 'data' / 'slots.img'
        self.image.parent.mkdir()
        self.image.write_bytes(b'')

    def md_array(self, host=None):
        host = host or self.host
        host.disk('nvme0n1', '259:0')
        partition = host.block('nvme0n1p1', '259:1', parent='nvme0n1')
        host.nodes['/dev/md0'] = '9:127'
        return host.block('md0', '9:127', slaves=[partition], md=True)

    def slot_on_loop(self, loop_free, data_free, host=None):
        """H/target on a btrfs loop device whose image file lives on an md array."""
        host = host or self.host
        self.md_array(host)
        host.block('loop0', '7:0', backing=self.image)
        host.nodes['/dev/loop0'] = '7:0'
        host.mount(self.target, '0:90', '/dev/loop0', '/slots', loop_free)
        host.mount(self.image, '0:42', '/dev/md0', '/data', data_free)
        return host

    @staticmethod
    def summary(chain):
        return [(filesystem['mount_point'], filesystem['source'], filesystem['available_bytes'])
                for filesystem in chain]

    def test_loop_chain_reaches_the_filesystem_holding_the_backing_file(self):
        self.slot_on_loop(570_400_000_000, 451_100_000_000)
        chain = self.host.chain(self.target)
        self.assertEqual(self.summary(chain), [('/slots', '/dev/loop0', 570_400_000_000),
                                               ('/data', '/dev/md0', 451_100_000_000)])
        self.assertEqual([filesystem['path'] for filesystem in chain],
                         [str(self.target), str(self.image)])
        # Each entry records the mountinfo device, the key a filesystem is measured once by.
        self.assertEqual([filesystem['device'] for filesystem in chain], ['0:90', '0:42'])
        self.assertEqual(chain[0]['backing_files'], [str(self.image)])
        self.assertEqual(chain[1]['backing_files'], [])

    def test_the_fullest_filesystem_in_the_chain_decides(self):
        for case, loop_free, data_free, decisive in (
                ('host-fuller', LINE * 2, LINE + RESERVATION - 1, '/data (/dev/md0)'),
                ('loop-fuller', LINE + RESERVATION - 1, LINE * 2, '/slots (/dev/loop0)')):
            with self.subTest(decisive=decisive):
                host = self.slot_on_loop(loop_free, data_free, FakeHost(self.root / case))
                chain = host.chain(self.target)
                with self.assertRaisesRegex(RuntimeError, re.escape(
                        decisive + ' has ' + str(LINE + RESERVATION - 1) + ' bytes')):
                    RUNNER.projected_disk(chain, LINE)
                self.assertEqual(RUNNER.minimum_available(
                    [Path(filesystem['path']) for filesystem in chain],
                    lambda here: host.free[Path(here)]), LINE + RESERVATION - 1)
        self.slot_on_loop(LINE + RESERVATION, LINE + RESERVATION + 1)
        RUNNER.projected_disk(self.host.chain(self.target), LINE)

    def test_an_unresolvable_backing_file_refuses_by_name(self):
        cases = dict(missing=self.root / 'data' / 'gone.img',
                     deleted=str(self.image) + ' (deleted)',
                     unreachable='(unreachable)' + str(self.image),
                     directory=self.image.parent, unreadable=False)
        for case, backing in cases.items():
            with self.subTest(case=case):
                host = FakeHost(self.root / case)
                self.md_array(host)
                host.block('loop0', '7:0', backing=backing)
                host.nodes['/dev/loop0'] = '7:0'
                host.mount(self.target, '0:90', '/dev/loop0', '/slots', LINE * 9)
                # Every reachable filesystem is mounted, so only the refusal can stop the walk.
                host.mount(self.image, '0:42', '/dev/md0', '/data', LINE * 9)
                with self.assertRaisesRegex(RuntimeError, 'backing file of loop device loop0'):
                    host.chain(self.target)

    def test_a_filesystem_whose_devices_cannot_be_read_refuses_by_name(self):
        not_readable = 'is not btrfs or a single-device type'
        for name, source, device, fstype, named in (
                ('tmpfs', 'tmpfs', '0:55', 'tmpfs', "(type tmpfs, source 'tmpfs') " + not_readable),
                # A free-text source naming a real btrfs member is still not believed.
                ('overlay', '/dev/md0', '0:56', 'overlay',
                 "(type overlay, source '/dev/md0') " + not_readable),
                ('nfs', 'server:/export', '0:57', 'nfs4',
                 "(type nfs4, source 'server:/export') " + not_readable),
                ('missing-node', '/dev/sdz', '8:240', 'ext4',
                 'block device 8:240 of the filesystem at /m (type ext4')):
            with self.subTest(name=name):
                host = FakeHost(self.root / name)
                host.btrfs('md-member', self.md_array(host))
                host.mount(self.target, device, source, '/m', LINE * 9, fstype=fstype)
                with self.assertRaisesRegex(RuntimeError, re.escape(named)):
                    host.chain(self.target)

    def test_a_btrfs_source_is_believed_only_as_a_member_btrfs_lists(self):
        host = FakeHost(self.root / 'unlisted')
        self.md_array(host)
        listed = host.disk('nvme1n1', '259:9')
        host.btrfs('other', listed)
        host.block('nvme1n1p2', '259:10', parent='nvme1n1')
        # The source is a block device, but no btrfs filesystem lists it as a member.
        host.mounts[self.target] = dict(device='0:90', source='/dev/nvme1n1p2', mount_point='/m',
                                        fstype='btrfs', options='rw')
        host.free[self.target] = LINE * 9
        host.nodes['/dev/nvme1n1p2'] = '259:10'
        with self.assertRaisesRegex(RuntimeError, re.escape(
                "(type btrfs, source '/dev/nvme1n1p2'): no btrfs filesystem lists its source")):
            host.chain(self.target)
        # A source that cannot be stat'ed names the error, not "not a block device".
        missing = str(self.root / 'no-such-device')
        with self.assertRaisesRegex(RuntimeError, re.escape(
                'cannot stat mount source ' + repr(missing) + ': [Errno 2]')):
            RUNNER.storage_chain(
                self.target, sys_root=host.sys, device_number=RUNNER.block_node_number,
                mount=lambda _: dict(device='0:91', source=missing, mount_point='/m',
                                     fstype='btrfs', options='rw'),
                available=lambda _: LINE * 9)

    def test_multi_device_filesystems_other_than_btrfs_refuse_by_name(self):
        for name, source, fstype, options, named in (
                ('bcachefs', '/dev/nvme0n1:/dev/nvme1n1', 'bcachefs', 'rw',
                 'is not btrfs or a single-device type'),
                ('f2fs', '/dev/nvme0n1', 'f2fs', 'rw', 'is not btrfs or a single-device type'),
                ('xfs-realtime', '/dev/nvme0n1', 'xfs', 'rw,attr2,rtdev=/dev/loop5,noquota',
                 'also uses rtdev=/dev/loop5; multi-device filesystems other than btrfs')):
            with self.subTest(name=name):
                host = FakeHost(self.root / name)
                host.disk('nvme0n1', '259:0')
                host.mount(self.target, '259:0', source, '/m', LINE * 9, fstype=fstype,
                           options=options)
                with self.assertRaisesRegex(RuntimeError, re.escape(named)):
                    host.chain(self.target)
        host = FakeHost(self.root / 'xfs')
        host.disk('nvme0n1', '259:0')
        host.mount(self.target, '259:0', '/dev/nvme0n1', '/m', LINE * 9, fstype='xfs',
                   options='rw,attr2,inode64,noquota')
        self.assertEqual(self.summary(host.chain(self.target)), [('/m', '/dev/nvme0n1', LINE * 9)])

    def test_devices_that_hide_their_backing_store_refuse_by_name(self):
        for name, layout in (('nbd0', dict(number='43:0')), ('zram0', dict(number='252:0')),
                             ('ublkb0', dict(number='259:20')), ('ram0', dict(number='1:0')),
                             ('nvme3n1', dict(number='259:21',
                                              under='virtual/nvme-fabrics/ctl/nvme3')),
                             ('sdb', dict(number='8:16', under='platform/host2/session1/'
                                          'target2:0:0/2:0:0:0/block')),
                             ('dm-0', dict(number='253:0', dm=True, thin_over_disk=True)),
                             ('md1', dict(number='9:1', md=True, over_nbd=True))):
            with self.subTest(name=name):
                host = FakeHost(self.root / name)
                lower = host.disk('nvme0n1', '259:0')
                options = dict(layout)
                number = options.pop('number')
                if options.pop('thin_over_disk', False):
                    options['slaves'] = [lower]
                if options.pop('over_nbd', False):
                    options['slaves'] = [host.block('nbd5', '43:5')]
                host.block(name, number, **options)
                host.mount(self.target, number, '/dev/' + name, '/m', LINE * 9, fstype='ext4')
                refused = 'nbd5' if name == 'md1' else name
                with self.assertRaisesRegex(RuntimeError, re.escape(
                        'block device ' + refused + ' (')
                        + '.*' + re.escape(') is not a disk on the local PCI bus')):
                    host.chain(self.target)
        host = FakeHost(self.root / 'local-disk')
        host.disk('nvme0n1', '259:0')
        host.mount(self.target, '259:0', '/dev/nvme0n1', '/m', LINE * 9, fstype='ext4')
        self.assertEqual(self.summary(host.chain(self.target)), [('/m', '/dev/nvme0n1', LINE * 9)])

    def test_partitioned_loop_devices_are_followed(self):
        host = FakeHost(self.root / 'partition')
        self.md_array(host)
        host.block('loop0', '7:0', backing=self.image)
        host.block('loop0p1', '259:7', parent='loop0')
        host.mount(self.target, '259:7', '/dev/loop0p1', '/upper', LINE * 3, fstype='ext4')
        host.mount(self.image, '0:42', '/dev/md0', '/data', LINE * 2)
        self.assertEqual(self.summary(host.chain(self.target)),
                         [('/upper', '/dev/loop0p1', LINE * 3), ('/data', '/dev/md0', LINE * 2)])

    def test_every_member_of_a_multi_device_btrfs_is_followed(self):
        md0 = self.md_array()
        member = self.host.block('loop1', '7:1', backing=self.image)
        self.host.btrfs('5ee0b1a9', md0, member)
        self.host.disk('nvme1n1', '259:9')
        self.host.nodes['/dev/nvme1n1'] = '259:9'
        # The mount source names only md0; the loop member is found through btrfs.
        self.host.mount(self.target, '0:42', '/dev/md0', '/data', LINE * 3)
        self.host.mount(self.image, '0:77', '/dev/nvme1n1', '/other', LINE * 2)
        chain = self.host.chain(self.target)
        self.assertEqual(self.summary(chain), [('/data', '/dev/md0', LINE * 3),
                                               ('/other', '/dev/nvme1n1', LINE * 2)])
        self.assertEqual(chain[0]['backing_files'], [str(self.image)])

    def test_nested_loops_are_followed_and_each_filesystem_is_measured_once(self):
        self.md_array()
        outer = self.root / 'data' / 'outer.img'
        outer.write_bytes(b'')
        inner = self.root / 'slot' / 'inner.img'
        inner.write_bytes(b'')
        self.host.block('loop0', '7:0', backing=outer)
        self.host.block('loop1', '7:1', backing=inner)
        second = self.host.block('loop2', '7:2', backing=self.image)
        third = self.host.block('loop3', '7:3', backing=outer)
        self.host.btrfs('0dd5', second, third)
        self.host.nodes.update({'/dev/loop0': '7:0', '/dev/loop1': '7:1', '/dev/loop2': '7:2'})
        # target -> loop1 (image inner.img) -> loop0 (image outer.img) -> md0
        nested = self.root / 'nested'
        self.host.mount(nested, '0:91', '/dev/loop1', '/nested', LINE * 4)
        self.host.mount(inner, '0:90', '/dev/loop0', '/slots', LINE * 3)
        self.host.mount(outer, '0:42', '/dev/md0', '/data', LINE * 2)
        self.assertEqual(self.summary(self.host.chain(nested)),
                         [('/nested', '/dev/loop1', LINE * 4), ('/slots', '/dev/loop0', LINE * 3),
                          ('/data', '/dev/md0', LINE * 2)])
        # Two loop members whose images share one filesystem: measured once.
        shared = self.root / 'shared'
        self.host.mount(shared, '0:92', '/dev/loop2', '/shared', LINE * 5)
        self.host.mount(self.image, '0:42', '/dev/md0', '/data', LINE * 2)
        chain = self.host.chain(shared)
        self.assertEqual(self.summary(chain), [('/shared', '/dev/loop2', LINE * 5),
                                               ('/data', '/dev/md0', LINE * 2)])
        self.assertEqual(chain[0]['backing_files'], [str(self.image), str(outer)])

    def test_mountinfo_rows_are_parsed_by_mount_id(self):
        table = '\n'.join((
            '36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue',
            '40 36 0:90 /slot /slot\\040dir\\134x rw shared:7 master:3 - btrfs /dev/loop0 rw',
            '41 40 0:42 / /data rw - btrfs /dev/md\\0400 rw'))
        self.assertEqual(RUNNER.mountinfo_row(table, '40', 'p'), dict(
            device='0:90', mount_point='/slot dir\\x', fstype='btrfs', source='/dev/loop0',
            options='rw'))
        self.assertEqual(RUNNER.mountinfo_row(table, '41', 'p'), dict(
            device='0:42', mount_point='/data', fstype='btrfs', source='/dev/md 0', options='rw'))
        self.assertEqual(RUNNER.mountinfo_row(table, '36', 'p')['options'], 'rw,errors=continue')
        with self.assertRaisesRegex(RuntimeError, 'mount 42 of p is missing'):
            RUNNER.mountinfo_row(table, '42', 'p')

    def test_the_live_mount_lookup_names_the_mount_holding_the_path(self):
        # Reads this process's own /proc entries only; no sysfs or loop device.
        row = RUNNER.mount_of(self.target)
        self.assertRegex(row['device'], '^[0-9]+:[0-9]+$')
        self.assertTrue(self.target == Path(row['mount_point'])
                        or Path(row['mount_point']) in self.target.parents, row)


# From the seat's review supplement: the two entry points that read the live host,
# and a walk whose members' backing files sit on different filesystems. Hermetic;
# they read no loop device. The stacked case uses md, since device-mapper refuses.
class LiveEntryPointsAndFanOut(unittest.TestCase):
    def test_mount_of_selects_the_mount_by_id_not_the_first_row(self):
        # /proc is procfs mounted at /proc on every Linux host; '/' is the first row.
        row = RUNNER.mount_of('/proc/self')
        self.assertEqual((row['mount_point'], row['fstype']), ('/proc', 'proc'))
        # The open file's mount decides, not the path text: a link under a temporary
        # directory that points into /proc is on procfs.
        with tempfile.TemporaryDirectory(prefix='private-static-mount-') as directory:
            link = Path(directory) / 'to-proc'
            link.symlink_to('/proc/self')
            self.assertEqual(RUNNER.mount_of(link)['fstype'], 'proc')

    def test_block_node_number_reads_the_named_device_of_a_block_node_only(self):
        fake = os.stat_result((stat.S_IFBLK | 0o660, 1, os.makedev(0, 5), 1, 0, 0, 0, 0, 0, 0),
                              {'st_rdev': os.makedev(259, 1)})
        with patch.object(RUNNER.os, 'stat', return_value=fake):
            self.assertEqual(RUNNER.block_node_number('/dev/fake-block'), '259:1')
        with self.assertRaisesRegex(RuntimeError, "'/dev/null' is not a block device"):
            RUNNER.block_node_number('/dev/null')  # a character device
        with tempfile.NamedTemporaryFile() as regular:
            with self.assertRaisesRegex(RuntimeError, 'is not a block device'):
                RUNNER.block_node_number(regular.name)
        with self.assertRaisesRegex(RuntimeError, "'tmpfs' is not a device path"):
            RUNNER.block_node_number('tmpfs')
        with self.assertRaisesRegex(RuntimeError, 'cannot stat mount source .*Errno 2'):
            RUNNER.block_node_number('/nonexistent/private-static-control')

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='private-static-fanout-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.target = self.root / 'slot' / 'target'
        self.target.mkdir(parents=True)
        self.first = self.root / 'a' / 'one.img'
        self.second = self.root / 'b' / 'two.img'
        for image in (self.first, self.second):
            image.parent.mkdir()
            image.write_bytes(b'')

    def host(self, layout):
        host = FakeHost(self.root / layout)
        host.disk('nvme0n1', '259:0')
        host.disk('nvme1n1', '259:9')
        host.nodes.update({'/dev/nvme0n1': '259:0', '/dev/nvme1n1': '259:9'})
        one = host.block('loop1', '7:1', backing=self.first)
        two = host.block('loop2', '7:2', backing=self.second)
        host.nodes.update({'/dev/loop1': '7:1', '/dev/loop2': '7:2'})
        host.mount(self.first, '0:41', '/dev/nvme0n1', '/a', LINE * 5)
        host.mount(self.second, '0:42', '/dev/nvme1n1', '/b', LINE * 2)
        return host, one, two

    def test_every_backing_filesystem_of_a_two_member_btrfs_is_measured(self):
        host, one, two = self.host('btrfs')
        host.btrfs('cafe', one, two)
        host.mount(self.target, '0:90', '/dev/loop1', '/slots', LINE * 9)
        chain = host.chain(self.target)
        self.assertEqual(sorted(f['mount_point'] for f in chain), ['/a', '/b', '/slots'])
        self.assertEqual(min(f['available_bytes'] for f in chain), LINE * 2)

    def test_every_member_of_an_md_array_is_followed(self):
        host, one, two = self.host('md')
        host.block('md0', '9:0', slaves=[one, two], md=True)
        host.mount(self.target, '9:0', '/dev/md0', '/upper', LINE * 9, fstype='ext4')
        chain = host.chain(self.target)
        self.assertEqual(sorted(f['mount_point'] for f in chain), ['/a', '/b', '/upper'])


if __name__ == '__main__':
    unittest.main()
