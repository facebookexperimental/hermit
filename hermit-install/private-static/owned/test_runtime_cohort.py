"""Data controls only; no source/build/child/ELF qualification claims."""
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
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


# Real Git fixtures for the derived cohort. Global and system configuration are
# excluded so an operator's signing or hook settings cannot reach a fixture.
LIVE = D.parents[2]
TRAP = 'reverie-inguest/src/trap.rs'
ORDINARY_GATE = 'reverie-liteinst-artifact/src/private_static/selected_initializers/ordinary_gate.rs'
GIT_FIXTURE_ENV = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1',
                       GIT_AUTHOR_NAME='fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                       GIT_COMMITTER_NAME='fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid',
                       GIT_AUTHOR_DATE='2026-01-01T00:00:00Z', GIT_COMMITTER_DATE='2026-01-01T00:00:00Z')


def fixture_git_bytes(path, *args):
    return subprocess.run(['git', '-C', str(path), '-c', 'commit.gpgsign=false', '-c', 'init.defaultBranch=main',
                           '-c', 'core.hooksPath=' + os.devnull, *args],
                          check=True, capture_output=True, env=GIT_FIXTURE_ENV, timeout=60).stdout


def fixture_git(path, *args):
    return fixture_git_bytes(path, *args).decode().strip()


def live_gitlink():
    header = fixture_git(LIVE, 'ls-tree', '--full-tree', 'HEAD', 'reverie').split('\t')[0].split()
    assert header[:2] == ['160000', 'commit'], header
    return header[2]


def live_reverie_object(path):
    return fixture_git_bytes(LIVE / 'reverie', 'cat-file', 'blob', live_gitlink() + ':' + path)


def lock_text(pin):
    return ('[[package]]\nname = "reverie"\nversion = "0.1.0"\n'
            'source = "git+https://github.com/rrnewton/reverie.git?rev={0}#{0}"\n'.format(pin))


