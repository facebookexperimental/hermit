"""Cancellation controls: KeyboardInterrupt/SystemExit keep their identity
after every required close, and ordinary failures keep their diagnostics."""
import ast
from contextlib import ExitStack
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest import mock

D = Path(__file__).resolve().parent

def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, D / relative)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

pipeline = load('test_cancel_pipeline', 'pipeline.py')
publisher = load('test_cancel_publish', 'publish.py')
cohort = load('test_cancel_cohort', 'runtime_cohort.py')
owned = load('test_cancel_owned', 'prepare.py')


class StageStatus(unittest.TestCase):
    def stage(self, main):
        return types.SimpleNamespace(__file__='/unit/stage.py', main=main)
    def test_success_status_continues(self):
        for status in [None, 0, False]:
            with self.subTest(status=status):
                pipeline.invoke(self.stage(lambda: status), ['--unit'])
    def test_nonzero_status_stops_the_pipeline(self):
        for status in [1, 2, True]:
            previous = list(sys.argv)
            with self.subTest(status=status):
                with self.assertRaisesRegex(RuntimeError, 'failure status: '+repr(status)):
                    pipeline.invoke(self.stage(lambda: status), ['--unit'])
                self.assertEqual(sys.argv, previous)
    def test_stage_cancellation_propagates_unchanged(self):
        for raised in [KeyboardInterrupt(), SystemExit(7)]:
            def main(): raise raised
            with self.subTest(raised=raised):
                with self.assertRaises(type(raised)) as caught:
                    pipeline.invoke(self.stage(main), [])
                self.assertIs(caught.exception, raised)


class RawInspections(unittest.TestCase):
    def fixture(self, directory):
        target = Path(directory) / 'initializer-definitions.not-qualified.elf'
        target.write_bytes(b'\x7fELF unit')
        c = dict(link_paths=dict(output_directory=directory), H=directory,
                 inspection_tools=dict(readelf=dict(path='/unit/readelf'), nm=dict(path='/unit/nm')))
        return c, {str(target): owned.file_fact(target)}
    def test_cancellation_starts_no_next_command(self):
        for raised in [KeyboardInterrupt(), SystemExit(3)]:
            calls = []
            def command(name, *args, **kwargs):
                calls.append(name)
                raise raised
            with self.subTest(raised=raised), tempfile.TemporaryDirectory() as directory:
                c, produced = self.fixture(directory)
                with self.assertRaises(type(raised)) as caught:
                    owned.inspect_produced(c, Path(directory), {}, produced, command)
                self.assertIs(caught.exception, raised)
                self.assertEqual(calls, ['final-headers'])
    def test_ordinary_failures_still_aggregate_every_command(self):
        calls = []
        def command(name, *args, **kwargs):
            calls.append(name)
            raise RuntimeError('unit failure ' + name)
        with tempfile.TemporaryDirectory() as directory:
            c, produced = self.fixture(directory)
            with self.assertRaisesRegex(RuntimeError, 'strict raw final inspections failed') as caught:
                owned.inspect_produced(c, Path(directory), {}, produced, command)
        self.assertEqual(len(calls), 5)
        for name in calls:
            self.assertIn('unit failure ' + name, str(caught.exception))


class CohortCloses(unittest.TestCase):
    def setUp(self):
        self.saved = cohort.COHORTS, cohort.observed_file
        cohort.COHORTS = [('unit', [('H', 'a', 1, 'aa'), ('H', 'b', 1, 'bb'), ('H', 'c', 1, 'cc')])]
        self.reads = []
    def tearDown(self):
        cohort.COHORTS, cohort.observed_file = self.saved
    def install(self, fail):
        def observed(path):
            self.reads.append(Path(path).name)
            failure = fail(Path(path).name, self.reads.count(Path(path).name))
            if failure is not None:
                raise failure
            return dict(path=str(path), bytes=1, sha256=Path(path).name * 2)
        cohort.observed_file = observed
    def test_body_cancellation_closes_reads_then_keeps_identity(self):
        for raised in [KeyboardInterrupt(), SystemExit(9)]:
            self.reads = []
            self.install(lambda name, count: raised if (name, count) == ('c', 1) else None)
            with self.subTest(raised=raised):
                with self.assertRaises(type(raised)) as caught:
                    cohort.bind('/unit')
                self.assertIs(caught.exception, raised)
                self.assertEqual(self.reads, ['a', 'b', 'c', 'a', 'b'])
    def test_close_cancellation_runs_later_closes(self):
        raised = KeyboardInterrupt()
        self.install(lambda name, count: raised if (name, count) == ('a', 2) else None)
        with self.assertRaises(KeyboardInterrupt) as caught:
            cohort.bind('/unit')
        self.assertIs(caught.exception, raised)
        self.assertEqual(self.reads, ['a', 'b', 'c', 'a', 'b', 'c'])
        self.assertIn('KeyboardInterrupt', str(caught.exception.__cause__))
    def test_ordinary_primary_and_secondary_failures_are_reported(self):
        def fail(name, count):
            if (name, count) == ('c', 1): return OSError('primary')
            if (name, count) == ('a', 2): return ValueError('secondary')
        self.install(fail)
        with self.assertRaises(RuntimeError) as caught:
            cohort.bind('/unit')
        self.assertIn('primary', str(caught.exception))
        self.assertIn('secondary', str(caught.exception))
        self.assertIsInstance(caught.exception.__cause__, OSError)
        self.assertEqual(self.reads, ['a', 'b', 'c', 'a', 'b'])


