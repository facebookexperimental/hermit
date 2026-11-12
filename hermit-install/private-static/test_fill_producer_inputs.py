"""Filler controls on a synthetic tree; no pipeline stage or build runs.

The code-pinned digests cannot be fabricated, so each synthetic file's real
sha256 is patched into the filler's pin constants. The golden config is then
checked with the maintained validators imported read-only from owned/.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
SCRIPT = HERE / 'fill_producer_inputs.py'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


FILLER = load('private_static_fill_producer_inputs', SCRIPT)
PIPELINE = load('private_static_owned_pipeline', HERE / 'owned/pipeline.py')
PUBLISH = load('private_static_owned_publish', HERE / 'owned/publish.py')
PYTHON = str(Path(sys.executable).resolve())


def digest(data):
    return hashlib.sha256(data).hexdigest()


class SyntheticTree:
    def __init__(self, root):
        self.root = root
        self.pins = {}

    def write(self, relative, data, executable=False):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        if executable:
            path.chmod(path.stat().st_mode | stat.S_IXUSR)
        return path

    def git(self, where, *args):
        subprocess.run(['git', '-C', str(where), *args], check=True,
                       stdout=subprocess.DEVNULL, timeout=30)

    def commit_all(self, where, message='x'):
        self.git(where, 'add', '-A')
        self.git(where, '-c', 'user.email=t@t', '-c', 'user.name=t',
                 'commit', '-qm', message)
        return subprocess.check_output(
            ['git', '-C', str(where), 'rev-parse', 'HEAD'], timeout=30).decode().strip()

    def repo(self, name):
        path = self.root / name
        path.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', '-b', 'main', str(path)],
                       check=True, stdout=subprocess.DEVNULL, timeout=30)
        return path

    def build(self):
        prefix = self.root / 'prefix'
        for name in ('cargo', 'rustc', 'rustdoc'):
            self.write(f'prefix/bin/{name}', f'private-{name}'.encode(), executable=True)
        gnu = 'prefix/lib/rustlib/x86_64-unknown-linux-gnu/bin/'
        lld = self.write(gnu + 'rust-lld', b'lld')
        objdump = self.write(gnu + 'llvm-objdump', b'objdump')
        base = 'prefix/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/'
        libc = self.write(base + 'libc.a', b'libc-bytes')
        libunwind = self.write(base + 'libunwind.a', b'unwind')
        host_objdump = self.write('host-objdump', b'gnu-objdump')
        inventory = self.write('input-inventory.json', b'{}')
        self.pins.update(
            LIBC_SHA256=digest(b'libc-bytes'), LIBC_BYTES=len(b'libc-bytes'),
            LIBUNWIND_SHA256=digest(b'unwind'), LIBUNWIND_BYTES=len(b'unwind'),
            RUST_LLD_SHA256=digest(b'lld'), LLVM_OBJDUMP_SHA256=digest(b'objdump'),
            HOST_OBJDUMP_PATH=str(host_objdump), HOST_OBJDUMP_SHA256=digest(b'gnu-objdump'),
            PRIVATE_INVENTORY_SHA256=digest(b'{}'))
        native = self.write('native-inputs.json', json.dumps(dict(
            toolchain=dict(prefix=str(prefix), input_inventory=str(inventory),
                           cargo_sha256=digest(b'private-cargo'),
                           rustc_sha256=digest(b'private-rustc'),
                           rustdoc_sha256=digest(b'private-rustdoc')),
            host_tool_names={}, host_tools_sha256={})).encode())
        inspection = self.write('inspection-tools.json', b'{"tools": {}}')
        self.pins['INSPECTION_TOOLS_SHA256'] = digest(b'{"tools": {}}')
        musl = self.write('musl.tar.gz', b'musl-archive')
        memcpy = self.write('memcpy.c', b'memcpy')
        stack_chk = self.write('stack_chk.c', b'stack-chk')
        self.pins.update(MUSL_ARCHIVE_SHA256=digest(b'musl-archive'),
                         MEMCPY_SOURCE_SHA256=digest(b'memcpy'),
                         STACK_CHK_SOURCE_SHA256=digest(b'stack-chk'))
        authority = self.root / 'authority'
        rows = []
        for relative, data in (('a.c', b'a'), ('sub/b.c', b'b'),
                               ('musl-1.2.5-selected/src/env/__libc_start_main.c', b'l'),
                               ('musl-1.2.5-selected/src/env/__init_tls.c', b't')):
            self.write('authority/' + relative, data)
            rows.append(dict(path=relative, bytes=len(data), sha256=digest(data)))
        authority_inventory = self.write(
            'authority/inventory.json', json.dumps(dict(files=rows)).encode())
        self.pins['AUTHORITY_INVENTORY_SHA256'] = hashlib.sha256(
            authority_inventory.read_bytes()).hexdigest()
        optional_dir = self.root / 'optional'
        self.write('optional/decoy.c', b'decoy')
        for label, constant in (('weak', 'OPTIONAL_STD_WEAK_SHA256'),
                                ('dtors', 'OPTIONAL_STD_DESTRUCTORS_SHA256'),
                                ('dlit', 'OPTIONAL_MUSL_DL_ITERATE_SHA256')):
            data = f'optional-{label}'.encode()
            self.write('optional/' + label + '.c', data)
            self.pins[constant] = digest(data)
        for name in ('cargo', 'rustc'):
            self.write(f'host/bin/{name}', f'host-{name}'.encode(), executable=True)
        (self.root / 'cargo-home').mkdir()
        reverie = self.repo('reverie')
        for relative in FILLER.MODULE_FILES.values():
            self.write('reverie/' + relative, ('module ' + relative).encode())
        runtime_head = self.commit_all(reverie)
        hermit = self.repo('hermit')
        self.write('hermit/hermit-install/private-static/run_pipeline.py', b'runner')
        (hermit / 'target').mkdir()
        # Stage the file and the gitlink explicitly: `git add -A` would drop
        # the cacheinfo gitlink because no submodule directory exists.
        self.git(hermit, 'add', 'hermit-install/private-static/run_pipeline.py')
        self.git(hermit, 'update-index', '--add', '--cacheinfo',
                 f'160000,{runtime_head},reverie')
        self.git(hermit, '-c', 'user.email=t@t', '-c', 'user.name=t',
                 'commit', '-qm', 'x')
        self.inputs = argparse.Namespace(
            hermit=str(hermit), runtime_reverie=str(reverie), tool_reverie=str(reverie),
            private_prefix=str(prefix), native_inputs_json=str(native),
            inspection_tools_json=str(inspection), musl_archive=str(musl),
            musl_authority=str(authority), memcpy_source=str(memcpy),
            stack_chk_source=str(stack_chk), optional_source_dir=str(optional_dir),
            cargo_home=str(self.root / 'cargo-home'),
            host_toolchain=str(self.root / 'host'),
            owner_sha256='a' * 64, legacy_roles=None,
            out=str(self.root / 'config.json'))
        self.runtime_head = runtime_head
        return self

    def fill(self):
        with mock.patch.multiple(FILLER, **self.pins):
            return FILLER.fill(self.inputs)


class FillerControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='private-static-fill-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.environment = dict(os.environ, PYTHONDONTWRITEBYTECODE='1')

    def tree(self, name='tree'):
        return SyntheticTree(self.root / name).build()

    def test_golden_fill_passes_maintained_validators(self):
        tree = self.tree()
        config = tree.fill()
        PIPELINE.validate_config(config)
        PUBLISH.validate_roles(config['data_roles'])
        self.assertEqual(config['schema'], 'hermit.private-static-producer-inputs.v1')
        self.assertEqual(config['heads']['runtime_R'], tree.runtime_head)
        self.assertEqual(len(config['paths']), 19)
        self.assertEqual(len(config['digests']), 11)
        self.assertEqual(config['paths']['target'], str(self.root / 'tree/hermit/target'))
        self.assertTrue(config['paths']['cargo'].endswith('prefix/bin/cargo'))

    def test_pin_mismatches_refuse_with_pin_named(self):
        cases = (
            ('musl.tar.gz', 'musl source archive'),
            ('memcpy.c', 'memcpy source'),
            ('stack_chk.c', 'stack_chk source'),
            ('inspection-tools.json', 'inspection-tools manifest'),
            ('input-inventory.json', 'private toolchain inventory'),
            ('authority/inventory.json', 'musl source authority inventory.json'),
            ('prefix/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/libc.a',
             'libc.a'),
            ('optional/weak.c', 'optional_std_weak_source'),
        )
        for relative, label in cases:
            with self.subTest(relative=relative):
                tree = self.tree('case-' + relative.replace('/', '-'))
                target = tree.root / relative
                target.write_bytes(target.read_bytes() + b'x')
                with self.assertRaises(RuntimeError) as caught:
                    tree.fill()
                self.assertIn(label, str(caught.exception))

    def test_gitlink_mismatch_refuses(self):
        tree = self.tree()
        tree.write('reverie/extra.txt', b'extra')
        tree.commit_all(tree.root / 'reverie', 'move runtime head')
        with self.assertRaises(RuntimeError) as caught:
            tree.fill()
        self.assertIn('gitlink', str(caught.exception))

    def test_symlinked_path_refuses(self):
        tree = self.tree()
        link = self.root / 'hermit-link'
        link.symlink_to(tree.root / 'hermit', target_is_directory=True)
        tree.inputs.hermit = str(link)
        with self.assertRaises(RuntimeError) as caught:
            tree.fill()
        self.assertIn('canonical', str(caught.exception))

    def test_legacy_roles_drop_package_roles_module(self):
        tree = self.tree()
        owner = dict(symbol='s', offset=1, owner_sha256='b' * 64)
        roles = dict(fallback_entry=owner, completion_syscall=owner, sites=[
            dict(operation=op, number=num, owner=owner)
            for op, num in ((1, 158), (2, 218), (3, 7), (4, 2))])
        path = tree.write('legacy-roles.json', json.dumps(roles).encode())
        tree.inputs.legacy_roles = str(path)
        tree.inputs.owner_sha256 = None
        config = tree.fill()
        PIPELINE.validate_config(config)
        PUBLISH.validate_roles(config['data_roles'])
        self.assertNotIn('package_roles_module', config['digests'])
        self.assertEqual(len(config['digests']), 10)

    def test_legacy_roles_refuse_unbounded_symbols(self):
        for symbol in ('bad\nsymbol', 'bad\rsymbol', 'bad\0symbol', 'x' * 16385):
            with self.subTest(symbol=repr(symbol[:12])):
                tree = self.tree('legacy-' + str(len(symbol)) + '-' + str(abs(hash(symbol)) % 9999))
                owner = dict(symbol=symbol, offset=1, owner_sha256='b' * 64)
                roles = dict(fallback_entry=owner, completion_syscall=owner, sites=[
                    dict(operation=op, number=num, owner=owner)
                    for op, num in ((1, 158), (2, 218), (3, 7), (4, 2))])
                path = tree.write('legacy-roles.json', json.dumps(roles).encode())
                tree.inputs.legacy_roles = str(path)
                tree.inputs.owner_sha256 = None
                with self.assertRaises(RuntimeError) as caught:
                    tree.fill()
                self.assertIn('owner', str(caught.exception))

    def test_help_and_roles_group(self):
        result = subprocess.run([PYTHON, '-B', str(SCRIPT), '--help'],
                                env=self.environment, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(b'--owner-sha256', result.stdout)
        self.assertIn(b'--legacy-roles', result.stdout)
        result = subprocess.run([PYTHON, '-B', str(SCRIPT)],
                                env=self.environment, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 2)


if __name__ == '__main__':
    unittest.main()