class PinnedTree:
    """A Hermit checkout with a Reverie submodule, built from real commits."""

    def __init__(self, root):
        self.root = root
        self.reverie = root / 'reverie'
        self.reverie.mkdir()
        fixture_git(self.reverie, 'init', '-q')
        for label, path in cohort.DERIVED_ROWS:
            if label == 'R':
                self.write(self.reverie / path, live_reverie_object(TRAP) if path == TRAP
                           else ('runtime ' + path + '\n').encode())
        self.write(self.reverie / 'README.md', b'not a reviewed row\n')
        self.rpin = self.commit(self.reverie)
        fixture_git(root, 'init', '-q')
        for label, path in cohort.DERIVED_ROWS:
            if label == 'H':
                self.write(root / path, ('host ' + path + '\n').encode())
        self.write(root / 'tests/unrelated/Cargo.lock', b'[[package]]\nname = "unrelated"\n')
        self.locks(self.rpin)
        self.h_head = self.commit(root)

    @staticmethod
    def write(path, raw):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)

    @staticmethod
    def commit(path):
        fixture_git(path, 'add', '-A')
        fixture_git(path, 'commit', '-q', '--allow-empty', '-m', 'fixture')
        return fixture_git(path, 'rev-parse', 'HEAD')

    def locks(self, pin, nested=None):
        self.write(self.root / 'Cargo.lock', lock_text(pin).encode())
        self.write(self.root / 'liteinst-runtime-build/Cargo.lock', lock_text(nested or pin).encode())

    def repin(self, nested=None):
        """Commit the Hermit side after a Reverie commit, pins kept current."""
        self.rpin = fixture_git(self.reverie, 'rev-parse', 'HEAD')
        self.locks(self.rpin, nested)
        self.h_head = self.commit(self.root)


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
            return cohort.select(root, cohort.COHORTS)
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
        result = cohort.select(root, cohort.COHORTS)
        self.assertEqual(result['cohort'], name)
        self.assertEqual(len(result['files']), 31)
        self.assertEqual({fact['path']: (fact['bytes'], fact['sha256'])
                          for fact in result['files']}, expected)

    def test_function_sections_fixture_keeps_all_31_exact_files(self):
        # Historical cohorts are fixtures rebuilt from their reviewed objects;
        # no fixture copies the live tree.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.historical_current_fixture(root)
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
                    cohort.select(root, cohort.COHORTS)

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
                        cohort.select(root, cohort.COHORTS)

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
                    cohort.select(root, cohort.COHORTS)


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
                    self.assertEqual(cohort.select(root, cohort.COHORTS)['cohort'], name)

    def test_live_checkout_binds_rows_derived_from_its_own_pins(self):
        # The live tree is admitted by the rows its own H_head and Reverie
        # gitlink objects declare, never by a literal tuple of any vintage.
        head = fixture_git(LIVE, 'rev-parse', 'HEAD')
        result = cohort.bind(LIVE, head)
        self.assertEqual(result['cohort'], cohort.DERIVED_COHORT)
        self.assertEqual((result['h_head'], result['runtime_R_pin']), (head, live_gitlink()))
        self.assertEqual(len(result['files']), len(cohort.DERIVED_ROWS))

    def test_mixed_old_and_neutral_support_rows_are_refused(self):
        root = Path('/data-only-cohort')
        facts = self.facts(cohort.CURRENT_SPLIT_RUNTIME, root)
        old = self.facts(cohort.FINAL_DEVELOPMENT_054, root)
        path = str(root / 'detcore-liteinst/build.rs')
        with patch.object(cohort, 'observed_file', side_effect=lambda path: facts[str(path)]):
            self.assertEqual(cohort.select(root, cohort.COHORTS)['cohort'], 'current-split-runtime-content-v1')
            facts[path] = old[path]
            with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                cohort.select(root, cohort.COHORTS)

    def test_each_changed_source_refuses_same_size_mutation_and_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ContentCohort().historical_fixture(root, 'current-split-runtime-content-v1')
            for relative in ['detcore-liteinst/build.rs', 'detcore-liteinst/src/lib.rs']:
                path = root / relative
                original = path.read_bytes()
                for changed in [bytes([original[0] ^ 1]) + original[1:], original[:-1]]:
                    with self.subTest(path=relative, size=len(changed)):
                        self.assertEqual(cohort.select(root, cohort.COHORTS)['cohort'], 'current-split-runtime-content-v1')
                        path.write_bytes(changed)
                        try:
                            with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort'):
                                cohort.select(root, cohort.COHORTS)
                        finally:
                            path.write_bytes(original)