class PublishCleanup(unittest.TestCase):
    def roles(self):
        def owner(): return dict(symbol='actual_owner',offset=1,owner_sha256='a'*64)
        return dict(fallback_entry=owner(),completion_syscall=owner(),
                    sites=[dict(owner=owner(),number=number,operation=operation)
                           for operation,number in [(1,158),(2,218),(3,7),(4,2)]])
    def run_publish(self, directory, headers_error, close_error=None):
        root = Path(directory)
        image = root / 'image.elf'; image.write_bytes(b'\x7fELF unit')
        helper = root / 'helper.py'
        helper.write_text('def module(name):\n    raise HEADERS_ERROR\n')
        result_path = root / 'result.json'
        result_path.write_text(json.dumps(dict(state='PASS_OWNED_FINAL_LINK_INSPECTION_ONLY',
            source_tools_closed=True, actual_lld_invocations=1, elf=dict(path=str(image)))))
        coordinates_path = root / 'coordinates.json'
        coordinates_path.write_text(json.dumps(dict(state='RECORDS_OBSERVED_NOT_QUALIFIED',
            source_tools_closed=True, receipt='unit-receipt', ELF=dict(path=str(image)))))
        m = types.SimpleNamespace(file_fact=lambda path: 'unit-receipt', sha=lambda path: 'unit',
                                  LEGACY=helper, LEGACY_SHA='unit')
        closed = []
        real_close = os.close
        def close(fd):
            closed.append(fd)
            real_close(fd)
            if close_error is not None:
                raise close_error
        saved_os, saved_spec = publisher.os, publisher.importlib.util.spec_from_file_location
        def spec(name, path):
            value = saved_spec(name, path)
            loader = value.loader
            class Loader:
                def create_module(self, spec): return None
                def exec_module(self, module):
                    module.HEADERS_ERROR = headers_error
                    loader.exec_module(module)
            value.loader = Loader()
            return value
        publisher.os = types.SimpleNamespace(**{k: getattr(os, k) for k in dir(os) if not k.startswith('__')})
        publisher.os.close = close
        publisher.importlib.util.spec_from_file_location = spec
        try:
            publisher.publish(m, {}, result_path, coordinates_path, self.roles(), root / 'package', None)
        finally:
            publisher.os = saved_os
            publisher.importlib.util.spec_from_file_location = saved_spec
            self.assertEqual(len(closed), 1, 'retained image descriptor closed exactly once')
    def test_body_cancellation_keeps_identity_after_cleanup(self):
        for raised in [KeyboardInterrupt(), SystemExit(5)]:
            with self.subTest(raised=raised), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(type(raised)) as caught:
                    self.run_publish(directory, raised)
                self.assertIs(caught.exception, raised)
    def test_cleanup_cancellation_after_ordinary_body_failure(self):
        raised = KeyboardInterrupt()
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(KeyboardInterrupt) as caught:
                self.run_publish(directory, OSError('primary'), close_error=raised)
        self.assertIs(caught.exception, raised)
        self.assertIn('primary', str(caught.exception.__cause__))
    def test_ordinary_primary_and_cleanup_failures_are_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(RuntimeError) as caught:
                self.run_publish(directory, OSError('primary'), close_error=ValueError('secondary'))
        self.assertIn('primary', str(caught.exception))
        self.assertIn('secondary', str(caught.exception))
        self.assertIsInstance(caught.exception.__cause__, OSError)




archive = load('test_cancel_archive', 'archive/prepare.py')
observer = load('test_cancel_observer', 'observe_coordinates.py')
section = load('test_cancel_section', 'section/prepare.py')


def thrown(error):
    def fail(*args, **kwargs):
        raise error
    return fail


