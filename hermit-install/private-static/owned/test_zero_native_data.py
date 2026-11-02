"""DATA-only guards; actual source-owned production is tested separately."""
import copy
import hashlib
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('zero_native_publisher',Path(__file__).parent/'publish.py')
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


class ExactNativeData(unittest.TestCase):
    def fixture(self):
        code = bytes.fromhex('4889febf02100000b89e0000000f05c3')
        digest = hashlib.sha256(code).hexdigest()
        owner = dict(name='__set_thread_area',type=2,size=0,value=4096,binding=0,visibility=2)
        expected = dict(symbol='__set_thread_area',offset=13,owner_sha256=digest)
        artifact = dict(path='actual.elf',sha256='a'*64)
        link_map = dict(path='actual.map',sha256='b'*64)
        runtime = dict(path='actual-runtime.a',sha256='c'*64)
        libc = dict(path='actual-libc.a',sha256='d'*64)
        leaf = dict(symbol='__set_thread_area',actual_symbol_size=0,final_entry=4096,
                    final_binding=0,final_visibility=2,archive=libc,member_ordinal=1223,
                    member_header=5904434,member_sha256='0f842b24b5425118e12f36fd97e7fd7e91b97728e49a7b52c26a84192754416c',
                    input_section=1,complete_section_bytes=16,code_sha256=digest)
        observed = dict(scope='SOURCE_MEMBER_MAP_RX_IDENTITIES_ONLY_NOT_SYSCALL_OR_ENTRY_AUTHORIZATION',
                        artifact=artifact,link_map=link_map,runtime_archive=runtime,set_thread_area=leaf)
        return observed,owner,expected,artifact,link_map,runtime,libc,code

    def apply(self,values):
        observed,owner,expected,artifact,link_map,runtime,libc,code=values
        def span(address,length,permission):
            self.assertEqual((address,length,permission),(4096,16,5))
            return code
        return publisher.zero_set_thread_area_data(observed,owner,expected,
                                                   artifact,link_map,runtime,libc,span)

    def test_exact_data_guard(self):
        self.assertEqual(self.apply(self.fixture()),4109)

    def test_foreign_artifact_map_and_runtime_refused(self):
        for field in ['artifact','link_map','runtime_archive']:
            values=copy.deepcopy(self.fixture())
            values[0][field]=dict(path='other',sha256='0'*64)
            with self.assertRaisesRegex(RuntimeError,'another actual owner'):self.apply(values)

    def test_other_zero_symbol_and_nonzero_symbol_refused(self):
        for key,value in [('name','other'),('type',1),('size',16),('value',8192),('binding',1),('visibility',0)]:
            values=copy.deepcopy(self.fixture());values[1][key]=value
            with self.assertRaisesRegex(RuntimeError,'native symbol differs'):self.apply(values)

    def test_changed_member_archive_and_extent_refused(self):
        for key,value in [('archive',{}),('member_ordinal',1224),('member_header',5904435),
                          ('member_sha256','0'*64),('input_section',2),('complete_section_bytes',15)]:
            values=copy.deepcopy(self.fixture());values[0]['set_thread_area'][key]=value
            with self.assertRaisesRegex(RuntimeError,'physical section/member'):self.apply(values)

    def test_changed_code_closed_digest_and_offset_refused(self):
        for mode in ['bytes','closed_digest','expected_digest','offset']:
            values=list(copy.deepcopy(self.fixture()))
            if mode=='bytes':values[-1]=b'x'*16
            if mode=='closed_digest':values[0]['set_thread_area']['code_sha256']='0'*64
            if mode=='expected_digest':values[2]['owner_sha256']='0'*64
            if mode=='offset':values[2]['offset']=12
            with self.assertRaisesRegex(RuntimeError,'body/digest/syscall offset'):self.apply(values)


if __name__=='__main__':unittest.main()