class DerivedCohort(unittest.TestCase):
    """Rows come from H_head and its Reverie gitlink objects, never the tree."""

    def test_consumer_tests_row_is_dropped_and_the_other_30_rows_remain(self):
        consumer = ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs')
        self.assertNotIn(consumer, cohort.DERIVED_ROWS)
        self.assertEqual(len(cohort.DERIVED_ROWS), 30)
        self.assertEqual(set(cohort.DERIVED_ROWS) | {consumer},
                         {(label, path) for label, path, _, _ in cohort.CURRENT_SPLIT_RUNTIME})

    def test_clean_tree_binds_rows_read_from_objects(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            result = cohort.bind(tree.root, tree.h_head)
            self.assertEqual(result['cohort'], cohort.DERIVED_COHORT)
            self.assertEqual((result['h_head'], result['runtime_R_pin']), (tree.h_head, tree.rpin))
            self.assertEqual(len(result['files']), 30)
            for label, path, size, digest in result['rows']:
                owner, rev = (tree.root, tree.h_head) if label == 'H' else (tree.reverie, tree.rpin)
                raw = fixture_git_bytes(owner, 'cat-file', 'blob', rev + ':' + path)
                self.assertEqual((size, digest), (len(raw), hashlib.sha256(raw).hexdigest()))
            self.assertEqual(result['gate']['x86_64'], cohort.TRUSTED_SYSCALL_X86_64_BLOCK_SHA256)
            self.assertEqual(cohort.bind(tree.root, tree.h_head, tree.rpin)['runtime_R_pin'], tree.rpin)

    def test_derivation_never_reads_the_working_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            path = 'detcore-liteinst/build.rs'
            committed = cohort.derive(tree.root, tree.h_head)['rows']
            (tree.root / path).write_bytes(b'uncommitted edit\n')
            self.assertEqual(cohort.derive(tree.root, tree.h_head)['rows'], committed)
            with patch.object(cohort, 'observed_file', side_effect=AssertionError('working tree read')):
                cohort.derive(tree.root, tree.h_head)

    def test_working_tree_edit_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            path = 'detcore-liteinst/build.rs'
            (tree.root / path).write_bytes(b'uncommitted edit\n')
            with self.assertRaisesRegex(RuntimeError, 'one complete reviewed content cohort.*H ' + path +
                                        ' observed 17:' + hashlib.sha256(b'uncommitted edit\n').hexdigest()):
                cohort.bind(tree.root, tree.h_head)

    def test_missing_objects_are_refused_by_name_without_fallback(self):
        for label, path in [('R', 'reverie-loader/src/lib.rs'), ('H', 'detcore-liteinst/src/lib.rs')]:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                tree = PinnedTree(Path(directory))
                self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
                owner = tree.reverie if label == 'R' else tree.root
                fixture_git(owner, 'rm', '-q', '--cached', path)
                if label == 'R':
                    fixture_git(owner, 'commit', '-q', '-m', 'drop row')
                    tree.repin()
                    rev = tree.rpin
                else:
                    fixture_git(owner, 'commit', '-q', '-m', 'drop row')
                    rev = tree.h_head = fixture_git(owner, 'rev-parse', 'HEAD')
                # The file is still present in the working tree; it is never consulted.
                self.assertTrue((owner / path).is_file())
                with patch.object(cohort, 'observed_file', side_effect=AssertionError('working tree read')):
                    with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: missing object ' +
                                                label + ' ' + rev + ':' + path):
                        cohort.bind(tree.root, tree.h_head)

    def test_lock_pins_that_disagree_with_the_gitlink_are_refused_by_name(self):
        other = 'f' * 40
        for lock in ['Cargo.lock', 'liteinst-runtime-build/Cargo.lock']:
            with self.subTest(lock=lock), tempfile.TemporaryDirectory() as directory:
                tree = PinnedTree(Path(directory))
                self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
                text = (tree.root / lock).read_text()
                (tree.root / lock).write_text(text.replace('#' + tree.rpin, '#' + other))
                tree.h_head = tree.commit(tree.root)
                with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: ' + lock +
                                            ':4 reverie pin ' + other + ' differs from the H_head gitlink ' + tree.rpin):
                    cohort.bind(tree.root, tree.h_head)

    def test_unpinned_or_absent_root_lock_reference_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            (tree.root / 'Cargo.lock').write_text(
                '[[package]]\nname = "reverie"\nsource = "git+https://github.com/rrnewton/reverie.git?branch=main"\n')
            tree.h_head = tree.commit(tree.root)
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: Cargo.lock:3 names reverie without a 40-hex pin'):
                cohort.bind(tree.root, tree.h_head)
            (tree.root / 'Cargo.lock').write_text('[[package]]\nname = "other"\n')
            tree.h_head = tree.commit(tree.root)
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: root Cargo.lock carries no reverie pin'):
                cohort.bind(tree.root, tree.h_head)

    def test_absent_gitlink_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            fixture_git(tree.root, 'rm', '-q', '--cached', 'reverie')
            fixture_git(tree.root, 'commit', '-q', '-m', 'drop gitlink')
            head = fixture_git(tree.root, 'rev-parse', 'HEAD')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: H_head ' + head + ' has no reverie gitlink'):
                cohort.bind(tree.root, head)

    def test_dirty_or_moved_submodule_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            readme = tree.reverie / 'README.md'
            readme.write_bytes(b'dirty\n')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: reverie submodule is dirty: .*README.md'):
                cohort.bind(tree.root, tree.h_head)
            fixture_git(tree.reverie, 'add', 'README.md')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: reverie submodule is dirty: .*README.md'):
                cohort.bind(tree.root, tree.h_head)
            fixture_git(tree.reverie, 'commit', '-q', '-m', 'moved')
            moved = fixture_git(tree.reverie, 'rev-parse', 'HEAD')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: reverie submodule HEAD ' + moved +
                                        ' is not the H_head gitlink ' + tree.rpin):
                cohort.bind(tree.root, tree.h_head)

    def test_wrong_heads_are_refused_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: H_head must be 40 lowercase hex'):
                cohort.bind(tree.root, 'HEAD')
            fixture_git(tree.root, 'commit', '-q', '--allow-empty', '-m', 'later')
            later = fixture_git(tree.root, 'rev-parse', 'HEAD')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: H checkout HEAD ' + later +
                                        ' is not H_head ' + tree.h_head):
                cohort.bind(tree.root, tree.h_head)
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: declared runtime_R_pin ' + '0' * 40 +
                                        ' differs from the H_head gitlink ' + tree.rpin):
                cohort.bind(tree.root, later, '0' * 40)

    def test_prepare_preflight_carries_the_declared_runtime_pin(self):
        prepare = load('prepare')
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            c = {'H': str(tree.root), 'H_head': tree.h_head, 'runtime_R_pin': tree.rpin}
            self.assertEqual(prepare.runtime_content_preflight(c)['runtime_R_pin'], tree.rpin)
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: declared runtime_R_pin ' + '0' * 40 +
                                        ' differs from the H_head gitlink ' + tree.rpin):
                prepare.runtime_content_preflight(dict(c, runtime_R_pin='0' * 40))

    def test_git_failure_never_falls_back_to_the_working_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            with patch.object(cohort, 'observed_file', side_effect=AssertionError('working tree read')), \
                 patch.dict(os.environ, {'PATH': str(Path(directory) / 'no-git-here')}):
                with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: git'):
                    cohort.bind(tree.root, tree.h_head)

    def test_gate_shape_is_checked_on_the_derived_trap_object(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            self.assertEqual(cohort.bind(tree.root, tree.h_head)['cohort'], cohort.DERIVED_COHORT)
            trap = tree.reverie / TRAP
            raw = trap.read_bytes()
            x86 = next(b for b in cohort.gate_shape(raw)['blocks'] if b['arch'] == 'x86_64' and b['contributes'])
            cut = raw.index(b'\n', raw.index(b'\n', x86['start']) + 1) + 1
            trap.write_bytes(raw[:cut] + b'    // edit inside the x86_64 gate block\n' + raw[cut:])
            fixture_git(tree.reverie, 'commit', '-q', '-am', 'gate edit')
            tree.repin()
            with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: x86_64 block sha256 [0-9a-f]{64} '
                                        'differs from reviewed ' + cohort.TRUSTED_SYSCALL_X86_64_BLOCK_SHA256):
                cohort.bind(tree.root, tree.h_head)