class BoundedCommandCleanup(unittest.TestCase):
    def test_cancellation_attempts_every_close_and_reaps_after_kill(self):
        for raised in [KeyboardInterrupt(), SystemExit(11)]:
            with self.subTest(raised=raised):
                events = []
                def action(name, error=None, value=None):
                    def run(*args, **kwargs):
                        events.append((name, kwargs))
                        if error is not None:
                            raise error
                        return value
                    return run
                pipes = [types.SimpleNamespace(fileno=lambda: 10,
                         close=action(name, OSError(name))) for name in ['stdout', 'stderr']]
                child = types.SimpleNamespace(pid=123, stdout=pipes[0], stderr=pipes[1],
                                              wait=action('wait', value=-9))
                selector = types.SimpleNamespace(register=lambda *args: None,
                    get_map=lambda: True, select=thrown(raised), close=action('selector', OSError('selector')))
                row = {}
                with mock.patch.object(owned.subprocess, 'Popen', return_value=child), \
                     mock.patch.object(owned.selectors, 'DefaultSelector', return_value=selector), \
                     mock.patch.object(owned.os, 'set_blocking'), \
                     mock.patch.object(owned.os, 'killpg', side_effect=action('kill', OSError('kill'))):
                    with self.assertRaises(type(raised)) as caught:
                        owned.bounded_stream_command(['/unit/child'], '/', {}, None, None, 1024, 1, row)
                self.assertIs(caught.exception, raised)
                self.assertEqual([name for name, _ in events], ['kill', 'selector', 'stdout', 'stderr', 'wait'])
                self.assertEqual(events[-1][1], {'timeout': 5})
                self.assertEqual(row['exit'], -9)
                self.assertEqual(len(row['cleanup_errors']), 4)

    def test_actual_child_is_killed_and_reaped_on_cancellation(self):
        for raised in [KeyboardInterrupt(), SystemExit(31)]:
            with self.subTest(raised=raised), tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
                selector = owned.selectors.DefaultSelector()
                row = {}
                with mock.patch.object(owned.selectors, 'DefaultSelector', return_value=selector), \
                     mock.patch.object(selector, 'select', side_effect=raised):
                    with self.assertRaises(type(raised)) as caught:
                        owned.bounded_stream_command([sys.executable, '-I', '-c',
                            'import time; time.sleep(60)'], '/', {}, stdout, stderr, 1024, 5, row)
                self.assertIs(caught.exception, raised)
                self.assertTrue(row['retained_leader_group_kill_sent'])
                self.assertEqual(row['exit'], -9)
                with self.assertRaises(ChildProcessError):
                    os.waitpid(row['pid'], os.WNOHANG)

    def test_ordinary_child_exit_and_output_are_unchanged(self):
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            row = {}
            owned.bounded_stream_command([sys.executable, '-I', '-c',
                'import sys; print("child-output"); sys.stderr.write("child-error\\n"); sys.exit(7)'],
                '/', {}, stdout, stderr, 1024, 5, row)
            stdout.seek(0); stderr.seek(0)
            self.assertEqual(stdout.read(), b'child-output\n')
            self.assertEqual(stderr.read(), b'child-error\n')
        self.assertEqual(row['exit'], 7)
        self.assertNotIn('cleanup_errors', row)


