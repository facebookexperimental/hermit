"""Data controls only; no source/build/child/ELF qualification claims."""
import hashlib
import importlib.util
from pathlib import Path
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