class TrustedSyscallGateShape(unittest.TestCase):
    """Per-arch binding of the global_asm! blocks that fill the gate section."""

    @classmethod
    def setUpClass(cls):
        cls.base = live_reverie_object(TRAP)

    def block(self, raw, arch, contributes=True):
        blocks = [b for b in cohort.gate_shape(raw)['blocks']
                  if b['arch'] == arch and b['contributes'] == contributes]
        self.assertEqual(len(blocks), 1)
        return blocks[0]

    def edit_inside(self, raw, block, text):
        """Insert an assembler comment just after the block's first line."""
        cut = raw.index(b'\n', block['start']) + 1
        cut = raw.index(b'\n', cut) + 1
        self.assertLess(cut, block['end'])
        return raw[:cut] + text + raw[cut:]

    def test_live_trap_source_has_the_reviewed_x86_64_gate(self):
        shape = cohort.gate_guard(self.base)
        self.assertEqual(shape['x86_64'], cohort.TRUSTED_SYSCALL_X86_64_BLOCK_SHA256)
        self.assertRegex(shape['aarch64'], '^[0-9a-f]{64}$')
        contributing = [b for b in shape['blocks'] if b['contributes']]
        self.assertEqual(sorted(b['arch'] for b in contributing), ['aarch64', 'x86_64'])
        self.assertTrue(all(b['defines_entry'] for b in contributing))
        self.assertFalse(any(b['defines_entry'] for b in shape['blocks'] if not b['contributes']))

    def test_aarch64_only_edit_does_not_trip_the_guard(self):
        changed = self.edit_inside(self.base, self.block(self.base, 'aarch64'), b'    // aarch64-only edit\n')
        before, after = cohort.gate_guard(self.base), cohort.gate_guard(changed)
        self.assertEqual(after['x86_64'], before['x86_64'])
        self.assertNotEqual(after['aarch64'], before['aarch64'])

    def test_x86_64_gate_edit_trips_the_guard(self):
        cohort.gate_guard(self.base)
        changed = self.edit_inside(self.base, self.block(self.base, 'x86_64'), b'    // x86_64 edit\n')
        observed = cohort.gate_shape(changed)['x86_64']
        with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: x86_64 block sha256 ' + observed +
                                    ' differs from reviewed ' + cohort.TRUSTED_SYSCALL_X86_64_BLOCK_SHA256):
            cohort.gate_guard(changed)

    third = (b'\n#[cfg(target_arch = "%s")]\ncore::arch::global_asm!(\n    r#"\n'
             b'    .pushsection .text.reverie_inguest_trusted_syscall,"ax",@progbits\n'
             b'    nop\n    .popsection\n"#\n);\n')

    def test_a_third_contributing_block_is_refused(self):
        for arch in [b'riscv64', b'x86_64', b'aarch64']:
            with self.subTest(arch=arch):
                cohort.gate_guard(self.base)
                with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: exactly one contributing block '
                                            'per target_arch x86_64 and aarch64 required'):
                    cohort.gate_guard(self.base + self.third % arch)

    clone = (b'\n#[cfg(target_arch = "x86_64")]\ncore::arch::global_asm!(\n    r#"\n'
             b'    %s\n    .p2align 4\n    .global reverie_inguest_thread_clone\n'
             b'reverie_inguest_thread_clone:\n    ret\n"#\n);\n')

    def test_thread_clone_outside_the_section_passes_and_moved_inside_is_refused(self):
        outside = self.base + self.clone % b'.text'
        shape = cohort.gate_guard(outside)
        clone = [b for b in shape['blocks'] if b'reverie_inguest_thread_clone' in outside[b['start']:b['end']]]
        self.assertTrue(clone and not any(b['contributes'] for b in clone))
        inside = self.base + self.clone % b'.pushsection .text.reverie_inguest_trusted_syscall,"ax",@progbits'
        with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: exactly one contributing block '
                                    'per target_arch x86_64 and aarch64 required'):
            cohort.gate_guard(inside)

    def test_section_named_outside_a_global_asm_block_is_refused(self):
        cohort.gate_guard(self.base)
        changed = self.base + b'\n#[link_section = ".text.reverie_inguest_trusted_syscall"]\nfn extra() {}\n'
        with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: section named outside a global_asm! block at line'):
            cohort.gate_guard(changed)

    def test_ungated_contributing_block_is_refused(self):
        cohort.gate_guard(self.base)
        x86 = self.block(self.base, 'x86_64')
        attribute = b'#[cfg(target_arch = "x86_64")]\n'
        self.assertEqual(self.base[x86['start'] - len(attribute):x86['start']], attribute)
        changed = self.base[:x86['start'] - len(attribute)] + self.base[x86['start']:]
        with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: contributing block at line [0-9]+ '
                                    'has no single target_arch cfg'):
            cohort.gate_guard(changed)

    def test_contributing_block_must_define_the_entry(self):
        cohort.gate_guard(self.base)
        x86 = self.block(self.base, 'x86_64')
        body = self.base[x86['start']:x86['end']]
        self.assertEqual(body.count(b'\nreverie_inguest_trusted_syscall:\n'), 1)
        changed = (self.base[:x86['start']] + body.replace(b'\nreverie_inguest_trusted_syscall:\n', b'\nrenamed_entry:\n')
                   + self.base[x86['end']:])
        with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: contributing x86_64 block at line [0-9]+ '
                                    'does not define reverie_inguest_trusted_syscall'):
            cohort.gate_shape(changed)

    def test_compiled_check_pins_a_44_byte_dedicated_section(self):
        # ordinary_gate.rs is the compiled half of this guard: the provider
        # accepts the DedicatedFunction cohort only for a 44-byte input section
        # of exactly this name. Cite it from the same pinned objects.
        source = live_reverie_object(ORDINARY_GATE).decode()
        self.assertIn('const DEDICATED_SECTION: &str = "' + cohort.TRUSTED_SYSCALL_SECTION + '";', source)
        text_size = source[source.index('    fn text_size(self) -> u64 {'):]
        text_size = text_size[:text_size.index('\n    }\n')]
        self.assertIn('Self::DedicatedFunction => 44,', text_size)
        body = source[source.index('const BODY: [u8; 44] = ['):]
        body = body[body.index('[', body.index('=')) + 1:body.index('];')]
        self.assertEqual(len([v for v in body.replace('\n', ' ').split(',') if v.strip()]), 44)


