"""Every hash literal in the producer that names a file in this tree is that
file's current sha256.

The producer pins several of its own helpers by sha256 and checks them only on
the pipeline's own path, which no other control reaches. 865c3e0e edited
archive/prepare.py and left section/prepare.py's MAINTAINED_SHA at the old
hash, so every pipeline run refused at the section stage while these controls
stayed green.

Pins are found by scanning, never from a list: every 64-hex literal in a
producer file (controls excluded) must be one of
- an in-tree pin, checked against the file it names:
  - a `<STEM>_SHA` or `<STEM>_SHA256` constant whose module defines the path `<STEM>`;
  - a filename-to-digest dict that a function reads beside `<BASE> / name`;
  - an inline `sha(<BASE> / 'name') == '<hex>'`, or a loop over a
    filename-to-digest dict that checks `sha(<BASE> / name)`;
- a source-files.json row, checked against its file;
- a REVERIE_PINS literal: Reverie content that Hermit's own reverie gitlink
  fixes, checked against the blob at that gitlink;
- in runtime_cohort.py only, a row of a table registered in its COHORTS: a
  historical cohort fixture unless it names a file in this tree (then
  checked). Matched by value from the loaded module, so a row-shaped tuple
  anywhere else, or one in runtime_cohort.py outside COHORTS, is unclassified;
- an EXCLUDED input outside both trees, named with its reason.
Every file under this tree must have a scanned suffix, and the census also
reads joined Python string constants and uppercase hex, so a pin cannot hide
in an unscanned file, a split literal or another case.
"""
import ast
import hashlib
import importlib.util
import json
import re
import subprocess
import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent
ROOT = D.parent
HERMIT = ROOT.parent.parent
HEX = re.compile('[0-9a-f]{64}')
ANY_HEX = re.compile('[0-9a-fA-F]{64}')
CONSTANT = re.compile('([A-Z][A-Z0-9_]*?)_SHA(?:256)?')
TEXT = ('.py', '.json', '.md', '.rs')