class MainCleanup(unittest.TestCase):
    def producer(self, module, directory):
        root = Path(directory)
        config = dict(state='PREPARED_NOT_RUN', out=str(root / 'out'), H_head='a'*40,
            tool_R_head='b'*40, runtime_R_pin='c'*40, R=str(root), target=str(root / 'target'),
            census_objdump='/unit/objdump', **{'host-cargo': '/unit/cargo'})
        if module is owned:
            names = ['prepare.py', 'main.rs', 'factory_controls.rs', 'structural_observation.rs',
                'native_leaf_controls.rs', 'optional_controls.rs', 'ordinary_controls.rs',
                'terminal_observation.rs', 'view_leaf_controls.rs', 'view_leaf_protocol.py']
            config['files'] = {str(D/name): owned.sha(D/name) for name in names}
            config['runtime_handoff_source'] = {'path': '/unit/handoff.rs'}
            config['files']['/unit/handoff.rs'] = owned.REQUIRED_HANDOFF_SOURCE_SHA256
        binding = root / 'binding.json'; binding.write_text(json.dumps(config))
        return config, [module.__file__, 'run', '--binding', str(binding),
                        '--binding-sha256', module.sha(binding)]

    def test_producer_mains_keep_cancellation_through_close_and_receipt_failure(self):
        for module in [owned, archive]:
            for raised in [KeyboardInterrupt(), SystemExit(23)]:
                with self.subTest(module=module.__name__, raised=raised), \
                     tempfile.TemporaryDirectory() as directory, ExitStack() as patches:
                    config, argv = self.producer(module, directory)
                    cancelled = False
                    events = []; saved = []
                    def command(*args, **kwargs):
                        nonlocal cancelled
                        cancelled = True
                        events.append('command')
                        raise raised
                    def close(*args):
                        events.append('close')
                        if cancelled:
                            raise OSError('source custody')
                    def private(*args):
                        events.append('private')
                        return {'unit': 'same'}
                    real_save = module.save
                    def save(path, value):
                        saved.append(json.loads(json.dumps(value)))
                        if value['state'] == 'FAIL':
                            raise OSError('receipt failure')
                        real_save(path, value)
                    patches.enter_context(mock.patch.object(sys, 'argv', argv))
                    patches.enter_context(mock.patch.object(module, 'files_close', side_effect=close))
                    patches.enter_context(mock.patch.object(module, 'private_close', side_effect=private))
                    patches.enter_context(mock.patch.object(module, 'declared_child_environment', return_value={}))
                    patches.enter_context(mock.patch.object(module, 'save', side_effect=save))
                    if module is owned:
                        patches.enter_context(mock.patch.object(module, 'runtime_content_preflight'))
                        patches.enter_context(mock.patch.object(module, 'runtime_handoff_preflight',
                                                               return_value=config['runtime_handoff_source']))
                        patches.enter_context(mock.patch.object(module, 'bounded_stream_command', side_effect=command))
                    else:
                        patches.enter_context(mock.patch.object(module.subprocess, 'run', side_effect=command))
                    with self.assertRaises(type(raised)) as caught:
                        module.main()
                    self.assertIs(caught.exception, raised)
                    self.assertEqual(events.count('command'), 1, 'no later producer command')
                    self.assertEqual(events.count('private'), 2, 'private close still attempted')
                    self.assertEqual(saved[-1]['state'], 'FAIL')
                    self.assertIn('source custody', repr(saved[-1]['close_errors']))
                    self.assertEqual(len(saved[-1]['commands']), 1)

    def test_producer_main_cleanup_cancellation_keeps_identity_and_diagnostics(self):
        for module in [owned, archive]:
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory() as directory, ExitStack() as patches:
                _, argv = self.producer(module, directory)
                raised = KeyboardInterrupt()
                closes = mock.Mock(side_effect=[OSError('body'), raised])
                private = mock.Mock(return_value={'unit': 'same'})
                patches.enter_context(mock.patch.object(sys, 'argv', argv))
                patches.enter_context(mock.patch.object(module, 'files_close', closes))
                patches.enter_context(mock.patch.object(module, 'private_close', private))
                if module is owned:
                    patches.enter_context(mock.patch.object(module, 'runtime_content_preflight'))
                    patches.enter_context(mock.patch.object(module, 'runtime_handoff_preflight',
                                                           return_value={'path': '/unit/handoff.rs'}))
                with self.assertRaises(KeyboardInterrupt) as caught:
                    module.main()
                self.assertIs(caught.exception, raised)
                self.assertEqual(closes.call_count, 2)
                private.assert_called_once()
                result = json.loads((Path(directory)/'out/result.json').read_text())
                self.assertEqual(result['state'], 'FAIL')
                self.assertIn('body', result['error'])
                self.assertIn('KeyboardInterrupt', repr(result['close_errors']))

    def test_section_main_cancellation_survives_final_save(self):
        for raised in [KeyboardInterrupt(), SystemExit(19)]:
            with self.subTest(raised=raised), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                args = {'build-result': str(root/'build.json'), 'build-sha256': 'a'*64,
                    'build-binding': str(root/'binding.json'), 'build-recipe': str(root/'recipe.json'),
                    'h-head': 'a'*40, 'r-head': 'b'*40, 'attempt': 'unit', 'evidence-root': directory,
                    'inspection-tools-json': '/unit/tools', 'inspection-tools-sha256': 'c'*64,
                    'memcpy-source': '/unit/memcpy', 'musl-source-authority': '/unit/musl',
                    'stack-chk-source': '/unit/stack'}
                argv = [section.__file__, *[v for k, value in args.items() for v in ['--'+k, value]]]
                real_save = section.save; saved = []
                def save(path, value):
                    saved.append(dict(value))
                    if value['state'] == 'FAIL':
                        raise OSError('final save')
                    real_save(path, value)
                with mock.patch.object(sys, 'argv', argv), \
                     mock.patch.object(section, 'sha', side_effect=raised), \
                     mock.patch.object(section, 'save', side_effect=save):
                    with self.assertRaises(type(raised)) as caught:
                        section.main()
                self.assertIs(caught.exception, raised)
                self.assertEqual(saved[-1]['state'], 'FAIL')
                self.assertEqual(saved[-1]['commands'], [])

    def test_pipeline_main_closes_and_saves_without_replacing_cancellation(self):
        for raised in [KeyboardInterrupt(), SystemExit(17)]:
            with self.subTest(raised=raised), tempfile.TemporaryDirectory() as directory, ExitStack() as patches:
                root = Path(directory)
                paths = {key: str(root/key) for key in ['H', 'runtime_R', 'tool_R', 'cargo', 'rustc',
                    'host_cargo', 'host_rustc', 'target', 'cargo_home', 'native_inputs_json', 'runner']}
                config = dict(paths=paths, heads={key: 'a'*40 for key in ['H', 'runtime_R', 'tool_R']},
                              digests={'native_inputs': 'b'*64, 'runner': 'c'*64})
                bound = root/'binding.json'; bound.write_text(json.dumps(dict(state='PREPARED_NOT_RUN',
                    config=config, sources={}, out=str(root/'out'))))
                digest = pipeline.sha(bound); real_sha = pipeline.sha; closed = []; saved = []
                def sha(path):
                    closed.append(str(path))
                    if len(closed) == 3:
                        raise OSError('close source')
                    return real_sha(path)
                def save(path, value):
                    saved.append(dict(value))
                    raise OSError('final save')
                stage = types.SimpleNamespace(__file__='/unit/stage.py', main=thrown(raised),
                    runtime_content_preflight=lambda c: None)
                patches.enter_context(mock.patch.object(sys, 'argv', [pipeline.__file__, 'run',
                    '--binding', str(bound), '--binding-sha256', digest]))
                patches.enter_context(mock.patch.object(pipeline, 'validate_config'))
                patches.enter_context(mock.patch.object(pipeline, 'module', return_value=stage))
                patches.enter_context(mock.patch.object(pipeline, 'sha', side_effect=sha))
                patches.enter_context(mock.patch.object(pipeline, 'save', side_effect=save))
                with self.assertRaises(type(raised)) as caught:
                    pipeline.main()
                self.assertIs(caught.exception, raised)
                self.assertEqual(len(closed), 3, 'final source close attempted')
                self.assertEqual(saved[-1]['state'], 'FAIL')
                self.assertIn('close source', repr(saved[-1]['close_errors']))

    def test_observer_main_command_cancel_survives_custody_and_save(self):
        for raised in [KeyboardInterrupt(), SystemExit(13)]:
            with self.subTest(raised=raised), tempfile.TemporaryDirectory() as directory, ExitStack() as patches:
                root = Path(directory); produced = root/'producer'; produced.mkdir()
                elf = produced/'initializer-definitions.not-qualified.elf'; elf.write_bytes(b'unit ELF')
                link_map = elf.with_name('initializer-definitions.map'); link_map.write_bytes(b'unit map')
                tool = root/'tool'; tool.write_bytes(b'unit tool')
                c = dict(out=str(produced), evidence_root=directory, link_paths={'output_directory': str(produced)},
                    H=directory, H_head='a'*40, runtime_R_pin='b'*40, tool_R_head='c'*40,
                    census_objdump=str(tool), inspection_tools={'readelf': {'path': str(tool)}},
                    files={str(tool): owned.sha(tool)}, source_before={'unit': 'same'})
                binding = root/'binding.json'; binding.write_text(json.dumps(c))
                result_path = produced/'result.json'
                result_path.write_text(json.dumps(dict(state='PASS_OWNED_FINAL_LINK_INSPECTION_ONLY',
                    source_tools_closed=True, actual_lld_invocations=1, binding_sha256=owned.sha(binding),
                    elf=owned.file_fact(elf), produced_outputs={str(p): owned.file_fact(p) for p in [elf, link_map]})))
                argv = [observer.__file__, '--result', str(result_path), '--result-sha256', owned.sha(result_path),
                    '--binding', str(binding), '--binding-sha256', owned.sha(binding), '--out', str(root/'observed')]
                cancelled = False; events = []; saved = []
                def command(*args, **kwargs):
                    nonlocal cancelled
                    cancelled = True; events.append('command'); raise raised
                def close(*args):
                    events.append('close')
                    if cancelled: raise OSError('custody')
                def save(path, value):
                    saved.append(value)
                    raise OSError('final save')
                m = observer.m
                patches.enter_context(mock.patch.object(sys, 'argv', argv))
                patches.enter_context(mock.patch.object(m, 'runtime_content_preflight'))
                patches.enter_context(mock.patch.object(m, 'files_close', side_effect=close))
                patches.enter_context(mock.patch.object(m, 'snapshot', return_value=c['source_before']))
                private = patches.enter_context(mock.patch.object(m, 'private_close', return_value={'unit': 'same'}))
                patches.enter_context(mock.patch.object(m, 'declared_child_environment', return_value={}))
                patches.enter_context(mock.patch.object(m, 'bounded_stream_command', side_effect=command))
                patches.enter_context(mock.patch.object(m, 'save', side_effect=save))
                with self.assertRaises(type(raised)) as caught:
                    observer.main()
                self.assertIs(caught.exception, raised)
                self.assertEqual(events, ['close', 'close', 'command', 'close', 'close'])
                self.assertEqual(private.call_count, 2)
                self.assertEqual(saved[-1]['state'], 'FAIL')
                self.assertIn('custody', repr(saved[-1]['close_errors']))
                self.assertEqual(len(saved[-1]['commands']), 1)


