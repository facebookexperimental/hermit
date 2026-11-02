"""Prepared data-only controls of the actual corrected AST predicate; unrun."""
import ast,hashlib,unittest
import copy
import tempfile
import tomllib
from pathlib import Path
SOURCE=Path(__file__).with_name('prepare.py')
SHA = 'ad4b38c02fddefd46d7a0fe4aa64c6df828955012040bf9098fe4f8f1d24b7cd'
class InventoryIdentity(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        raw=SOURCE.read_bytes()
        if hashlib.sha256(raw).hexdigest()!=SHA:raise AssertionError('candidate changed')
        tree=ast.parse(raw)
        calls=[n for n in ast.walk(tree) if isinstance(n,ast.Call) and isinstance(n.func,ast.Name) and n.func.id=='need' and len(n.args)==2 and isinstance(n.args[1],ast.Constant) and n.args[1].value=='combined native inputs differ from actual bound link inputs']
        if len(calls)!=1:raise AssertionError('actual predicate absent/duplicate')
        cls.predicate=compile(ast.Expression(calls[0].args[0]),str(SOURCE),'eval')
    def fixture(self):
        paths={'musl_source_authority':'/unit/authority','libc':'/unit/libc.a','memcpy_source':'/unit/memcpy.s'}
        c={'files':{'/unit/authority/inventory.json':'actual-inventory-sha'}}
        obs={'link_map':{'path':'/unit/current.map','sha256':'actual-map-sha'}}
        combined={'native_memcpy':{'link_map':dict(obs['link_map']),'archive':{'path':paths['libc']},'source':{'path':paths['memcpy_source']},'source_inventory':{'path':'/unit/authority/inventory.json','sha256':'actual-inventory-sha'}}}
        return dict(paths=paths,c=c,obs=obs,combined=combined)
    def accepts(self,f):return eval(self.predicate,{'Path':Path,'__builtins__':{'str':str}},f)
    def test_exact_actual_inventory_file_identity(self):self.assertTrue(self.accepts(self.fixture()))
    def test_directory_is_not_inventory_file(self):
        f=self.fixture();f['combined']['native_memcpy']['source_inventory']['path']='/unit/authority';self.assertFalse(self.accepts(f))
    def test_equal_digest_wrong_file_is_refused(self):
        f=self.fixture();f['combined']['native_memcpy']['source_inventory']['path']='/unit/other/inventory.json';self.assertFalse(self.accepts(f))
    def test_wrong_inventory_digest_is_refused(self):
        f=self.fixture();f['combined']['native_memcpy']['source_inventory']['sha256']='other';self.assertFalse(self.accepts(f))
    def test_other_three_joins_stay_exact(self):
        for role in ['link_map','archive','source']:
            f=self.fixture();f['combined']['native_memcpy'][role]['path']='/unit/wrong'
            with self.subTest(role=role):self.assertFalse(self.accepts(f))
    def test_missing_declared_inventory_is_refused(self):
        f=self.fixture();f['c']['files'].clear()
        with self.assertRaises(KeyError):self.accepts(f)


class HostCheckerProfiles(unittest.TestCase):
    """Data-only controls of the actual Cargo admission helpers, without Cargo."""

    DEPENDENCIES = {
        'serde_json': '1.0.145',
        'sha2': '0.10.9',
        'digest': '0.10.7',
        'goblin': '0.10.3',
    }
    REGISTRY = 'registry+https://github.com/rust-lang/crates.io-index'

    @classmethod
    def setUpClass(cls):
        raw = SOURCE.read_bytes()
        if hashlib.sha256(raw).hexdigest() != SHA:
            raise AssertionError('candidate changed')
        names = {'need', 'host_checker_profile', 'host_checker_dependencies'}
        helpers = [node for node in ast.parse(raw).body
                   if isinstance(node, ast.FunctionDef) and node.name in names]
        if len(helpers) != len(names) or {node.name for node in helpers} != names:
            raise AssertionError('actual host checker helpers absent/duplicate')
        namespace = {'Path': Path}
        exec(compile(ast.Module(body=helpers, type_ignores=[]), str(SOURCE), 'exec'),
             namespace)
        cls.check_profile = staticmethod(namespace['host_checker_profile'])
        cls.check_dependencies = staticmethod(namespace['host_checker_dependencies'])

    @staticmethod
    def profile(opt_level='2', debuginfo=2):
        return {
            'opt_level': opt_level,
            'debuginfo': debuginfo,
            'debug_assertions': True,
            'overflow_checks': True,
            'test': False,
        }

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='host-checker-controls-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.cargo_home = self.root / 'cargo-home'
        self.target = self.root / 'target'
        lock_path = self.root / 'Cargo.lock'
        lock_path.write_text('version = 4\n' + ''.join(
            f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
            f'source = "{self.REGISTRY}"\n'
            for name, version in self.DEPENDENCIES.items()))
        with lock_path.open('rb') as stream:
            self.lock = tomllib.load(stream)
        self.records = [self.make_artifact(name, version)
                        for name, version in self.DEPENDENCIES.items()]
        # The same locked package emits both a library and a custom-build row.
        # Their different roles, not opt0 versus opt2, distinguish the outputs.
        library = self.artifact(self.records, 'serde_json')
        build_script = copy.deepcopy(library)
        build_source = self.write_file(Path(library['manifest_path']).parent / 'build.rs',
                                       b'fn main() {}\n')
        build_output = self.write_file(
            self.target / 'debug/build/serde_json-0123456789abcdef/build-script-build')
        build_script.update({
            'target': {'name': 'build-script-build', 'kind': ['custom-build'],
                       'crate_types': ['bin'], 'src_path': str(build_source)},
            'profile': self.profile('0', 0),
            'filenames': [str(build_output)],
            'executable': None,
        })
        self.records.append(build_script)
        # Cargo's build-only libraries/macros legitimately retain opt0/debug0.
        self.records.extend([
            self.make_artifact('build_support', '1.0.0', optimized=False),
            self.make_artifact('build_macro', '1.0.0', optimized=False,
                               kind='proc-macro'),
            {'reason': 'build-finished', 'success': True},
        ])

    def write_file(self, path, contents=b'fixture output\n'):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)
        return path

    def make_artifact(self, name, version, optimized=True, kind='lib'):
        package = self.cargo_home / 'registry/src/unit-registry' / f'{name}-{version}'
        manifest_text = f'[package]\nname = "{name}"\nversion = "{version}"\n'
        manifest = self.write_file(package / 'Cargo.toml', manifest_text.encode())
        source = self.write_file(package / 'src/lib.rs', b'// data-only fixture\n')
        suffix = 'so' if kind == 'proc-macro' else 'rlib'
        output = self.write_file(self.target / 'debug/deps' / f'lib{name}-unit.{suffix}')
        return {
            'reason': 'compiler-artifact',
            'package_id': f'{self.REGISTRY}#{name}@{version}',
            'manifest_path': str(manifest),
            'target': {'name': name, 'kind': [kind], 'crate_types': [kind],
                       'src_path': str(source)},
            'profile': self.profile() if optimized else self.profile('0', 0),
            'filenames': [str(output)],
            'executable': None,
            'fresh': False,
        }

    def artifact(self, records, name):
        return next(row for row in records if row.get('target', {}).get('name') == name)

    def check(self, records=None, lock=None):
        return self.check_dependencies(self.records if records is None else records,
                                       self.lock if lock is None else lock,
                                       self.cargo_home, self.target)

    def test_exact_four_locked_libraries_and_build_only_artifacts_are_accepted(self):
        selected = self.check()
        self.assertEqual(set(selected), set(self.DEPENDENCIES))
        for name, row in selected.items():
            self.assertIs(row, self.artifact(self.records, name))
            self.assertEqual(row['profile'], self.profile())
        for name in ('build_support', 'build_macro'):
            self.assertEqual(self.artifact(self.records, name)['profile'],
                             self.profile('0', 0))
            self.assertNotIn(name, selected)
        for profile in (self.profile('0', 0), self.profile()):
            with self.subTest(custom_build_profile=profile):
                records = copy.deepcopy(self.records)
                build_script = self.artifact(records, 'build-script-build')
                build_script['profile'] = profile
                selected = self.check(records)
                self.assertEqual(set(selected), set(self.DEPENDENCIES))
                self.assertEqual(build_script['package_id'], selected['serde_json']['package_id'])
                self.assertIs(selected['serde_json'], self.artifact(records, 'serde_json'))
                self.assertIsNot(selected['serde_json'], build_script)

    def test_fresh_and_rebuilt_outputs_are_both_observed_without_relabelling(self):
        for fresh in (True, False):
            with self.subTest(fresh=fresh):
                records = copy.deepcopy(self.records)
                for row in records:
                    if row['reason'] == 'compiler-artifact':
                        row['fresh'] = fresh
                selected = self.check(records)
                self.assertEqual(set(selected), set(self.DEPENDENCIES))
                for row in selected.values():
                    self.assertIs(row['fresh'], fresh)

    def test_profile_fields_are_complete_and_exact(self):
        for field in self.profile():
            with self.subTest(missing=field):
                profile = self.profile()
                del profile[field]
                with self.assertRaisesRegex(RuntimeError, '^HOST checker profile fields differ$'):
                    self.check_profile(profile)
        for profile in (None, [], dict(self.profile(), unexpected=True)):
            with self.subTest(profile=profile):
                with self.assertRaisesRegex(RuntimeError, '^HOST checker profile fields differ$'):
                    self.check_profile(profile)

    def test_profile_types_and_cargo_ranges_are_strict(self):
        bad_values = {
            'opt_level': (2, True, None, '4', ''),
            'debuginfo': (True, False, '2', None, -1, 3),
        }
        for field, values in bad_values.items():
            for value in values:
                with self.subTest(field=field, value=value):
                    profile = self.profile()
                    profile[field] = value
                    with self.assertRaisesRegex(
                            RuntimeError, '^HOST checker profile types or safety fields differ$'):
                        self.check_profile(profile)
        for opt_level in ('0', '1', '2', '3', 's', 'z'):
            for debuginfo in (0, 1, 2):
                with self.subTest(opt_level=opt_level, debuginfo=debuginfo):
                    self.assertIsNone(self.check_profile(self.profile(opt_level, debuginfo)))

    def test_every_emitted_artifact_keeps_all_safety_fields(self):
        artifacts = [row for row in self.records if row['reason'] == 'compiler-artifact']
        bad_values = {
            'debug_assertions': (False, 1, 'true', None),
            'overflow_checks': (False, 1, 'true', None),
            'test': (True, 0, 'false', None),
        }
        for artifact in artifacts:
            name = artifact['target']['name']
            for field, values in bad_values.items():
                for value in values:
                    with self.subTest(artifact=name, field=field, value=value):
                        records = copy.deepcopy(self.records)
                        self.artifact(records, name)['profile'][field] = value
                        with self.assertRaisesRegex(
                                RuntimeError, '^HOST checker profile types or safety fields differ$'):
                            self.check(records)

    def test_each_selected_library_requires_opt2_and_debug2(self):
        for name in self.DEPENDENCIES:
            alternatives = [('opt_level', level) for level in ('0', '1', '3', 's', 'z')]
            alternatives += [('debuginfo', level) for level in (0, 1)]
            for field, value in alternatives:
                with self.subTest(dependency=name, field=field, value=value):
                    records = copy.deepcopy(self.records)
                    self.artifact(records, name)['profile'][field] = value
                    with self.assertRaisesRegex(
                            RuntimeError, '^selected HOST checker library is not opt2/debug2$'):
                        self.check(records)

    def test_locked_dependencies_cannot_be_missing_duplicated_or_from_another_source(self):
        for name in self.DEPENDENCIES:
            for case in ('missing', 'duplicate', 'other-source'):
                with self.subTest(dependency=name, case=case):
                    lock = copy.deepcopy(self.lock)
                    package = next(row for row in lock['package'] if row['name'] == name)
                    if case == 'missing':
                        lock['package'].remove(package)
                    elif case == 'duplicate':
                        lock['package'].append(dict(package))
                    else:
                        package['source'] = 'registry+https://example.invalid/index'
                    with self.assertRaisesRegex(
                            RuntimeError, '^HOST checker locked dependency absent/ambiguous: ' + name + '$'):
                        self.check(lock=lock)

    def test_missing_and_duplicate_candidates_include_malformed_duplicates(self):
        for name in self.DEPENDENCIES:
            for case in ('missing', 'duplicate', 'malformed-duplicate', 'disguised-library'):
                with self.subTest(dependency=name, case=case):
                    records = copy.deepcopy(self.records)
                    row = self.artifact(records, name)
                    if case == 'missing':
                        records.remove(row)
                        if name == 'serde_json':
                            # The genuine script remains; it cannot substitute
                            # for its package's missing library artifact.
                            self.assertEqual(
                                self.artifact(records, 'build-script-build')['package_id'],
                                row['package_id'])
                    else:
                        duplicate = copy.deepcopy(row)
                        if case == 'malformed-duplicate':
                            # Its target name still claims this dependency. A
                            # wrong package/kind must not hide the ambiguity.
                            duplicate['package_id'] = 'path+file:///wrong-package#0.0.0'
                            duplicate['target']['kind'] = ['proc-macro']
                        elif case == 'disguised-library':
                            # Keep the locked package, lib crate type, lib.rs
                            # and rlib. Renaming two role fields cannot exempt
                            # this competing library as a real build script.
                            duplicate['target']['name'] = 'build-script-build'
                            duplicate['target']['kind'] = ['custom-build']
                        records.append(duplicate)
                    with self.assertRaisesRegex(
                            RuntimeError, '^HOST checker dependency output absent/ambiguous: ' + name + '$'):
                        self.check(records)

    def test_malformed_duplicate_profiles_are_refused_before_candidate_selection(self):
        cases = {
            'missing-profile': 'HOST checker profile fields differ',
            'missing-field': 'HOST checker profile fields differ',
            'unsafe-profile': 'HOST checker profile types or safety fields differ',
        }
        for name in self.DEPENDENCIES:
            for case, reason in cases.items():
                with self.subTest(dependency=name, case=case):
                    records = copy.deepcopy(self.records)
                    original = self.artifact(records, name)
                    self.assertEqual(original['profile'], self.profile())
                    duplicate = copy.deepcopy(original)
                    if case == 'missing-profile':
                        del duplicate['profile']
                    elif case == 'missing-field':
                        del duplicate['profile']['debuginfo']
                    else:
                        duplicate['profile']['overflow_checks'] = False
                    records.append(duplicate)
                    with self.assertRaisesRegex(RuntimeError, '^' + reason + '$'):
                        self.check(records)

    def test_locked_version_and_emitted_package_target_identity_must_agree(self):
        for name in self.DEPENDENCIES:
            for case in ('locked-version', 'package-source', 'package-version',
                         'target-name', 'target-kind', 'crate-types', 'executable', 'source-path'):
                with self.subTest(dependency=name, case=case):
                    records = copy.deepcopy(self.records)
                    lock = copy.deepcopy(self.lock)
                    row = self.artifact(records, name)
                    if case == 'locked-version':
                        next(q for q in lock['package'] if q['name'] == name)['version'] = '99.0.0'
                    elif case == 'package-source':
                        row['package_id'] = f'registry+https://example.invalid/index#{name}@{self.DEPENDENCIES[name]}'
                    elif case == 'package-version':
                        row['package_id'] = f'{self.REGISTRY}#{name}@99.0.0'
                    elif case == 'target-name':
                        row['target']['name'] = 'other_library'
                    elif case == 'target-kind':
                        row['target']['kind'] = ['bin']
                    elif case == 'crate-types':
                        row['target']['crate_types'] = ['cdylib']
                    elif case == 'executable':
                        row['executable'] = str(self.target / 'debug/unexpected-executable')
                    else:
                        other = Path(row['manifest_path']).parent / 'src/other.rs'
                        row['target']['src_path'] = str(self.write_file(other))
                    with self.assertRaisesRegex(
                            RuntimeError, '^HOST checker dependency identity differs: ' + name + '$'):
                        self.check(records, lock)

    def test_manifest_must_be_actual_canonical_file_in_exact_registry_package(self):
        name = 'serde_json'
        original = Path(self.artifact(self.records, name)['manifest_path'])
        outside = self.write_file(self.root / 'elsewhere' / original.parent.name / 'Cargo.toml')
        wrong_version = self.write_file(original.parent.with_name(name + '-99.0.0') / 'Cargo.toml')
        wrong_basename = self.write_file(original.with_name('Other.toml'), original.read_bytes())
        alias = original.with_name('manifest-alias.toml')
        alias.symlink_to(original)
        directory = original.with_name('manifest-directory.toml')
        directory.mkdir()
        manifests = {
            'outside-registry': outside,
            'wrong-package-version': wrong_version,
            'existing-wrong-basename': wrong_basename,
            'missing-file': original.with_name('absent.toml'),
            'symlink': alias,
            'directory': directory,
            'noncanonical': original.parent / '..' / original.parent.name / 'Cargo.toml',
            'relative': Path('relative-registry') / original.parent.name / 'Cargo.toml',
        }
        for case, manifest in manifests.items():
            with self.subTest(case=case):
                records = copy.deepcopy(self.records)
                row = self.artifact(records, name)
                row['manifest_path'] = str(manifest)
                row['target']['src_path'] = str(manifest.parent / 'src/lib.rs')
                with self.assertRaisesRegex(
                        RuntimeError, '^HOST checker dependency identity differs: serde_json$'):
                    self.check(records)

    def test_all_outputs_are_regular_canonical_debug_files_with_one_rlib(self):
        name = 'sha2'
        original = Path(self.artifact(self.records, name)['filenames'][0])
        metadata = self.write_file(original.with_suffix('.rmeta'))
        second = self.write_file(original.with_name('libsha2-second.rlib'))
        outside = self.write_file(self.root / 'outside/libsha2.rlib')
        release = self.write_file(self.target / 'release/deps/libsha2.rlib')
        alias = original.with_name('libsha2-alias.rlib')
        alias.symlink_to(original)
        directory = original.with_name('libsha2-directory.rlib')
        directory.mkdir()
        # A normal rlib plus metadata is legal; every sidecar must be checked.
        records = copy.deepcopy(self.records)
        self.artifact(records, name)['filenames'] = [str(original), str(metadata)]
        self.assertEqual(set(self.check(records)), set(self.DEPENDENCIES))
        outputs = {
            'empty': [],
            'metadata-only': [metadata],
            'two-rlibs': [original, second],
            'outside-target': [outside],
            'release-output': [release],
            'missing-file': [original.with_name('libsha2-absent.rlib')],
            'symlink': [alias],
            'directory': [directory],
            'noncanonical': [original.parent / '..' / 'deps' / original.name],
            'relative': [Path('target/debug/deps') / original.name],
            'unsafe-sidecar': [original, self.write_file(self.root / 'outside/sha2.rmeta')],
        }
        for case, filenames in outputs.items():
            with self.subTest(case=case):
                records = copy.deepcopy(self.records)
                self.artifact(records, name)['filenames'] = [str(path) for path in filenames]
                with self.assertRaisesRegex(
                        RuntimeError, '^HOST checker dependency outputs differ: sha2$'):
                    self.check(records)

    def test_build_finished_is_unique_and_success_is_a_boolean_true(self):
        for case in ('missing', 'duplicate', 'false', 'integer', 'string', 'null', 'no-success'):
            with self.subTest(case=case):
                records = copy.deepcopy(self.records)
                finished = records[-1]
                if case == 'missing':
                    records.remove(finished)
                elif case == 'duplicate':
                    records.append(dict(finished))
                elif case == 'no-success':
                    del finished['success']
                else:
                    finished['success'] = {
                        'false': False, 'integer': 1, 'string': 'true', 'null': None,
                    }[case]
                with self.assertRaisesRegex(RuntimeError, '^Cargo did not complete one successful build$'):
                    self.check(records)


if __name__=='__main__':unittest.main()
