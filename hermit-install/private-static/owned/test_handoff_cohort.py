"""Prepared data-only source/preflight controls, never a source-factory positive.

No candidate import, compiler, metadata, runtime entry or guest. The handoff
source identity is the row derived from the pinned Reverie object; no consumer
carries a literal handoff digest.
"""
import ast
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

D = Path(__file__).resolve().parent
HANDOFF = 'reverie-loader/src/handoff.rs'
# Reviewed historical handoff identities, retained as fixtures only.
HISTORICAL = '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'
LEGACY = '85e0f1cb9393c1ed11db4d6115f81dbce5d3f0afcfc733aa4d0cd2d6725245c7'


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, D / filename)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


fixtures = load('handoff_cohort_fixtures', 'test_runtime_cohort.py')
prepare = load('handoff_cohort_prepare', 'prepare.py')


def protocol_predicate():
    """The one actual protocol predicate, extracted from the maintained source."""
    path = D / 'view_leaf_protocol.py'
    tree = ast.parse(path.read_bytes())
    predicates = [n.args[0] for n in ast.walk(tree) if isinstance(n, ast.Call)
                  and isinstance(n.func, ast.Name) and n.func.id == 'require'
                  and len(n.args) == 2 and isinstance(n.args[1], ast.Constant)
                  and n.args[1].value == 'view leaf actual handoff source differs']
    if len(predicates) != 1:
        raise AssertionError('actual protocol source predicate missing/duplicated')
    return compile(ast.Expression(predicates[0]), str(path), 'eval')


class HandoffCohort(unittest.TestCase):
    def test_no_consumer_carries_a_handoff_digest_literal(self):
        # Structural, replacing the former sha256(prepare.py) self-pin: that pin
        # failed on every edit of its sibling and guarded nothing the AST
        # extraction below does not. What must hold is that neither consumer
        # declares its own handoff digest of any vintage.
        for name in ['prepare.py', 'view_leaf_protocol.py']:
            with self.subTest(source=name):
                raw = (D / name).read_text()
                tree = ast.parse(raw)
                assigned = [t.id for n in tree.body if isinstance(n, ast.Assign)
                            for t in n.targets if isinstance(t, ast.Name)]
                self.assertEqual([v for v in assigned if 'HANDOFF' in v and 'SHA' in v], [])
                for literal in [HISTORICAL, LEGACY]:
                    self.assertNotIn(literal, raw)

    def test_historical_identity_is_the_reviewed_object_fixture(self):
        row = next(r for r in fixtures.cohort.CURRENT_SPLIT_RUNTIME if r[:2] == ('R', HANDOFF))
        self.assertEqual(row[3], HISTORICAL)
        rev = fixtures.HISTORICAL_SOURCE_COMMITS['current-split-runtime-content-v1']['R']
        raw = fixtures.fixture_git_bytes(fixtures.LIVE / 'reverie', 'cat-file', 'blob', rev + ':' + HANDOFF)
        self.assertEqual((len(raw), hashlib.sha256(raw).hexdigest()), row[2:])

    def test_live_handoff_source_matches_its_pinned_object(self):
        head = fixtures.fixture_git(fixtures.LIVE, 'rev-parse', 'HEAD')
        raw = fixtures.live_reverie_object(HANDOFF)
        actual = prepare.runtime_handoff_preflight({'H': str(fixtures.LIVE), 'H_head': head})
        self.assertEqual(actual, {'path': str(fixtures.LIVE / 'reverie' / HANDOFF), 'bytes': len(raw),
                                  'sha256': hashlib.sha256(raw).hexdigest()})

    def test_handoff_that_differs_from_its_derived_row_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = fixtures.PinnedTree(Path(directory))
            c = {'H': str(tree.root), 'H_head': tree.h_head, 'runtime_R_pin': tree.rpin}
            raw = (tree.reverie / HANDOFF).read_bytes()
            self.assertEqual(prepare.runtime_handoff_preflight(c)['sha256'], hashlib.sha256(raw).hexdigest())
            with patch.object(prepare, 'derived_runtime_row', return_value=(len(raw), HISTORICAL)):
                with self.assertRaisesRegex(RuntimeError, '^actual runtime handoff source differs from its pinned '
                                            'Reverie object$'):
                    prepare.runtime_handoff_preflight(c)

    def test_edited_handoff_is_refused_before_owned_work(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = fixtures.PinnedTree(Path(directory))
            c = {'H': str(tree.root), 'H_head': tree.h_head}
            prepare.runtime_handoff_preflight(c)
            (tree.reverie / HANDOFF).write_bytes(b'not the pinned handoff source')
            with self.assertRaisesRegex(RuntimeError, 'runtime cohort derivation: reverie submodule is dirty'):
                prepare.runtime_handoff_preflight(c)

    def test_missing_or_symlink_source_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = fixtures.PinnedTree(Path(directory))
            c = {'H': str(tree.root), 'H_head': tree.h_head}
            prepare.runtime_handoff_preflight(c)
            path = tree.reverie / HANDOFF
            target = tree.root / 'other'
            target.write_bytes(path.read_bytes())
            path.unlink()
            with self.assertRaisesRegex(RuntimeError, '^nonregular/indirect evidence:'):
                prepare.runtime_handoff_preflight(c)
            path.symlink_to(target)
            with self.assertRaisesRegex(RuntimeError, '^nonregular/indirect evidence:'):
                prepare.runtime_handoff_preflight(c)

    def test_only_the_preflight_digest_passes_the_protocol_predicate(self):
        predicate = protocol_predicate()
        for value, expected in [(HISTORICAL, True), (LEGACY, False), ('0' * 64, False),
                                (HISTORICAL[:-1], False), (None, False)]:
            observation = {'source': {'sha256': value}, 'declared_expected_sha256': value}
            with self.subTest(digest=value):
                self.assertIs(eval(predicate, {}, {'observation': observation, 'handoff_sha': HISTORICAL}), expected)

    def test_report_cannot_nominate_its_own_expected_digest(self):
        observation = {'source': {'sha256': LEGACY}, 'handoff_sha': LEGACY, 'HANDOFF_SOURCE_SHA256': LEGACY}
        self.assertFalse(eval(protocol_predicate(), {}, {'observation': observation, 'handoff_sha': HISTORICAL}))


if __name__ == '__main__':
    unittest.main()