class SectionCommandCleanup(unittest.TestCase):
    def test_actual_nested_command_preserves_cancel_and_ordinary_custody_error(self):
        # Execute the actual nested function unchanged. Its enclosing setup joins
        # a real compiler/archive receipt; no compiler is needed to test cleanup.
        tree = ast.parse((D/'section/prepare.py').read_text())
        main = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'main')
        command = next(node for node in ast.walk(main) if isinstance(node, ast.FunctionDef) and node.name == 'run')
        for error in [KeyboardInterrupt(), SystemExit(29), OSError('body')]:
            with self.subTest(error=error), tempfile.TemporaryDirectory() as directory:
                custody = ValueError('custody')
                guard = mock.Mock(side_effect=[None, custody]); result = {'commands': []}
                namespace = dict(vars(section), out=Path(directory), h=Path(directory), env={},
                                 produced={}, result=result, guard=guard)
                exec(compile(ast.Module(body=[command], type_ignores=[]), section.__file__, 'exec'), namespace)
                with mock.patch.object(section.subprocess, 'run', side_effect=error):
                    expected = error if not isinstance(error, Exception) else custody
                    with self.assertRaises(type(expected)) as caught:
                        namespace['run']('unit', ['/unit/tool'])
                self.assertIs(caught.exception, expected)
                self.assertEqual(guard.call_count, 2)
                row = json.loads((Path(directory)/'result.json').read_text())['commands'][0]
                self.assertEqual(row['state'], 'FAIL')
                self.assertEqual(row['error'], repr(error))
                self.assertEqual(row['custody_error'], repr(custody))
                self.assertEqual(row['stdout']['bytes'], 0)
                self.assertEqual(row['stderr']['bytes'], 0)