# Hash literals naming inputs outside this tree: file -> {digest: name and reason}.
EXCLUDED = {
    'README.md': {
        '3c87bfa780d378b9e8313c904ae6fa91ae3e2e57de12275b340122a41c8cccbb': 'PID_HELPER_SHA256: pid-bound-v2.py, supplied by the caller',
    },
    'run_pipeline.py': {
        '3c87bfa780d378b9e8313c904ae6fa91ae3e2e57de12275b340122a41c8cccbb': 'PID_HELPER_SHA256: pid-bound-v2.py, supplied by the caller',
    },
    'fill_producer_inputs.py': {
        'ecabca6176a177d2c1426caa9e2508fad967c4e4792a7c2bad9c513d26d99866': 'PRIVATE_INVENTORY_SHA256: private toolchain inventory',
        '3cb125f0204fb78605be0493a2941a118624b903aa819eede5ab911afe88503e': 'INSPECTION_TOOLS_SHA256: inspection tool inventory',
        'a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4': 'MUSL_ARCHIVE_SHA256: musl 1.2.5 source archive',
        '3beab05d07515e2005d1ee59b66327a4c5de385be4ba689f180d9cc470c62bf1': 'AUTHORITY_INVENTORY_SHA256: source authority inventory.json',
        '790bdd75f7736c83ed7a21f8813b20454ead1ccfc49e806a88692d2a471024eb': 'MEMCPY_SOURCE_SHA256: memcpy source',
        '2ea4d8cf6758daddecf8973913ac196ffa76fdb355cea391ca890aeafdc2ac6c': 'STACK_CHK_SOURCE_SHA256: __stack_chk_fail source',
        '0f9c4e63d9f15b2486e8c0a9af89c4c2ad88fcad7c1a5127a95d8031e13ec2d3': 'OPTIONAL_STD_WEAK_SHA256: optional std weak-syscall source',
        '5634b13e8faa9ead0e7dac104abf51a990bc14cbc39c5c6bae22a7b70811e2b3': 'OPTIONAL_STD_DESTRUCTORS_SHA256: optional std destructors source',
        '802a537924c900ff607b65bda465bf66899890b7b0707c0cd55c7b800db20327': 'OPTIONAL_MUSL_DL_ITERATE_SHA256: optional musl dl_iterate_phdr source',
        'e699c64b0c6b427d89ad5bf767bf78beb30c19930c5dedff635951636e8f9f70': 'LIBC_SHA256: private self-contained libc.a',
        'eb16569e611a5d6cdda2e7604fd6402a3ad810817dd8d63ed50baa2ce8070aaa': 'LIBUNWIND_SHA256: private self-contained libunwind.a',
        'eb9b9bc681e57f7ff4166f8fb57fad7336d29a51c85e8c70ba1ca203600e392b': 'RUST_LLD_SHA256: private toolchain binary',
        '8f906de30ee713c953d1762f78901b3a5d4ccd99f89c65627b712618aeeddcf4': 'LLVM_OBJDUMP_SHA256: private toolchain binary',
        '0a1b558f444ed2bca7fb467dede7416a651bc490b1abc77705ab470d99c527c1': 'HOST_OBJDUMP_SHA256: host GNU objdump 2.35.2',
    },
    'producer-inputs.example.json': {
        '3cb125f0204fb78605be0493a2941a118624b903aa819eede5ab911afe88503e': 'inspection_tools: inspection tool inventory',
        'a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4': 'musl_source_archive: musl 1.2.5 source archive',
        '790bdd75f7736c83ed7a21f8813b20454ead1ccfc49e806a88692d2a471024eb': 'memcpy_source: memcpy source',
        '3beab05d07515e2005d1ee59b66327a4c5de385be4ba689f180d9cc470c62bf1': 'musl_source_authority: source authority inventory.json',
        '2ea4d8cf6758daddecf8973913ac196ffa76fdb355cea391ca890aeafdc2ac6c': 'stack_chk_source: __stack_chk_fail source',
        '0f9c4e63d9f15b2486e8c0a9af89c4c2ad88fcad7c1a5127a95d8031e13ec2d3': 'optional_std_weak_source: optional std weak-syscall source',
        '5634b13e8faa9ead0e7dac104abf51a990bc14cbc39c5c6bae22a7b70811e2b3': 'optional_std_destructors_source: optional std destructors source',
        '802a537924c900ff607b65bda465bf66899890b7b0707c0cd55c7b800db20327': 'optional_musl_dl_iterate_source: optional musl dl_iterate_phdr source',
    },
    'owned/archive/prepare.py': {
        'ecabca6176a177d2c1426caa9e2508fad967c4e4792a7c2bad9c513d26d99866': 'PRIVATE_INVENTORY_SHA256: private toolchain inventory',
    },
    'owned/prepare.py': {
        'ecabca6176a177d2c1426caa9e2508fad967c4e4792a7c2bad9c513d26d99866': 'PRIVATE_INVENTORY_SHA256: private toolchain inventory',
        '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b': 'ORDINARY_LEGACY_TRAP_SOURCE_SHA256: historical Reverie trap.rs',
        'a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4': 'musl_source_archive: musl 1.2.5 source archive',
        '0f9c4e63d9f15b2486e8c0a9af89c4c2ad88fcad7c1a5127a95d8031e13ec2d3': 'std_weak: optional std weak-syscall source',
        '5634b13e8faa9ead0e7dac104abf51a990bc14cbc39c5c6bae22a7b70811e2b3': 'std_destructors: optional std destructors source',
        '802a537924c900ff607b65bda465bf66899890b7b0707c0cd55c7b800db20327': 'musl_dl_iterate: optional musl dl_iterate_phdr source',
        '3cb125f0204fb78605be0493a2941a118624b903aa819eede5ab911afe88503e': 'inspection_tools: inspection tool inventory',
        '8f906de30ee713c953d1762f78901b3a5d4ccd99f89c65627b712618aeeddcf4': 'llvm-objdump: private toolchain binary',
        '0a1b558f444ed2bca7fb467dede7416a651bc490b1abc77705ab470d99c527c1': 'census objdump: host GNU objdump 2.35.2',
    },
    'owned/publish.py': {
        '0f842b24b5425118e12f36fd97e7fd7e91b97728e49a7b52c26a84192754416c': 'member_sha256: libc.a member body in the private toolchain',
    },
    'owned/runtime_cohort.py': {
        '095374039289c0c2063acdc8eabbd7bb1cbaf6a07e44d50db32ee9ab796fd4e2': 'TRUSTED_SYSCALL_X86_64_BLOCK_SHA256: Reverie trap.rs gate block, checked by test_runtime_cohort',
    },
    'owned/section/prepare.py': {
        '3cb125f0204fb78605be0493a2941a118624b903aa819eede5ab911afe88503e': 'inspection_tools: inspection tool inventory',
        'eb9b9bc681e57f7ff4166f8fb57fad7336d29a51c85e8c70ba1ca203600e392b': 'rust-lld: private toolchain binary',
        'e699c64b0c6b427d89ad5bf767bf78beb30c19930c5dedff635951636e8f9f70': 'libc: private self-contained libc.a',
        'eb16569e611a5d6cdda2e7604fd6402a3ad810817dd8d63ed50baa2ce8070aaa': 'libunwind: private self-contained libunwind.a',
        '790bdd75f7736c83ed7a21f8813b20454ead1ccfc49e806a88692d2a471024eb': 'memcpy_source: memcpy source',
        '2ea4d8cf6758daddecf8973913ac196ffa76fdb355cea391ca890aeafdc2ac6c': 'stack_chk_source: __stack_chk_fail source',
        '3beab05d07515e2005d1ee59b66327a4c5de385be4ba689f180d9cc470c62bf1': 'musl_source_authority: source authority inventory.json',
    },
}

