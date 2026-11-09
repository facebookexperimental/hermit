"""Data controls only; no source/build/child/ELF qualification claims."""
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

D = Path(__file__).resolve().parent

def load(name):
    spec = importlib.util.spec_from_file_location('cohort_test_'+name,D/(name+'.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

cohort = load('runtime_cohort')
publisher = load('publish')


# Reviewed historical source objects, not an admission of the current tree.
# The function-section Reverie object is the section change itself: the Hermit
# introducer's gitlink predates that change and matches only 30 of the 31 rows.
HISTORICAL_SOURCE_COMMITS = {
    'development-054-content-v1': {
        'H': '5f53234fbb0129114a34de8cb06dfdd7cde5d194',
        'R': 'a131082c93759d2666984a639e6d02813cd16e23',
    },
    'function-sections-054-content-v1': {
        'H': '97a088f04b8a13799861302dd9a3b04df3a5231f',
        'R': 'd2d6da03bb284b55f0c6199d56b6415f77b86794',
    },
    'current-split-runtime-content-v1': {
        'H': 'ee51613d1cd2dd1a6f26b053636614fd4a35a5ae',
        'R': 'e520999c018a22374b3a46cebbddfb9aba485e53',
    },
}


def _git_cat_file(owner, specs, limit):
    """Read only named local objects, with bounded output and elapsed time."""
    source = D.parents[2] / ('reverie' if owner == 'R' else '')
    request = ''.join(spec + '\n' for spec in specs).encode('ascii')
    if owner not in ('H', 'R') or not specs or len(request) > 4096 or not 0 < limit <= 64 * 1024 * 1024:
        raise RuntimeError('historical git request bounds')
    description = owner + ' ' + ', '.join(specs)
    environment = dict(os.environ, GIT_NO_LAZY_FETCH='1', GIT_NO_REPLACE_OBJECTS='1')
    with subprocess.Popen(['git', '--no-replace-objects', '-C', str(source), 'cat-file', '--batch'],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, env=environment) as child:
        try:
            deadline = time.monotonic() + 10
            child.stdin.write(request)
            child.stdin.close()
            result = bytearray()
            with selectors.DefaultSelector() as ready:
                ready.register(child.stdout, selectors.EVENT_READ)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not ready.select(remaining):
                        raise RuntimeError('historical git object read timed out: ' + description)
                    raw = os.read(child.stdout.fileno(), min(65536, limit - len(result) + 1))
                    if not raw:
                        break
                    result.extend(raw)
                    if len(result) > limit:
                        raise RuntimeError('historical git object output exceeds bound: ' + description)
            if child.wait(timeout=max(0.001, deadline - time.monotonic())):
                raise RuntimeError('historical git object read failed: ' + description)
            return bytes(result)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()


class ContentCohort(unittest.TestCase):
    def fixture(self,root):
        (root/'reverie').mkdir()
        for p,raw in [(root/'unit.rs',b'host role'),(root/'reverie/unit.rs',b'runtime role')]:
            p.write_bytes(raw)
        return (('H','unit.rs',9,hashlib.sha256(b'host role').hexdigest()),
                ('R','unit.rs',12,hashlib.sha256(b'runtime role').hexdigest()))
    def apply(self,root,rows):
        previous=cohort.COHORTS
        try:
            cohort.COHORTS=(('data-only',rows),)
            return cohort.bind(root)
        finally:
            cohort.COHORTS=previous
    def test_identical_content_in_distinct_source_roots(self):
        with tempfile.TemporaryDirectory() as a,tempfile.TemporaryDirectory() as b:
            first,second=Path(a),Path(b)
            rows=self.fixture(first);self.fixture(second)
            self.assertEqual(self.apply(first,rows)['cohort'],'data-only')
            self.assertEqual(self.apply(second,rows)['cohort'],'data-only')
            # These are not Git factories; real full-head custody remains mandatory.
    def test_changed_or_mixed_content_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            self.assertEqual(self.apply(root,rows)['cohort'],'data-only')
            (root/'reverie/unit.rs').write_bytes(b'changed role')
            with self.assertRaisesRegex(RuntimeError,'one complete reviewed content cohort'):
                self.apply(root,rows)
    def test_missing_file_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            self.assertEqual(self.apply(root,rows)['cohort'],'data-only')
            (root/'unit.rs').unlink()
            with self.assertRaises(RuntimeError): self.apply(root,rows)
    def test_symlink_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            self.assertEqual(self.apply(root,rows)['cohort'],'data-only')
            (root/'unit.rs').rename(root/'original.rs');(root/'unit.rs').symlink_to('original.rs')
            with self.assertRaisesRegex(RuntimeError,'indirect path'): self.apply(root,rows)
    def test_oversized_file_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            self.assertEqual(self.apply(root,rows)['cohort'],'data-only')
            with (root/'unit.rs').open('wb') as out: out.truncate(2*1024*1024+1)
            with self.assertRaisesRegex(RuntimeError,'oversized source'): self.apply(root,rows)
    def test_duplicate_or_crossed_declared_rows_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            for bad in [rows+(rows[0],),((rows[0][0],rows[0][1],rows[1][2],rows[1][3]),rows[1])]:
                self.assertEqual(self.apply(root,rows)['cohort'],'data-only')
                with self.assertRaisesRegex(RuntimeError,'one complete reviewed content cohort'):
                    self.apply(root,bad)

    def historical_fixture(self, root, name):
        rows = dict(cohort.COHORTS)[name]
        self.assertEqual(len(rows), 31)
        heads = HISTORICAL_SOURCE_COMMITS[name]
        for owner in ('H', 'R'):
            members = [row for row in rows if row[0] == owner]
            specs = [heads[owner] + ':' + path for _, path, _, _ in members]
            # Each fixed-size header/missing-object line is below 200 bytes.
            limit = sum(size + 200 for _, _, size, _ in members)
            stream = io.BytesIO(_git_cat_file(owner, specs, limit))
            for spec, (_, path, size, digest) in zip(specs, members):
                description = 'historical object ' + owner + ' ' + spec
                header = stream.readline(201)
                if header == (spec + ' missing\n').encode('ascii'):
                    raise RuntimeError('missing ' + description)
                fields = header.split()
                self.assertEqual(len(fields), 3, description)
                self.assertEqual(fields[1], b'blob', description)
                self.assertEqual(int(fields[2]), size, description)
                raw = stream.read(size)
                self.assertEqual(stream.read(1), b'\n', description)
                self.assertEqual((len(raw), hashlib.sha256(raw).hexdigest()),
                                 (size, digest), description)
                target = root / ('reverie' if owner == 'R' else '') / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(raw)
            self.assertEqual(stream.read(), b'')

    def historical_current_fixture(self, root):
        self.historical_fixture(root, 'function-sections-054-content-v1')

    def current_fixture(self, root):
        source = D.parents[2]
        for owner, path, _, _ in cohort.FINAL_FUNCTION_SECTIONS_054:
            relative = Path(path) if owner == 'H' else Path('reverie') / path
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / relative, target)
        # Construct the exact historical first-party source fixture. These
        # inverses affect this temporary data fixture only, never cohort.bind's
        # observed source or expected hashes; assert_complete_cohort checks all
        # 31 original whole-file identities below.
        inverses = {
            'detcore-liteinst/build.rs': [
                (b'//! src/lib.rs compiles common/inguest/glibc_compat.rs',
                 b'//! src/lib.rs compiles detcore-sabre/src/glibc_compat.rs'),
            ],
            'detcore-liteinst/src/lib.rs': [
                (b"// SHARED FILE: the guest preloads' glibc compatibility definitions, compiled",
                 b"// SHARED FILE: the SaBRe plugin's glibc compatibility definitions, compiled"),
                (b'#[path = "../../common/inguest/glibc_compat.rs"]',
                 b'#[path = "../../detcore-sabre/src/glibc_compat.rs"]'),
            ],
            'reverie/reverie-inguest/Cargo.toml': [
                (b'# Read-only native callback observations; no constructor or entry authority.\n'
                 b'native-entry-observer = ["coordinator-rpc"]\n', b''),
            ],
        }
        for relative, edits in inverses.items():
            path = root / relative
            raw = path.read_bytes()
            for current, historical in edits:
                self.assertEqual(raw.count(current), 1)
                raw = raw.replace(current, historical)
            path.write_bytes(raw)

    # Reverse only the two reviewed section-directive hunks in a temporary
    # fixture. The historical complete-file digest must still match exactly.
    trap_section_edits = (
        (b'    .pushsection .text.reverie_inguest_trusted_syscall,"ax",@progbits\n',
         b'    .text\n'),
        (b'    .size reverie_inguest_trusted_syscall, .-reverie_inguest_trusted_syscall\n'
         b'    .popsection\n\n    .text\n',
         b'    .size reverie_inguest_trusted_syscall, .-reverie_inguest_trusted_syscall\n\n'),
    )

    def development_fixture(self, root):
        self.historical_fixture(root, 'development-054-content-v1')
        trap = root / 'reverie/reverie-inguest/src/trap.rs'
        # Retain the reviewed transition checks, now against a separately
        # reconstructed immutable sectioned fixture rather than live sources.
        with tempfile.TemporaryDirectory() as directory:
            sectioned = Path(directory)
            self.historical_current_fixture(sectioned)
            raw = (sectioned / 'reverie/reverie-inguest/src/trap.rs').read_bytes()
        for before, after in self.trap_section_edits:
            self.assertEqual(raw.count(before), 1)
            raw = raw.replace(before, after)
        expected = next(row for row in cohort.FINAL_DEVELOPMENT_054
                        if row[:2] == ('R', 'reverie-inguest/src/trap.rs'))
        self.assertEqual((len(raw), hashlib.sha256(raw).hexdigest()), expected[2:])
        self.assertEqual(raw, trap.read_bytes())
        trap.write_bytes(raw)

    def assert_complete_cohort(self, root, name, rows):
        self.assertEqual(len(rows), 31)
        expected = {
            str(root / (Path(path) if owner == 'H' else Path('reverie') / path)):
                (size, digest)
            for owner, path, size, digest in rows
        }
        self.assertEqual(len(expected), 31)
        for path, fact in expected.items():
            raw = Path(path).read_bytes()
            self.assertEqual((len(raw), hashlib.sha256(raw).hexdigest()), fact)
        result = cohort.bind(root)
        self.assertEqual(result['cohort'], name)
        self.assertEqual(len(result['files']), 31)
        self.assertEqual({fact['path']: (fact['bytes'], fact['sha256'])
                          for fact in result['files']}, expected)

    def test_current_runtime_source_cohort_keeps_all_31_exact_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.current_fixture(root)
            self.assert_complete_cohort(root, 'function-sections-054-content-v1',
                                        cohort.FINAL_FUNCTION_SECTIONS_054)

    def test_development_runtime_source_cohort_keeps_all_31_exact_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.development_fixture(root)
            self.assert_complete_cohort(root, 'development-054-content-v1',
                                        cohort.FINAL_DEVELOPMENT_054)

    def test_partial_function_section_transition_is_not_a_reviewed_cohort(self):
        for before, after in self.trap_section_edits:
            with self.subTest(directive=before), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.historical_current_fixture(root)
                self.assert_complete_cohort(root, 'function-sections-054-content-v1',
                                            cohort.FINAL_FUNCTION_SECTIONS_054)
                trap = root / 'reverie/reverie-inguest/src/trap.rs'
                raw = trap.read_bytes()
                self.assertEqual(raw.count(before), 1)
                trap.write_bytes(raw.replace(before, after))
                with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                    cohort.bind(root)

    def test_one_previous_manifest_does_not_form_a_reviewed_cohort(self):
        for label, fixture, name, rows in [
                ('current_fixture', self.historical_current_fixture, 'function-sections-054-content-v1', cohort.FINAL_FUNCTION_SECTIONS_054),
                ('development_fixture', self.development_fixture, 'development-054-content-v1', cohort.FINAL_DEVELOPMENT_054)]:
            for path in ['reverie-inguest/Cargo.toml', 'reverie-loader/Cargo.toml']:
                with self.subTest(cohort=label, path=path), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    fixture(root)
                    self.assert_complete_cohort(root, name, rows)
                    manifest = root / 'reverie' / path
                    raw = manifest.read_bytes()
                    self.assertIn(b'0.5.4', raw)
                    manifest.write_bytes(raw.replace(b'0.5.4', b'0.5.1'))
                    with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                        cohort.bind(root)

    def test_unknown_manifest_version_is_not_a_runtime_source_identity(self):
        for label, fixture, name, rows in [
                ('current_fixture', self.historical_current_fixture, 'function-sections-054-content-v1', cohort.FINAL_FUNCTION_SECTIONS_054),
                ('development_fixture', self.development_fixture, 'development-054-content-v1', cohort.FINAL_DEVELOPMENT_054)]:
            with self.subTest(cohort=label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fixture(root)
                self.assert_complete_cohort(root, name, rows)
                for path in ['reverie-inguest/Cargo.toml', 'reverie-loader/Cargo.toml']:
                    manifest = root / 'reverie' / path
                    manifest.write_bytes(manifest.read_bytes().replace(b'0.5.4', b'0.5.5'))
                with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                    cohort.bind(root)


class NeutralCoreSupportCohort(unittest.TestCase):
    """Source-association controls only; neither compilation nor qualification."""

    def facts(self, rows, root):
        return {
            str(root / ('reverie' if label == 'R' else '') / relative):
                dict(path=str(root / ('reverie' if label == 'R' else '') / relative),
                     bytes=size, sha256=digest)
            for label, relative, size, digest in rows
        }

    def test_all_complete_historical_associations_remain_accepted(self):
        # Metadata fixtures exercise the complete selector with every old tuple
        # present. They do not substitute for observed-file or Git custody proof.
        root = Path('/data-only-cohort')
        historical = cohort.COHORTS[:-1]
        self.assertEqual(len(historical), 7)
        for name, rows in historical:
            with self.subTest(cohort=name):
                facts = self.facts(rows, root)
                with patch.object(cohort, 'observed_file', side_effect=lambda path: facts[str(path)]):
                    self.assertEqual(cohort.bind(root)['cohort'], name)

    def test_current_neutral_core_sources_match_complete_association(self):
        self.assertEqual(cohort.bind(D.parents[2])['cohort'], 'current-split-runtime-content-v1')

    def test_mixed_old_and_neutral_support_rows_are_refused(self):
        root = Path('/data-only-cohort')
        facts = self.facts(cohort.CURRENT_SPLIT_RUNTIME, root)
        old = self.facts(cohort.FINAL_DEVELOPMENT_054, root)
        path = str(root / 'detcore-liteinst/build.rs')
        with patch.object(cohort, 'observed_file', side_effect=lambda path: facts[str(path)]):
            self.assertEqual(cohort.bind(root)['cohort'], 'current-split-runtime-content-v1')
            facts[path] = old[path]
            with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                cohort.bind(root)

    def test_each_changed_source_refuses_same_size_mutation_and_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ContentCohort().historical_fixture(root, 'current-split-runtime-content-v1')
            for relative in ['detcore-liteinst/build.rs', 'detcore-liteinst/src/lib.rs']:
                path = root / relative
                original = path.read_bytes()
                for changed in [bytes([original[0] ^ 1]) + original[1:], original[:-1]]:
                    with self.subTest(path=relative, size=len(changed)):
                        self.assertEqual(cohort.bind(root)['cohort'], 'current-split-runtime-content-v1')
                        path.write_bytes(changed)
                        try:
                            with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                                cohort.bind(root)
                        finally:
                            path.write_bytes(original)


class FixedPageData(unittest.TestCase):
    def fixture(self):
        gate=0x6000e1001000;reference=gate+8192
        page=bytearray(b'\xcc'*4096)
        page[0x7c:0x85]=bytes.fromhex('f30f1efa0f0541ffe4')
        page[0x200:0x202]=b'\x0f\x05';page[0x309:0x30b]=b'\x0f\x05'
        symbols={
            'reverie_inguest_mapped_gate_page':dict(value=gate,type=1,size=4096),
            'reverie_inguest_fixed_gate_template':dict(value=reference,type=1,size=4096),
            'reverie_inguest_mapped_gate_page_end':dict(value=gate+4096,size=0),
            'reverie_inguest_fixed_gate_template_end':dict(value=reference+4096,size=0)}
        return gate,reference,page,symbols
    def apply(self,gate,reference,page,symbols,decoded,expected=None,reference_page=None):
        def span(address,size,permission):
            self.assertEqual(size,4096)
            self.assertEqual(permission,5 if address==gate else 4)
            return bytes(page if address==gate else (reference_page if reference_page is not None else page))
        return publisher.fixed_gate_data(symbols.__getitem__,span,
            lambda address,size,permission: 0 if address==gate else 8192,
            gate,reference,decoded,expected or hashlib.sha256(page).hexdigest())
    def test_object_pages_need_no_fabricated_decoded_rows(self):
        gate,reference,page,symbols=self.fixture()
        result=self.apply(gate,reference,page,symbols,{0x700002:{}})
        self.assertEqual(result,{gate+0x82,gate+0x202,gate+0x30b})
        self.assertEqual(sorted({0x700002}-result),[0x700002])
    def test_extra_decoded_gate_site_refused(self):
        gate,reference,page,symbols=self.fixture()
        self.assert_positive_page(gate,reference,page,symbols)
        with self.assertRaisesRegex(RuntimeError,'unexpected decoded'):
            self.apply(gate,reference,page,symbols,{gate+0x902:{}})
    def test_changed_reference_or_closed_digest_refused(self):
        gate,reference,page,symbols=self.fixture()
        self.assert_positive_page(gate,reference,page,symbols)
        with self.assertRaisesRegex(RuntimeError,'complete bytes/closed structural digest'):
            self.apply(gate,reference,page,symbols,{},reference_page=b'x'*4096)
        self.assert_positive_page(gate,reference,page,symbols)
        with self.assertRaisesRegex(RuntimeError,'complete bytes/closed structural digest'):
            self.apply(gate,reference,page,symbols,{},expected='0'*64)
    def test_end_alias_and_abi_refused(self):
        gate,reference,page,symbols=self.fixture()
        self.assert_positive_page(gate,reference,page,symbols)
        symbols['reverie_inguest_mapped_gate_page_end']['value']+=1
        with self.assertRaisesRegex(RuntimeError,'end alias'):self.apply(gate,reference,page,symbols,{})
        symbols['reverie_inguest_mapped_gate_page_end']['value']-=1
        self.assert_positive_page(gate,reference,page,symbols)
        page[0x7c]^=1
        with self.assertRaisesRegex(RuntimeError,'native gate ABI'):self.apply(gate,reference,page,symbols,{})
    def test_other_fixed_syscall_bytes_refused(self):
        gate,reference,page,symbols=self.fixture()
        self.assert_positive_page(gate,reference,page,symbols)
        page[0x200]^=1
        with self.assertRaisesRegex(RuntimeError,'syscall bytes'):self.apply(gate,reference,page,symbols,{})

    def assert_positive_page(self,gate,reference,page,symbols):
        self.assertEqual(self.apply(gate,reference,page,symbols,{}),
                         {gate+0x82,gate+0x202,gate+0x30b})


class HistoricalFixtureControls(unittest.TestCase):
    def test_development_fixture_does_not_copy_live_sources(self):
        case = ContentCohort()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(shutil, 'copyfile', side_effect=AssertionError('historical fixture copied live source')):
                case.development_fixture(root)
            case.assert_complete_cohort(root, 'development-054-content-v1',
                                        cohort.FINAL_DEVELOPMENT_054)

    def test_missing_historical_object_is_named_without_live_fallback(self):
        case = ContentCohort()
        name = 'function-sections-054-content-v1'
        missing = '0' * 40
        with tempfile.TemporaryDirectory() as good, tempfile.TemporaryDirectory() as bad:
            case.historical_fixture(Path(good), name)
            case.assert_complete_cohort(Path(good), name, cohort.FINAL_FUNCTION_SECTIONS_054)
            heads = dict(HISTORICAL_SOURCE_COMMITS[name], H=missing)
            with patch.dict(HISTORICAL_SOURCE_COMMITS, {name: heads}):
                with self.assertRaisesRegex(RuntimeError, 'missing historical object H ' + missing):
                    case.historical_fixture(Path(bad), name)

    def test_historical_object_hash_is_checked_before_use(self):
        case = ContentCohort()
        name = 'function-sections-054-content-v1'
        reader = _git_cat_file

        def changed_object(owner, specs, limit):
            raw = reader(owner, specs, limit)
            if owner == 'H':
                start = raw.index(b'\n') + 1
                raw = raw[:start] + bytes([raw[start] ^ 1]) + raw[start + 1:]
            return raw

        with tempfile.TemporaryDirectory() as good, tempfile.TemporaryDirectory() as bad:
            case.historical_fixture(Path(good), name)
            case.assert_complete_cohort(Path(good), name, cohort.FINAL_FUNCTION_SECTIONS_054)
            with patch(__name__ + '._git_cat_file', side_effect=changed_object):
                with self.assertRaisesRegex(AssertionError, 'historical object H .*detcore-liteinst/build.rs'):
                    case.historical_fixture(Path(bad), name)
            self.assertFalse((Path(bad) / 'detcore-liteinst/build.rs').exists())

    def test_every_negative_control_binds_a_positive_baseline(self):
        controls = {
            ContentCohort: [
                'test_changed_or_mixed_content_refused',
                'test_missing_file_refused',
                'test_symlink_refused',
                'test_oversized_file_refused',
                'test_duplicate_or_crossed_declared_rows_refused',
                'test_partial_function_section_transition_is_not_a_reviewed_cohort',
                'test_one_previous_manifest_does_not_form_a_reviewed_cohort',
                'test_unknown_manifest_version_is_not_a_runtime_source_identity',
            ],
            NeutralCoreSupportCohort: [
                'test_mixed_old_and_neutral_support_rows_are_refused',
                'test_each_changed_source_refuses_same_size_mutation_and_truncation',
            ],
            FixedPageData: [
                'test_extra_decoded_gate_site_refused',
                'test_changed_reference_or_closed_digest_refused',
                'test_end_alias_and_abi_refused',
                'test_other_fixed_syscall_bytes_refused',
            ],
        }
        for cls, methods in controls.items():
            for method in methods:
                with self.subTest(control=cls.__name__ + '.' + method):
                    events = []

                    def observe(callback):
                        def traced(*args, **kwargs):
                            try:
                                value = callback(*args, **kwargs)
                            except (RuntimeError, AssertionError):
                                events.append('refused')
                                raise
                            events.append('accepted')
                            return value
                        return traced

                    result = unittest.TestResult()
                    with patch.object(cohort, 'bind', side_effect=observe(cohort.bind)), \
                         patch.object(publisher, 'fixed_gate_data', side_effect=observe(publisher.fixed_gate_data)):
                        cls(method).run(result)
                    self.assertEqual(result.errors + result.failures, [])
                    self.assertTrue(events)
                    self.assertEqual(events, ['accepted', 'refused'] * (len(events) // 2))


if __name__ == '__main__':
    unittest.main()
