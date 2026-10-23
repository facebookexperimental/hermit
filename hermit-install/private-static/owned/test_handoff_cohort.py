"""Prepared data-only source/preflight controls, never a source-factory positive.

No candidate import, compiler, metadata, runtime entry or guest. These controls
extract the exact frozen helper/predicate AST; execution belongs to root.
"""
import ast
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

D = Path(__file__).resolve().parent
PREPARE_SHA = 'f78167f4cc6520b6316d006695eb68d073aeee0b83d37ce67ffa7c9601029d45'
PROTOCOL_SHA = 'd6ef0e73d539ba0f8824610ad1140ffa1170eee72e11bd7863e0843993fdafa5'
EXPECTED = '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'
LEGACY = '85e0f1cb9393c1ed11db4d6115f81dbce5d3f0afcfc733aa4d0cd2d6725245c7'

class HandoffCohort(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        raw = (D / 'prepare.py').read_bytes()
        if hashlib.sha256(raw).hexdigest() != PREPARE_SHA:
            raise AssertionError('candidate prepare changed')
        tree = ast.parse(raw)
        names = {'need', 'sha', 'file_fact', 'runtime_handoff_preflight'}
        selected = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in names]
        if len(selected) != len(names):
            raise AssertionError('actual preflight helpers missing/duplicated')
        const = [n.value.value for n in tree.body if isinstance(n, ast.Assign)
                 and any(isinstance(t, ast.Name) and t.id == 'REQUIRED_HANDOFF_SOURCE_SHA256' for t in n.targets)]
        if const != [EXPECTED]:
            raise AssertionError('independent preflight cohort changed')
        cls.scope = {'Path': Path, 'hashlib': hashlib, 'REQUIRED_HANDOFF_SOURCE_SHA256': EXPECTED}
        exec(compile(ast.Module(body=selected, type_ignores=[]), str(D / 'prepare.py'), 'exec'), cls.scope)
        raw = (D / 'view_leaf_protocol.py').read_bytes()
        if hashlib.sha256(raw).hexdigest() != PROTOCOL_SHA:
            raise AssertionError('candidate protocol changed')
        tree = ast.parse(raw)
        const = [n.value.value for n in tree.body if isinstance(n, ast.Assign)
                 and any(isinstance(t, ast.Name) and t.id == 'HANDOFF_SOURCE_SHA256' for t in n.targets)]
        if const != [EXPECTED]:
            raise AssertionError('protocol cohort differs from independent preflight')
        predicates = [n.args[0] for n in ast.walk(tree) if isinstance(n, ast.Call)
                      and isinstance(n.func, ast.Name) and n.func.id == 'require'
                      and len(n.args) == 2 and isinstance(n.args[1], ast.Constant)
                      and n.args[1].value == 'view leaf actual handoff source differs']
        if len(predicates) != 1:
            raise AssertionError('actual protocol source predicate missing/duplicated')
        cls.predicate = compile(ast.Expression(predicates[0]), str(D / 'view_leaf_protocol.py'), 'eval')

    def path(self, root):
        p = Path(root) / 'reverie/reverie-loader/src/handoff.rs'
        p.parent.mkdir(parents=True)
        return p

    def test_actual_declared_source_bytes_pass_preflight(self):
        # Maintained source root; retain the actual pinned handoff and exact hash.
        h = D.parents[2]
        source = h / 'reverie/reverie-loader/src/handoff.rs'
        raw = source.read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(), EXPECTED)
        with tempfile.TemporaryDirectory() as root:
            p = self.path(root)
            p.write_bytes(raw)
            actual = self.scope['runtime_handoff_preflight']({'H': root})
            self.assertEqual(actual, {'path': str(p), 'bytes': len(raw), 'sha256': EXPECTED})

    def test_unknown_modified_bytes_fail_before_owned_work(self):
        with tempfile.TemporaryDirectory() as root:
            self.path(root).write_bytes(b'not the declared handoff source')
            with self.assertRaisesRegex(RuntimeError, '^actual runtime handoff source differs from declared V10 cohort$'):
                self.scope['runtime_handoff_preflight']({'H': root})

    def test_missing_source_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(RuntimeError, '^nonregular/indirect evidence:'):
                self.scope['runtime_handoff_preflight']({'H': root})

    def test_symlink_source_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            p = self.path(root)
            target = Path(root) / 'other'
            target.write_bytes(b'other')
            p.symlink_to(target)
            with self.assertRaisesRegex(RuntimeError, '^nonregular/indirect evidence:'):
                self.scope['runtime_handoff_preflight']({'H': root})

    def test_only_declared_digest_passes_actual_protocol_predicate(self):
        for value, expected in [(EXPECTED, True), (LEGACY, False), ('0' * 64, False), (EXPECTED[:-1], False), (None, False)]:
            observation = {'source': {'sha256': value}, 'declared_expected_sha256': value}
            with self.subTest(digest=value):
                self.assertIs(eval(self.predicate, {'HANDOFF_SOURCE_SHA256': EXPECTED}, {'observation': observation}), expected)

    def test_report_cannot_nominate_its_own_expected_digest(self):
        observation = {'source': {'sha256': LEGACY}, 'HANDOFF_SOURCE_SHA256': LEGACY}
        self.assertFalse(eval(self.predicate, {'HANDOFF_SOURCE_SHA256': EXPECTED}, {'observation': observation}))

if __name__ == '__main__':
    unittest.main()