class StreamContextCleanup(unittest.TestCase):
    def command(self, module, directory, error):
        name = 'command' if module in [owned, archive] else 'run'
        tree = ast.parse(Path(module.__file__).read_text())
        main = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'main')
        function = next(node for node in ast.walk(main) if isinstance(node, ast.FunctionDef) and node.name == name)
        def execute(*args, **kwargs):
            if error is not None:
                raise error
            if args and isinstance(args[-1], dict):
                args[-1]['exit'] = 0
            return types.SimpleNamespace(returncode=0)
        run = mock.Mock(side_effect=execute)
        namespace = dict(vars(module), out=Path(directory), c={'H': directory}, h=Path(directory),
            env={}, produced={}, result={'commands': []}, record={'commands': []},
            files_close=lambda c: None, declared_child_environment=lambda *args, **kwargs: {},
            close=lambda: None, guard=lambda: None, bounded_stream_command=run,
            subprocess=types.SimpleNamespace(run=run, DEVNULL=-3))
        if module is observer:
            namespace['m'] = types.SimpleNamespace(**dict(vars(observer.m),
                bounded_stream_command=run, declared_child_environment=lambda *args, **kwargs: {}))
        exec(compile(ast.Module(body=[function], type_ignores=[]), module.__file__, 'exec'), namespace)
        args = ['unit', ['/unit/command']]
        if name == 'command':
            args.append(directory)
        return lambda: namespace[name](*args), run

    def exercise(self, module, error, fail_open=False, close_errors=None, expected=None):
        with tempfile.TemporaryDirectory() as directory:
            invoke, child = self.command(module, directory, error)
            original_open = Path.open
            closed = []
            if close_errors is None:
                close_error = OSError('buffered stream close')
                close_errors = {'.stdout' if fail_open else '.stderr': close_error}
                expected = error if not isinstance(error, Exception) else close_error
            class Stream:
                def __init__(self, path):
                    self.path = path
                    self.stream = original_open(path, 'xb')
                def __enter__(self):
                    return self.stream
                def close(self):
                    self.stream.close()
                    closed.append(self.path.suffix)
                    failure = close_errors.get(self.path.suffix)
                    if failure is not None:
                        raise failure
                def __exit__(self, *args):
                    self.close()
            def opened(path, mode='r', *args, **kwargs):
                if mode == 'xb' and path.suffix in ['.stdout', '.stderr']:
                    if fail_open and path.suffix == '.stderr':
                        raise error
                    return Stream(path)
                return original_open(path, mode, *args, **kwargs)
            with mock.patch.object(Path, 'open', new=opened):
                if expected is None:
                    invoke()
                else:
                    caught = None
                    try:
                        invoke()
                    except (KeyboardInterrupt, SystemExit, Exception) as error:
                        caught = error
                    # Catch unexpected cancellation types too, so a broken
                    # implementation fails this oracle instead of aborting
                    # unittest before the rest of the precedence matrix.
                    self.assertIs(caught, expected)
            self.assertEqual(child.call_count, 0 if fail_open else 1)
            self.assertEqual(closed, ['.stdout'] if fail_open else ['.stderr', '.stdout'])

    def test_body_cancellation_survives_buffered_close_in_all_commands(self):
        for module in [owned, archive, observer, section]:
            for error in [KeyboardInterrupt(), SystemExit(41), ValueError('ordinary body')]:
                with self.subTest(module=module.__name__, error=error):
                    self.exercise(module, error)

    def test_second_stream_open_cancellation_survives_first_close(self):
        for module in [owned, archive, observer, section]:
            for error in [KeyboardInterrupt(), SystemExit(43)]:
                with self.subTest(module=module.__name__, error=error):
                    self.exercise(module, error, fail_open=True)


    def test_cleanup_cancellation_precedence_and_all_close_callbacks(self):
        for module in [owned, archive, observer, section]:
            for cancellation_error in [KeyboardInterrupt(), SystemExit(47)]:
                body = ValueError('ordinary body')
                later_error = OSError('later close')
                with self.subTest(module=module.__name__, cancellation=cancellation_error, phase='first close'):
                    self.exercise(module, body,
                        close_errors={'.stderr': cancellation_error, '.stdout': later_error},
                        expected=cancellation_error)
                with self.subTest(module=module.__name__, cancellation=cancellation_error, phase='last close'):
                    self.exercise(module, body,
                        close_errors={'.stderr': later_error, '.stdout': cancellation_error},
                        expected=cancellation_error)
                later_cancellation = SystemExit(53) if isinstance(cancellation_error, KeyboardInterrupt) else KeyboardInterrupt()
                with self.subTest(module=module.__name__, cancellation=cancellation_error, phase='both closes'):
                    self.exercise(module, None,
                        close_errors={'.stderr': cancellation_error, '.stdout': later_cancellation},
                        expected=cancellation_error)
                with self.subTest(module=module.__name__, cancellation=cancellation_error, phase='body then close'):
                    self.exercise(module, cancellation_error,
                        close_errors={'.stderr': later_cancellation, '.stdout': later_error},
                        expected=cancellation_error)

    def test_ordinary_stream_outcomes_keep_original_behavior(self):
        for module in [owned, archive, observer, section]:
            body = ValueError('ordinary body')
            first = OSError('first close')
            last = RuntimeError('last close')
            for label, error, closes, expected in [
                ('success', None, {}, None),
                ('body', body, {}, body),
                ('first close', None, {'.stderr': first}, first),
                ('both closes', body, {'.stderr': first, '.stdout': last}, last),
            ]:
                with self.subTest(module=module.__name__, phase=label):
                    self.exercise(module, error, close_errors=closes, expected=expected)


