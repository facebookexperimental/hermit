"""Host controls; no stock dagrun, PID helper or producer is invoked.

The pinned AU pure Python model requires Python 3.10 or newer.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import shlex
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
            free=options.pop('free', lambda _: RUNNER.DISK_FLOOR_BYTES + 100),
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

    def test_projected_floor_preserves_exact_disk_arithmetic(self):
        threshold = 537_000_000_000 + 25_769_803_776
        RUNNER.projected_disk(threshold)
        with self.assertRaisesRegex(RuntimeError, 'do not lower the floor or reservation'):
            RUNNER.projected_disk(threshold - 1)
        self.assertIsNone(RUNNER.guard_breach(537_000_000_000, 16_777_216))
        self.assertIn('below 537000000000', RUNNER.guard_breach(536_999_999_999, 0))

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

    def test_disk_poll_uses_bound_bulk_target_instead_of_evidence_path(self):
        target = self.root / 'producer-target'
        target.mkdir()
        observed = []
        def free(path):
            observed.append(path)
            return (RUNNER.DISK_FLOOR_BYTES - 1 if path == target
                    else RUNNER.DISK_FLOOR_BYTES + RUNNER.DISK_RESERVATION_BYTES)
        result = self.engine('pass', free=free, disk_path=target)
        self.assertTrue(observed)
        self.assertTrue(all(path == target for path in observed))
        self.assertEqual(result['disk_guard_path'], str(target))
        self.assertTrue(any('below 537000000000' in value for value in result['guard_breaches']))

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
            return RUNNER.DISK_FLOOR_BYTES
        result = self.engine(program, free=free)
        self.assertEqual(result['native']['returncode'], 0)
        self.assertEqual(result['console_bytes'], 16_777_217)
        self.assertIn('console exceeds 16 MiB', result['guard_breaches'])

    def test_floor_breach_terminates_and_retains_native_status_and_time(self):
        result = self.engine('import time;time.sleep(60)',
                             free=lambda _: RUNNER.DISK_FLOOR_BYTES - 1)
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
            return RUNNER.DISK_FLOOR_BYTES - 1 if ready.exists() else RUNNER.DISK_FLOOR_BYTES
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
            return RUNNER.DISK_FLOOR_BYTES - 1 if pidfile.exists() else RUNNER.DISK_FLOOR_BYTES
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
            free=lambda _: RUNNER.DISK_FLOOR_BYTES)
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

    def test_cli_failed_preflight_names_receipt_and_working_remedy(self):
        self.retain_recipe(self.recipe([PYTHON, '-c', 'print("must-not-run")']))
        args = ['--name', 'preflight-control', '--recipe', str(self.root / 'recipe.json'),
                '--recipe-sha256', '0' * 64, '--source', str(self.root),
                '--source-head', 'a' * 40, '--engine', PYTHON, '--engine-sha256', '0' * 64,
                '--engine-provenance', str(self.root / 'unused.provenance'),
                '--engine-provenance-sha256', '0' * 64, '--pid-helper', str(self.root / 'unused.py'),
                '--pid-helper-sha256', RUNNER.PID_HELPER_SHA256, '--tmpdir', str(self.root),
                '--xdg-cache-home', str(self.root), '--output-root', str(self.root)]
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


if __name__ == '__main__':
    unittest.main()
