"""Prepared data-only controls of the actual corrected AST predicate; unrun."""
import ast,hashlib,unittest
from pathlib import Path
SOURCE=Path(__file__).with_name('prepare.py')
SHA = 'f78167f4cc6520b6316d006695eb68d073aeee0b83d37ce67ffa7c9601029d45'
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
if __name__=='__main__':unittest.main()