# Live pins on Reverie content that Hermit's reverie gitlink fixes: file ->
# {digest: Reverie path}. Each is compared with the blob at the gitlink, so a
# Reverie bump that changes the file turns this test red at the bump.
REVERIE_PINS = {
    'owned/prepare.py': {
        # The ordinary provider's fixed-gates source role (prepare.py's
        # "ordinary source role differs from actual factory source").
        'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54': 'reverie-inguest/src/trap/fixed_gates.rs',
        # The native leaf sources, read on every run from the tool Reverie
        # checkout c['R']; the gitlink's copy is the reviewed one.
        'aa843bcfb5b75e232df8d26dab6e5cec4f7123b33bebe655ce957f95f839f2e1': 'reverie-liteinst-artifact/tests/fixtures/musl_memset_source.txt',
        '9a6299228be5f8f467652263e8f98c1f9ab2e0937243589b8c5642f0ebe3236d': 'reverie-liteinst-artifact/tests/fixtures/musl_set_thread_area_source.txt',
    },
}


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def load(path):
    spec = importlib.util.spec_from_file_location('file_pins_' + re.sub(r'\W', '_', str(path.relative_to(ROOT))), path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tree_files():
    for path in sorted(ROOT.rglob('*')):
        if path.is_file() and '__pycache__' not in path.parts:
            yield path


def producer_files():
    for path in tree_files():
        if path.suffix in TEXT and not path.name.startswith('test_'):
            yield path


def reverie_blob(path):
    """The bytes of Reverie PATH at the commit Hermit's HEAD records for reverie/."""
    def git(where, *args):
        return subprocess.run(['git', '-C', str(where), *args], check=True, capture_output=True, timeout=60).stdout
    entry = git(HERMIT, 'ls-tree', '--full-tree', 'HEAD', 'reverie').decode().split('\t')[0].split()
    assert entry[:2] == ['160000', 'commit'], entry
    return git(HERMIT / 'reverie', 'cat-file', 'blob', f'{entry[2]}:{path}')


def hex_constant(node):
    return isinstance(node, ast.Constant) and isinstance(node.value, str) and HEX.fullmatch(node.value)


def module_assignments(tree):
    names = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            names[node.targets[0].id] = node.value
    return names


def base_div(node):
    """BASE for `BASE / <anything>`."""
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Div) and isinstance(node.left, ast.Name):
        return node.left.id
    return None