class CleanupCallbackOrdering(unittest.TestCase):
    def cancellations(self):
        class OwnerCancellation(BaseException):
            pass
        return [KeyboardInterrupt(), SystemExit(37), GeneratorExit(), OwnerCancellation()]

    def test_cohort_keeps_first_of_multiple_cleanup_cancellations(self):
        for first in self.cancellations():
            second = SystemExit(41)
            calls = []
            def observed(path):
                name = Path(path).name
                calls.append(name)
                if calls.count(name) == 2:
                    if name == 'a': raise first
                    if name == 'b': raise second
                    raise OSError('last close')
                return dict(path=str(path), bytes=1, sha256=name * 2)
            rows = [('unit', [('H', name, 1, name * 2) for name in ['a', 'b', 'c']])]
            with self.subTest(first=first), \
                 mock.patch.object(cohort, 'COHORTS', rows), \
                 mock.patch.object(cohort, 'observed_file', side_effect=observed):
                with self.assertRaises(type(first)) as caught:
                    cohort.bind('/unit')
            self.assertIs(caught.exception, first)
            self.assertEqual(calls, ['a', 'b', 'c', 'a', 'b', 'c'])
            for error in [first, second, 'last close']:
                self.assertIn(repr(error) if not isinstance(error, str) else error,
                              str(caught.exception.__cause__))

    def test_collector_keeps_first_cleanup_cancel_and_attempts_reap(self):
        for first in self.cancellations():
            calls = []
            def action(name, error=None):
                def run(*args, **kwargs):
                    calls.append((name, kwargs))
                    if error is not None: raise error
                    return -9
                return run
            pipes = [types.SimpleNamespace(fileno=lambda: 10, close=action(name, OSError(name)))
                     for name in ['stdout', 'stderr']]
            child = types.SimpleNamespace(pid=123, stdout=pipes[0], stderr=pipes[1],
                                          wait=action('wait'))
            selector = types.SimpleNamespace(register=lambda *args: None, get_map=lambda: True,
                select=thrown(ValueError('body')), close=action('selector', SystemExit(41)))
            row = {}
            with self.subTest(first=first), \
                 mock.patch.object(owned.subprocess, 'Popen', return_value=child), \
                 mock.patch.object(owned.selectors, 'DefaultSelector', return_value=selector), \
                 mock.patch.object(owned.os, 'set_blocking'), \
                 mock.patch.object(owned.os, 'killpg', side_effect=action('kill', first)):
                with self.assertRaises(type(first)) as caught:
                    owned.bounded_stream_command(['/unit/child'], '/', {}, None, None, 1024, 1, row)
            self.assertIs(caught.exception, first)
            self.assertEqual([name for name, _ in calls], ['kill', 'selector', 'stdout', 'stderr', 'wait'])
            self.assertEqual(calls[-1][1], {'timeout': 5})
            self.assertEqual(row['exit'], -9)
            self.assertEqual(len(row['cleanup_errors']), 4)

    def test_producer_cleanup_cancellations_preserve_order_and_final_journal(self):
        for module in [owned, archive]:
            for first in self.cancellations():
                with self.subTest(module=module.__name__, first=first), \
                     tempfile.TemporaryDirectory() as directory, ExitStack() as patches:
                    _, argv = MainCleanup.producer(self, module, directory)
                    events = []; saved = []
                    def close(*args):
                        events.append('source')
                        if len(events) == 1: raise ValueError('body')
                        raise first
                    def private(*args):
                        events.append('private')
                        raise SystemExit(41)
                    real_save = module.save
                    def save(path, value):
                        if value['state'] != 'FAIL':
                            return real_save(path, value)
                        events.append('journal')
                        saved.append(json.loads(json.dumps(value)))
                        raise OSError('journal')
                    patches.enter_context(mock.patch.object(sys, 'argv', argv))
                    patches.enter_context(mock.patch.object(module, 'files_close', side_effect=close))
                    patches.enter_context(mock.patch.object(module, 'private_close', side_effect=private))
                    patches.enter_context(mock.patch.object(module, 'save', side_effect=save))
                    if module is owned:
                        patches.enter_context(mock.patch.object(module, 'runtime_content_preflight'))
                        patches.enter_context(mock.patch.object(module, 'runtime_handoff_preflight',
                                                               return_value={'path': '/unit/handoff.rs'}))
                    with self.assertRaises(type(first)) as caught:
                        module.main()
                    self.assertIs(caught.exception, first)
                    self.assertEqual(events, ['source', 'source', 'private', 'journal'])
                    self.assertEqual(saved[-1]['state'], 'FAIL')
                    self.assertIn('body', saved[-1]['error'])
                    self.assertEqual(len(saved[-1]['close_errors']), 2)

    def test_publish_cleanup_continues_owned_rollback_after_multiple_cancellations(self):
        # Exercise the actual finalizer with its publication-ownership flags set.
        # No producer/ELF inspection is needed to reach the late rollback boundary.
        tree = ast.parse(Path(publisher.__file__).read_text())
        publish = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'publish')
        transaction = next(n for n in publish.body if isinstance(n, ast.Try) and n.finalbody)
        fixture = ast.parse('def finalizer():\n    cancelled = None\n    final_errors = []\n').body[0]
        fixture.body.extend(transaction.finalbody)
        for first in self.cancellations():
            with self.subTest(first=first), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                pending_image, pending_data, image_path, data_path, sentinel = [root/name for name in
                    ['pending-image', 'pending-data', 'image', 'data', 'foreign-sentinel']]
                for path in [pending_image, pending_data, image_path, data_path, sentinel]:
                    path.write_bytes(path.name.encode())
                fd = os.open(image_path, os.O_RDONLY)
                events = []; real_close = os.close; real_unlink = Path.unlink
                def close(descriptor):
                    events.append('close'); real_close(descriptor); raise first
                def unlink(path, *args, **kwargs):
                    events.append(path.name)
                    if path == pending_image: raise SystemExit(41)
                    return real_unlink(path, *args, **kwargs)
                namespace = dict(vars(publisher), fd=fd, original_error=ValueError('body'),
                    pending_image=pending_image, pending_data=pending_data, image_path=image_path,
                    data_path=data_path, created_image=True, created_data=True)
                exec(compile(ast.fix_missing_locations(ast.Module(body=[fixture], type_ignores=[])),
                             publisher.__file__, 'exec'), namespace)
                fake_os = types.SimpleNamespace(close=close)
                namespace['os'] = fake_os
                with mock.patch.object(Path, 'unlink', unlink):
                    with self.assertRaises(type(first)) as caught:
                        namespace['finalizer']()
                self.assertIs(caught.exception, first)
                self.assertEqual(events, ['close', 'pending-image', 'pending-data', 'data', 'image'])
                self.assertTrue(pending_image.exists(), 'failed unlink remains a cleanup failure')
                self.assertFalse(pending_data.exists())
                self.assertFalse(data_path.exists())
                self.assertFalse(image_path.exists())
                self.assertEqual(sentinel.read_bytes(), b'foreign-sentinel')
                self.assertIn('body', str(caught.exception.__cause__))
                self.assertIn('SystemExit(41)', str(caught.exception.__cause__))


if __name__ == '__main__':
    unittest.main()
