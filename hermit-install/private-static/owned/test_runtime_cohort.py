"""Data controls only; no source/build/child/ELF qualification claims."""
import hashlib
import importlib.util
from pathlib import Path
import shutil
import tempfile
import unittest

D = Path(__file__).resolve().parent

def load(name):
    spec = importlib.util.spec_from_file_location('cohort_test_'+name,D/(name+'.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

cohort = load('runtime_cohort')
publisher = load('publish')


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
            (root/'reverie/unit.rs').write_bytes(b'changed role')
            with self.assertRaisesRegex(RuntimeError,'one complete reviewed content cohort'):
                self.apply(root,rows)
    def test_missing_file_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root);(root/'unit.rs').unlink()
            with self.assertRaises(RuntimeError): self.apply(root,rows)
    def test_symlink_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            (root/'unit.rs').rename(root/'original.rs');(root/'unit.rs').symlink_to('original.rs')
            with self.assertRaisesRegex(RuntimeError,'indirect path'): self.apply(root,rows)
    def test_oversized_file_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            with (root/'unit.rs').open('wb') as out: out.truncate(2*1024*1024+1)
            with self.assertRaisesRegex(RuntimeError,'oversized source'): self.apply(root,rows)
    def test_duplicate_or_crossed_declared_rows_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);rows=self.fixture(root)
            for bad in [rows+(rows[0],),((rows[0][0],rows[0][1],rows[1][2],rows[1][3]),rows[1])]:
                with self.assertRaisesRegex(RuntimeError,'one complete reviewed content cohort'):
                    self.apply(root,bad)

    def current_fixture(self, root):
        source = D.parents[2]
        for owner, path, _, _ in cohort.FINAL_FUNCTION_SECTIONS_054:
            relative = Path(path) if owner == 'H' else Path('reverie') / path
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / relative, target)

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
        self.current_fixture(root)
        trap = root / 'reverie/reverie-inguest/src/trap.rs'
        raw = trap.read_bytes()
        for before, after in self.trap_section_edits:
            self.assertEqual(raw.count(before), 1)
            raw = raw.replace(before, after)
        expected = next(row for row in cohort.FINAL_DEVELOPMENT_054
                        if row[:2] == ('R', 'reverie-inguest/src/trap.rs'))
        self.assertEqual((len(raw), hashlib.sha256(raw).hexdigest()), expected[2:])
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
                self.current_fixture(root)
                trap = root / 'reverie/reverie-inguest/src/trap.rs'
                raw = trap.read_bytes()
                self.assertEqual(raw.count(before), 1)
                trap.write_bytes(raw.replace(before, after))
                with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                    cohort.bind(root)

    def test_one_previous_manifest_does_not_form_a_reviewed_cohort(self):
        for fixture in [self.current_fixture, self.development_fixture]:
            for path in ['reverie-inguest/Cargo.toml', 'reverie-loader/Cargo.toml']:
                with self.subTest(cohort=fixture.__name__, path=path), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    fixture(root)
                    manifest = root / 'reverie' / path
                    raw = manifest.read_bytes()
                    self.assertIn(b'0.5.4', raw)
                    manifest.write_bytes(raw.replace(b'0.5.4', b'0.5.1'))
                    with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                        cohort.bind(root)

    def test_unknown_manifest_version_is_not_a_runtime_source_identity(self):
        for fixture in [self.current_fixture, self.development_fixture]:
            with self.subTest(cohort=fixture.__name__), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fixture(root)
                for path in ['reverie-inguest/Cargo.toml', 'reverie-loader/Cargo.toml']:
                    manifest = root / 'reverie' / path
                    manifest.write_bytes(manifest.read_bytes().replace(b'0.5.4', b'0.5.5'))
                with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                    cohort.bind(root)


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
        with self.assertRaisesRegex(RuntimeError,'unexpected decoded'):
            self.apply(gate,reference,page,symbols,{gate+0x902:{}})
    def test_changed_reference_or_closed_digest_refused(self):
        gate,reference,page,symbols=self.fixture()
        with self.assertRaisesRegex(RuntimeError,'complete bytes/closed structural digest'):
            self.apply(gate,reference,page,symbols,{},reference_page=b'x'*4096)
        with self.assertRaisesRegex(RuntimeError,'complete bytes/closed structural digest'):
            self.apply(gate,reference,page,symbols,{},expected='0'*64)
    def test_end_alias_and_abi_refused(self):
        gate,reference,page,symbols=self.fixture()
        symbols['reverie_inguest_mapped_gate_page_end']['value']+=1
        with self.assertRaisesRegex(RuntimeError,'end alias'):self.apply(gate,reference,page,symbols,{})
        symbols['reverie_inguest_mapped_gate_page_end']['value']-=1
        page[0x7c]^=1
        with self.assertRaisesRegex(RuntimeError,'native gate ABI'):self.apply(gate,reference,page,symbols,{})
    def test_other_fixed_syscall_bytes_refused(self):
        gate,reference,page,symbols=self.fixture();page[0x200]^=1
        with self.assertRaisesRegex(RuntimeError,'syscall bytes'):self.apply(gate,reference,page,symbols,{})


if __name__ == '__main__':
    unittest.main()
