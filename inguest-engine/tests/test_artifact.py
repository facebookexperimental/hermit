"""Producer format/custody controls, not compiled ELF or loader qualification."""

import importlib.util
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "artifact.py"
SPEC = importlib.util.spec_from_file_location("engine_artifact_controls", SOURCE)
artifact = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(artifact)


class ProducerControls(unittest.TestCase):
    def test_cargo_stream_requires_one_success_and_complete_json(self):
        success = b'{"reason":"build-finished","success":true}\n'
        self.assertEqual(artifact.parse_cargo_events(success)[0]["success"], True)
        for raw in (b"", b"{", success + success,
                    b'{"reason":"build-finished","success":false}\n',
                    success + b'{"reason":"compiler-artifact"}\n'):
            with self.assertRaises(ValueError):
                artifact.parse_cargo_events(raw)

    def test_exact_cdylib_selection_refuses_duplicates_and_wrong_features(self):
        root = Path(tempfile.gettempdir()).resolve() / "engine-source-fixture"
        package = {"name": "hermit-inguest-engine", "id": "fixture-package",
                   "manifest_path": str(root / "inguest-engine/Cargo.toml")}
        unit = {"reason": "compiler-artifact", "package_id": "fixture-package",
                "target": {"name": "hermit_inguest_engine", "crate_types": ["cdylib"]},
                "profile": {"test": False}, "features": []}
        metadata = {"packages": [package]}
        self.assertEqual(artifact.select_engine([unit], metadata, root), (package, unit))
        for units in ([unit, unit], [{**unit, "features": ["sabre"]}],
                      [{**unit, "profile": {"test": True}}],
                      [{**unit, "package_id": "other-package"}],
                      [{**unit, "target": {"name": "hermit_inguest_engine",
                                             "crate_types": ["rlib"]}}]):
            with self.assertRaises(ValueError):
                artifact.select_engine(units, metadata, root)

    def test_exclusive_copy_is_exact_and_never_replaces_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, destination = root / "source", root / "installed"
            source.write_bytes(b"whole exact artifact fixture\x00\xff")
            result = artifact.copy_exclusive(source, destination, 0o700)
            self.assertEqual(result, artifact.digest(destination))
            self.assertEqual(source.read_bytes(), destination.read_bytes())
            with self.assertRaises(FileExistsError):
                artifact.copy_exclusive(source, destination, 0o700)
            self.assertEqual(source.read_bytes(), destination.read_bytes())

    def test_held_identity_rejects_replacement_and_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.write_bytes(b"same bytes")
            held, before = artifact.open_regular(source)
            try:
                replacement = root / "replacement"
                replacement.write_bytes(b"same bytes")
                os.replace(replacement, source)
                with self.assertRaises(ValueError):
                    artifact.recheck(source, held, before)
            finally:
                held.close()
            link = root / "link"
            link.symlink_to(source)
            with self.assertRaises(ValueError):
                artifact.digest(link)

    def test_elf_format_refuses_wrong_architecture_type_and_missing_execute_bit(self):
        # Deliberately only a format fixture: it cannot establish a linked DSO.
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "elf"
            raw = bytearray(64)
            raw[:7] = b"\x7fELF\x02\x01\x01"
            struct.pack_into("<HH", raw, 16, 3, 62)
            path.write_bytes(raw)
            path.chmod(0o700)
            artifact.elf(path, shared=True)
            for kind, machine in ((2, 62), (3, 183)):
                altered = bytearray(raw)
                struct.pack_into("<HH", altered, 16, kind, machine)
                path.write_bytes(altered)
                with self.assertRaises(ValueError):
                    artifact.elf(path, shared=True)
            path.write_bytes(raw)
            path.chmod(0o600)
            with self.assertRaises(ValueError):
                artifact.elf(path, shared=True)

    def test_full_revision_is_required(self):
        self.assertEqual(artifact.revision("a" * 40), "a" * 40)
        for invalid in ("a" * 12, "A" * 40, "a" * 39 + "z", "main", ""):
            with self.assertRaises(ValueError):
                artifact.revision(invalid)

    def test_complete_cohort_requires_both_exact_header_roles_and_bytes(self):
        # Only a producer-format fixture, never a compiled/linked DSO claim.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = bytearray(64)
            raw[:7] = b"\x7fELF\x02\x01\x01"
            struct.pack_into("<HH", raw, 16, 3, 62)
            engine = root / artifact.ENGINE
            engine.write_bytes(raw)
            engine.chmod(0o700)
            for name in (artifact.HEADER, artifact.NATIVE_HEADER):
                (root / name).write_bytes((SOURCE.parent / "include" / name).read_bytes())
            record = {
                "schema": artifact.SCHEMA, "kind": "hermit-inguest-engine",
                "core_h_revision": "a" * 40, "core_h_tree": "b" * 40,
                "core_r_revision": "c" * 40, "profile": {"test": False},
                "features": [], "cargo_package_id": "format-fixture-only",
                "compiled_units": [{"fixture": "not a compiled artifact"}],
                "source_records": {name: {} for name in (
                    "cargo_events", "cargo_metadata", "cargo_lock",
                    "package_manifest", "installer")},
                "qualification": "format fixture only",
                "artifacts": {role: artifact.digest(root / name) for role, name in (
                    ("engine", artifact.ENGINE), ("header", artifact.HEADER),
                    ("native_header", artifact.NATIVE_HEADER))},
            }
            manifest = root / artifact.MANIFEST
            manifest.write_text(json.dumps(record))
            self.assertEqual(artifact.check(manifest), record)
            for roles in (
                {key: value for key, value in record["artifacts"].items()
                 if key != "native_header"},
                {**record["artifacts"], "unknown_header": record["artifacts"]["header"]},
                {**record["artifacts"], "native_header": record["artifacts"]["header"]},
            ):
                manifest.write_text(json.dumps({**record, "artifacts": roles}))
                with self.assertRaises(ValueError):
                    artifact.check(manifest)
            manifest.write_text(json.dumps(record))
            native = root / artifact.NATIVE_HEADER
            native.write_bytes(native.read_bytes() + b"\n/* changed after install */\n")
            with self.assertRaises(ValueError):
                artifact.check(manifest)


if __name__ == "__main__":
    unittest.main()
