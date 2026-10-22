"""Data-only controls of actual pipeline/role guards; no producer or guest."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

D = Path(__file__).resolve().parent

def load(name):
    spec = importlib.util.spec_from_file_location('test_'+name,D/(name+'.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

pipeline = load('pipeline')
publisher = load('publish')


class DataRoles(unittest.TestCase):
    def fixture(self):
        def owner(): return dict(symbol='actual_owner',offset=1,owner_sha256='a'*64)
        return dict(fallback_entry=owner(),completion_syscall=owner(),
                    sites=[dict(owner=owner(),number=number,operation=operation)
                           for operation,number in [(1,158),(2,218),(3,7),(4,2)]])
    def test_closed_conditional_data(self):
        publisher.validate_roles(self.fixture())
    def test_wrong_or_duplicate_operation_refused(self):
        for change in [('operation',3),('number',7)]:
            data=self.fixture();data['sites'][0][change[0]]=change[1]
            with self.assertRaisesRegex(RuntimeError,'source-contract roles differ'):
                publisher.validate_roles(data)
    def test_extra_coordinate_or_authority_field_refused(self):
        for value in ['entry','ready','absolute_address']:
            data=self.fixture();data[value]=True
            with self.assertRaisesRegex(RuntimeError,'unknown/missing'):
                publisher.validate_roles(data)
    def test_offset_bool_negative_or_oversized_refused(self):
        for value in [True,-1,256*1024]:
            data=self.fixture();data['fallback_entry']['offset']=value
            with self.assertRaisesRegex(RuntimeError,'offset/hash'):
                publisher.validate_roles(data)
    def test_unknown_hash_or_symbol_refused(self):
        for key,value in [('owner_sha256','A'*64),('owner_sha256','a'*63),('symbol','')]:
            data=self.fixture();data['completion_syscall'][key]=value
            with self.assertRaises(RuntimeError): publisher.validate_roles(data)
    def test_removed_native_row_refused(self):
        data=self.fixture();data['sites'].pop()
        with self.assertRaisesRegex(RuntimeError,'exactly four'):
            publisher.validate_roles(data)


class SourceCustody(unittest.TestCase):
    def fixture(self,directory):
        root=Path(directory);module=root/'unit.py';module.write_text('retained original bytes\n')
        manifest=root/'source-files.json'
        manifest.write_text(json.dumps([dict(path='unit.py',sha256=pipeline.sha(module))]))
        return root,module,manifest
    def test_actual_producer_source_guard_changed_module(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root;expected=pipeline.sha(manifest)
                pipeline.source_files(expected)
                module.write_text('changed bytes\n')
                with self.assertRaisesRegex(RuntimeError,'producer source changed'):
                    pipeline.source_files(expected)
            finally: pipeline.D=previous
    def test_actual_producer_source_guard_changed_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root;expected=pipeline.sha(manifest)
                manifest.write_text('[]')
                with self.assertRaisesRegex(RuntimeError,'manifest differs'):
                    pipeline.source_files(expected)
            finally: pipeline.D=previous
    def test_path_escape_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root
                manifest.write_text(json.dumps([dict(path='../unit.py',sha256='a'*64)]))
                with self.assertRaisesRegex(RuntimeError,'path escape'):
                    pipeline.source_files(pipeline.sha(manifest))
            finally: pipeline.D=previous
    def test_unknown_config_shape_refused(self):
        with self.assertRaisesRegex(RuntimeError,'config shape'):
            pipeline.validate_config(dict(schema='hermit.private-static-producer-inputs.v1',ready=True))


class AttemptIsolation(unittest.TestCase):
    def test_stage_helper_names_never_reuse_another_attempt(self):
        roots = [Path('/data/run-a'), Path('/data/run-b'), Path('/other/run-a')]
        names = [pipeline.stage_output(root, stage).name
                 for root in roots for stage in ('archive', 'owned')]
        self.assertEqual(len(set(names)), 6)
        for root in roots:
            for stage in ('archive', 'owned'):
                self.assertEqual(pipeline.stage_output(root, stage).parent, root)
    def test_unknown_stage_refuses(self):
        with self.assertRaisesRegex(RuntimeError, 'unknown pipeline stage'):
            pipeline.stage_output(Path('/data/run-a'), '../escape')


if __name__ == '__main__':
    unittest.main()