def python_pins(path):
    """(label, file named, digest) for every in-tree pin form in one Python module."""
    tree = ast.parse(path.read_text())
    assigned = module_assignments(tree)
    module = None

    def attribute(name):
        nonlocal module
        module = module or load(path)
        return getattr(module, name)

    pins = []
    # <STEM>_SHA / <STEM>_SHA256 constants whose module defines the path <STEM>.
    for name, value in assigned.items():
        match = CONSTANT.fullmatch(name)
        if match and hex_constant(value) and match[1] in assigned:
            pins.append((name, Path(attribute(match[1])), value.value))
    # Filename-to-digest dict constants, resolved by the `BASE / name` read beside them.
    for name, value in assigned.items():
        if isinstance(value, ast.Dict) and value.values and all(hex_constant(v) for v in value.values):
            bases = {base_div(n) for f in ast.walk(tree) if isinstance(f, ast.FunctionDef)
                     if any(isinstance(s, ast.Subscript) and getattr(s.value, 'id', None) == name for s in ast.walk(f))
                     for n in ast.walk(f)} - {None}
            if len(bases) != 1:
                raise AssertionError(f'{path}: digest table {name} has no single BASE / name read: {bases}')
            base = attribute(bases.pop())
            pins += [(f'{name}[{k.value}]', Path(base) / k.value, v.value) for k, v in zip(value.keys, value.values)]
    for node in ast.walk(tree):
        # sha(BASE / 'name') == '<hex>'
        if (isinstance(node, ast.Compare) and len(node.ops) == 1 and isinstance(node.ops[0], ast.Eq)
                and isinstance(node.left, ast.Call) and getattr(node.left.func, 'id', None) == 'sha'
                and len(node.left.args) == 1 and base_div(node.left.args[0])
                and isinstance(node.left.args[0].right, ast.Constant) and hex_constant(node.comparators[0])):
            file = Path(attribute(base_div(node.left.args[0]))) / node.left.args[0].right.value
            pins.append((f'inline {file.name}', file, node.comparators[0].value))
        # for name, digest in {'name': '<hex>', ...}.items(): ... sha(BASE / name) ...
        if (isinstance(node, ast.For) and isinstance(node.iter, ast.Call)
                and isinstance(node.iter.func, ast.Attribute) and node.iter.func.attr == 'items'
                and isinstance(node.iter.func.value, ast.Dict)
                and all(hex_constant(v) for v in node.iter.func.value.values)):
            bases = {base_div(c.args[0]) for b in node.body for c in ast.walk(b)
                     if isinstance(c, ast.Call) and getattr(c.func, 'id', None) == 'sha' and len(c.args) == 1} - {None}
            if len(bases) != 1:
                raise AssertionError(f'{path}:{node.lineno}: digest loop has no single sha(BASE / name): {bases}')
            base = attribute(bases.pop())
            table = node.iter.func.value
            pins += [(f'inline table {k.value}', Path(base) / k.value, v.value) for k, v in zip(table.keys, table.values)]
    return pins


def cohort_rows(path):
    """(cohort, label, path, bytes, digest) for every row of every table that
    runtime_cohort.py registers in COHORTS, by value: a computed table counts,
    a row-shaped tuple outside COHORTS does not."""
    rows = []
    for cohort, table in load(path).COHORTS:
        for label, row_path, size, digest in table:
            if label not in ('H', 'R'):
                raise AssertionError(f'{path}: {cohort} row {row_path} has label {label!r}, not H or R')
            rows.append((cohort, label, row_path, size, digest))
    return rows


def scan():
    """{relative file: {digest: classification}} plus the checked pins."""
    classified, checked = {}, []
    for path in producer_files():
        relative = path.relative_to(ROOT).as_posix()
        mine = classified.setdefault(relative, {})
        if path.suffix == '.py':
            for label, file, digest in python_pins(path):
                checked.append((f'{relative} {label}', file, digest))
                mine[digest] = 'in-tree pin'
        if relative == 'owned/runtime_cohort.py':
            for cohort, label, row_path, _size, digest in cohort_rows(path):
                file = (HERMIT if label == 'H' else HERMIT / 'reverie') / row_path
                if file.resolve().is_relative_to(ROOT):
                    checked.append((f'{relative} {cohort} {row_path}', file, digest))
                    mine[digest] = 'in-tree pin'
                else:
                    mine.setdefault(digest, f'{cohort} {label} row: historical runtime cohort fixture')
        if path.name == 'source-files.json':
            for row in json.loads(path.read_text()):
                checked.append((f'{relative} {row["path"]}', path.parent / row['path'], row['sha256']))
                mine[row['sha256']] = 'in-tree pin'
        for digest, reverie_path in REVERIE_PINS.get(relative, {}).items():
            mine.setdefault(digest, 'reverie gitlink pin: ' + reverie_path)
        for digest, why in EXCLUDED.get(relative, {}).items():
            mine.setdefault(digest, 'excluded: ' + why)
    return classified, checked