class OrdinaryTrapSource(unittest.TestCase):
    """prepare.py's trap.rs role check for the ordinary provider."""

    @classmethod
    def setUpClass(cls):
        cls.prepare = load('prepare')
        cls.base = live_reverie_object(TRAP)

    def value(self, root, raw):
        path = root / TRAP
        path.parent.mkdir(parents=True)
        path.write_bytes(raw)
        return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest()}

    def test_dedicated_function_binds_the_gate_shape_not_a_file_literal(self):
        with tempfile.TemporaryDirectory() as directory:
            value = self.value(Path(directory), self.base)
            shape = self.prepare.ordinary_trap_source('DedicatedFunction', value)
            self.assertEqual(shape['x86_64'], cohort.TRUSTED_SYSCALL_X86_64_BLOCK_SHA256)

    def test_dedicated_function_refuses_an_x86_64_gate_edit(self):
        x86 = next(b for b in cohort.gate_shape(self.base)['blocks'] if b['arch'] == 'x86_64' and b['contributes'])
        cut = self.base.index(b'\n', self.base.index(b'\n', x86['start']) + 1) + 1
        changed = self.base[:cut] + b'    // x86_64 edit\n' + self.base[cut:]
        with tempfile.TemporaryDirectory() as good, tempfile.TemporaryDirectory() as bad:
            self.prepare.ordinary_trap_source('DedicatedFunction', self.value(Path(good), self.base))
            with self.assertRaisesRegex(RuntimeError, 'trusted syscall gate shape: x86_64 block sha256'):
                self.prepare.ordinary_trap_source('DedicatedFunction', self.value(Path(bad), changed))

    def test_reported_digest_must_be_the_file_read(self):
        with tempfile.TemporaryDirectory() as directory:
            value = self.value(Path(directory), self.base)
            self.prepare.ordinary_trap_source('DedicatedFunction', value)
            value['sha256'] = '0' * 64
            with self.assertRaisesRegex(RuntimeError, 'ordinary source role differs from actual factory source'):
                self.prepare.ordinary_trap_source('DedicatedFunction', value)

    def test_historical_cohorts_keep_their_reviewed_literal(self):
        legacy = '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'
        for name in ['LegacyE3', 'CheckedHandoff5b3']:
            with self.subTest(cohort=name):
                self.assertIsNone(self.prepare.ordinary_trap_source(
                    name, {'path': '/unit/' + TRAP, 'sha256': legacy}))
                with self.assertRaisesRegex(RuntimeError, 'ordinary source role differs from actual factory source'):
                    self.prepare.ordinary_trap_source(name, {'path': '/unit/' + TRAP, 'sha256': '0' * 64})
        with self.assertRaisesRegex(RuntimeError, 'ordinary unknown source/section cohort'):
            self.prepare.ordinary_trap_source('Unknown', {'path': '/unit/' + TRAP, 'sha256': legacy})


class PinBumpReport(unittest.TestCase):
    """The review attention a re-pin needs, listed per changed R row."""

    def bump(self, tree, path, raw):
        target = tree.reverie / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        fixture_git(tree.reverie, 'add', '-A')
        fixture_git(tree.reverie, 'commit', '-q', '-m', 'bump')
        return fixture_git(tree.reverie, 'rev-parse', 'HEAD')

    def test_report_names_a_changed_trap_source(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            old = tree.rpin
            raw = (tree.reverie / TRAP).read_bytes() + b'// appended outside every gate block\n'
            new = self.bump(tree, TRAP, raw)
            rows = cohort.pin_bump_report(tree.reverie, old, new)
            self.assertEqual([row['path'] for row in rows], [TRAP])
            row = rows[0]
            self.assertEqual(row['new_sha256'], hashlib.sha256(raw).hexdigest())
            self.assertEqual(row['old_sha256'], hashlib.sha256(live_reverie_object(TRAP)).hexdigest())
            self.assertEqual(row['stat'], '+1 -0')
            self.assertEqual(row['gate']['old']['x86_64'], row['gate']['new']['x86_64'])
            text = cohort.format_pin_bump_report(old, new, rows)
            self.assertEqual([line.split()[1] for line in text.splitlines() if line.startswith('R ')], [TRAP])

    def test_report_is_empty_when_no_reviewed_row_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            old = tree.rpin
            new = self.bump(tree, 'README.md', b'only an unreviewed file changes\n')
            rows = cohort.pin_bump_report(tree.reverie, old, new)
            self.assertEqual(rows, [])
            text = cohort.format_pin_bump_report(old, new, rows)
            self.assertEqual([line for line in text.splitlines() if line.startswith('R ')], [])

    def test_report_cli_prints_the_changed_row(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            old = tree.rpin
            new = self.bump(tree, 'reverie-loader/src/lib.rs', b'changed loader\n')
            child = subprocess.run([sys.executable, '-B', str(D.parent / 'pin_bump_report.py'),
                                    '--reverie-git', str(tree.reverie), old, new],
                                   capture_output=True, text=True, timeout=60, env=GIT_FIXTURE_ENV)
            self.assertEqual(child.returncode, 0, child.stderr)
            self.assertEqual([line.split()[1] for line in child.stdout.splitlines() if line.startswith('R ')],
                             ['reverie-loader/src/lib.rs'])

    def test_report_refuses_a_missing_revision_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = PinnedTree(Path(directory))
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: missing object R ' + 'e' * 40 + ':'):
                cohort.pin_bump_report(tree.reverie, tree.rpin, 'e' * 40)


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
                    with patch.object(cohort, 'select', side_effect=observe(cohort.select)), \
                         patch.object(publisher, 'fixed_gate_data', side_effect=observe(publisher.fixed_gate_data)):
                        cls(method).run(result)
                    self.assertEqual(result.errors + result.failures, [])
                    self.assertTrue(events)
                    self.assertEqual(events, ['accepted', 'refused'] * (len(events) // 2))


if __name__ == '__main__':
    unittest.main()