def census(path):
    """Every 64-hex token in PATH, in any case, plus every joined Python string
    constant that is one (adjacent literals are joined by the parser)."""
    text = path.read_text()
    found = set(ANY_HEX.findall(text))
    if path.suffix == '.py':
        found |= {node.value for node in ast.walk(ast.parse(text))
                  if isinstance(node, ast.Constant) and isinstance(node.value, str) and ANY_HEX.fullmatch(node.value)}
    return found


class InTreeFilePins(unittest.TestCase):
    def test_every_hash_literal_is_classified(self):
        classified, _ = scan()
        for path in producer_files():
            relative = path.relative_to(ROOT).as_posix()
            found = census(path)
            with self.subTest(file=relative):
                self.assertEqual(set(), found - set(classified[relative]),
                                 'each hash literal must be an in-tree pin the scan checks, a Reverie gitlink pin, '
                                 'a COHORTS fixture row, or a named EXCLUDED input')
                stale = (set(EXCLUDED.get(relative, {})) | set(REVERIE_PINS.get(relative, {}))) - found
                self.assertEqual(set(), stale, 'EXCLUDED or REVERIE_PINS entry no longer present')

    def test_every_file_in_the_tree_is_scanned(self):
        # A pin in a file the census never reads (a .txt beside a .sh that runs
        # sha256sum -c on it) would otherwise stay green while it rots.
        for path in tree_files():
            with self.subTest(file=path.relative_to(ROOT).as_posix()):
                self.assertIn(path.suffix, TEXT, 'a file the pin scan does not read')

    def test_each_reverie_pin_is_the_blob_at_the_reverie_gitlink(self):
        for relative, pins in REVERIE_PINS.items():
            for digest, reverie_path in pins.items():
                with self.subTest(pin=f'{relative} {reverie_path}'):
                    self.assertEqual(hashlib.sha256(reverie_blob(reverie_path)).hexdigest(), digest,
                                     f'{relative} pins Reverie {reverie_path} at a sha256 the reverie gitlink does not hold')

    def test_each_in_tree_pin_names_the_current_file(self):
        _, checked = scan()
        for label, file, digest in checked:
            with self.subTest(pin=label):
                self.assertTrue(Path(file).resolve().is_relative_to(ROOT), f'{label} names {file} outside this tree')
                self.assertTrue(Path(file).is_file(), f'{label} names missing {file}')
                self.assertEqual(sha(file), digest, f'{label} pins {file} at a stale sha256')
        # The code pins this producer has today, exactly: the scan must keep
        # finding each, and a new one is added here on purpose. Every
        # source-files.json row is checked above, one per listed file.
        code_pins = {label for label, _, _ in checked if 'source-files.json' not in label}
        self.assertEqual({
            'owned/assets/final_inspection.py HELPERS[elf_metadata.py]',
            'owned/prepare.py RUNTIME_COHORT_SHA256',
            'owned/prepare.py LEGACY_SHA',
            'owned/prepare.py inline table elf_metadata.py',
            'owned/prepare.py inline table archive_metadata.py',
            'owned/prepare.py inline table source_guard.py',
            'owned/prepare.py inline table native_guard.py',
            'owned/prepare.py inline factory_controls.rs',
            'owned/section/prepare.py MAINTAINED_SHA',
            'owned/section/prepare.py HELPERS[archive_metadata.py]',
        }, code_pins)
        self.assertIn('owned/source-files.json section/prepare.py', {label for label, _, _ in checked})

    def test_the_section_stage_loads_its_reviewed_archive_helper(self):
        # The section stage's own check, as the pipeline runs it.
        maintained = load(D / 'section/prepare.py').maintained_module()
        self.assertTrue(callable(maintained.main))


if __name__ == '__main__':
    unittest.main()
