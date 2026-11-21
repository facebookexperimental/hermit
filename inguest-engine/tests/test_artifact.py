"""Producer format/custody controls, not compiled ELF or loader qualification."""

import importlib.util
import json
import os
import subprocess
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

    def test_workspace_reverie_deps_accept_actual_layout_and_refuse_wrong_ones(self):
        import tomllib

        engine_manifest = tomllib.loads(
            (SOURCE.parent / "Cargo.toml").read_text())
        workspace_manifest = tomllib.loads(
            (SOURCE.parents[1] / "Cargo.toml").read_text())
        # The actual layout has no rev keys: the pre-fix check raised
        # KeyError here. The workspace/path check must accept it.
        artifact.require_workspace_reverie_deps(
            engine_manifest, workspace_manifest)

        for mutate in (
            # A wrong gitlink in path's clothing: renamed submodule path.
            lambda ws: ws["workspace"]["dependencies"]["reverie-core"].update(
                path="elsewhere/reverie"),
            # A rev pin is a different provenance than the gitlink.
            lambda ws: ws["workspace"]["dependencies"]["reverie-inguest"].update(
                rev="c" * 40),
            lambda ws: ws["workspace"]["dependencies"]["reverie-liteinst"].update(
                git="https://example.invalid/reverie.git"),
        ):
            workspace = json.loads(json.dumps(workspace_manifest))
            mutate(workspace)
            with self.assertRaises(ValueError):
                artifact.require_workspace_reverie_deps(engine_manifest, workspace)

        engine = json.loads(json.dumps(engine_manifest))
        engine["dependencies"]["reverie-core"] = {"path": "../reverie/reverie"}
        with self.assertRaises(ValueError):
            artifact.require_workspace_reverie_deps(engine, workspace_manifest)

    def test_compiled_path_dependency_requires_the_exact_submodule_path(self):
        root = Path(tempfile.gettempdir()).resolve() / "engine-source-fixture"
        for name, path in artifact.REVERIE_WORKSPACE_DEPS.items():
            good = {
                "source": None,
                "manifest_path": str(root / path / "Cargo.toml"),
            }
            self.assertEqual(
                artifact.require_compiled_path_dependency(good, name, root),
                (root / path).resolve(),
            )
            for bad in (
                # A git dependency at another Reverie commit.
                {**good, "source": "git+https://example.invalid/reverie.git#" + "c" * 40},
                # A registry dependency.
                {**good, "source": "registry+https://example.invalid/index"},
                # Inside the submodule, but not the declared package path.
                {**good, "manifest_path": str(root / "reverie" / "elsewhere" / "Cargo.toml")},
                # Entirely outside the submodule.
                {**good, "manifest_path": str(root / "elsewhere" / "Cargo.toml")},
            ):
                with self.assertRaises(ValueError, msg=f"{name}: {bad}"):
                    artifact.require_compiled_path_dependency(bad, name, root)

    def test_install_wires_both_workspace_checks(self):
        # Mutant control at the wiring level: removing either call from
        # install(), or restoring the old rev-key loop, fails this test.
        source = SOURCE.read_text()
        install = source.split("def install(", 1)[1]
        self.assertIn("require_workspace_reverie_deps(source_manifest, workspace_manifest)", install)
        self.assertIn("require_compiled_path_dependency(dependency, name, root)", install)
        self.assertNotIn('["rev"]', install)
        self.assertIn("for name in REVERIE_WORKSPACE_DEPS:", install)

    def test_every_workspace_reverie_dep_is_bound_by_the_table(self):
        import tomllib

        engine_manifest = tomllib.loads((SOURCE.parent / "Cargo.toml").read_text())
        workspace_manifest = tomllib.loads(
            (SOURCE.parents[1] / "Cargo.toml").read_text())
        workspace_deps = workspace_manifest["workspace"]["dependencies"]
        resolved_into_reverie = {
            name
            for name, selected in engine_manifest["dependencies"].items()
            if isinstance(selected, dict)
            and selected.get("workspace") is True
            and isinstance(workspace_deps.get(name), dict)
            and str(workspace_deps[name].get("path", "")).startswith("reverie/")
        }
        self.assertEqual(resolved_into_reverie, set(artifact.REVERIE_WORKSPACE_DEPS))

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


class InstallLevelControls(unittest.TestCase):
    """One real install() over a scratch superproject + reverie submodule.

    Only the Cargo metadata/events and the emitted ELF are synthetic
    (the seat's fixture shape); every git, manifest and provenance
    check in install() runs for real, so deleting either workspace
    check, or restoring the old rev-key loop, fails a case here.
    """

    def git(self, root: Path, *arguments: str) -> str:
        env = {**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null"}
        result = subprocess.run(
            ["git", "-C", str(root),
             "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "protocol.file.allow=always", *arguments],
            capture_output=True, check=False, env=env, timeout=30)
        if result.returncode != 0:
            raise AssertionError(result.stderr.decode())
        return result.stdout.decode().strip()

    def build_fixture(self, base: Path, *, workspace_paths=None):
        """A committed superproject whose install() can succeed.

        Returns (module, args-ready dict). `workspace_paths` overrides
        the workspace dependency paths (committed, so only the layout
        check can refuse it).
        """
        import types

        base.mkdir(parents=True)
        paths = dict(artifact.REVERIE_WORKSPACE_DEPS)
        paths.update(workspace_paths or {})

        submodule = base / "reverie-source"
        submodule.mkdir()
        self.git(submodule, "init", "-q")
        for crate_path in artifact.REVERIE_WORKSPACE_DEPS.values():
            crate = submodule / Path(crate_path).relative_to("reverie")
            crate.mkdir(parents=True)
            (crate / "Cargo.toml").write_text("[package]\nname = \"fixture\"\n")
        self.git(submodule, "add", "-A")
        self.git(submodule, "commit", "-qm", "reverie fixture")
        reverie_revision = self.git(submodule, "rev-parse", "HEAD")

        root = base / "core"
        (root / "inguest-engine" / "include").mkdir(parents=True)
        (root / "detcore").mkdir()
        engine_deps = "".join(
            f'{name} = {{ workspace = true }}\n' for name in paths)
        (root / "inguest-engine" / "Cargo.toml").write_text(
            "[dependencies]\n" + engine_deps)
        workspace_deps = "".join(
            f'{name} = {{ path = "{path}" }}\n' for name, path in paths.items())
        (root / "Cargo.toml").write_text(
            "[workspace]\nmembers = []\n[workspace.dependencies]\n" + workspace_deps)
        (root / "detcore" / "Cargo.toml").write_text(
            "[package]\nname = \"hermit-detcore\"\n")
        (root / "Cargo.lock").write_text("# fixture lock\n")
        for name in (artifact.HEADER, artifact.NATIVE_HEADER):
            (root / "inguest-engine" / "include" / name).write_bytes(
                (SOURCE.parent / "include" / name).read_bytes())
        (root / "inguest-engine" / "artifact.py").write_bytes(SOURCE.read_bytes())
        self.git(root, "init", "-q")
        self.git(root, "submodule", "add", "-q", str(submodule), "reverie")
        self.git(root, "add", "-A")
        self.git(root, "commit", "-qm", "core fixture")
        hermit_revision = self.git(root, "rev-parse", "HEAD")

        target = base / "target"
        emitted = target / artifact.ENGINE
        emitted.parent.mkdir(parents=True)
        raw = bytearray(64)
        raw[:7] = b"\x7fELF\x02\x01\x01"
        struct.pack_into("<HH", raw, 16, 3, 62)
        emitted.write_bytes(raw)
        emitted.chmod(0o755)

        packages = [{
            "name": "hermit-inguest-engine", "id": "engine-package",
            "manifest_path": str(root / "inguest-engine" / "Cargo.toml"),
            "source": None,
        }, {
            "name": "hermit-detcore", "id": "detcore-package",
            "manifest_path": str(root / "detcore" / "Cargo.toml"),
            "source": None,
        }] + [{
            "name": name, "id": f"pkg-{name}",
            "manifest_path": str(root / path / "Cargo.toml"),
            "source": None,
        } for name, path in artifact.REVERIE_WORKSPACE_DEPS.items()]
        units = [{
            "reason": "compiler-artifact", "package_id": package["id"],
            "target": {"name": "hermit_inguest_engine", "crate_types": ["cdylib"]},
            "profile": {"test": False}, "features": [], "fresh": False,
            "filenames": [str(emitted)],
        } if package["name"] == "hermit-inguest-engine" else {
            "reason": "compiler-artifact", "package_id": package["id"],
            "target": {"name": package["name"], "crate_types": ["lib"]},
            "profile": {"test": False}, "features": [], "fresh": False, "filenames": [],
        } for package in packages]
        metadata = {
            "packages": packages,
            "workspace_root": str(root),
            "target_directory": str(target),
        }
        metadata_path = base / "metadata.json"
        metadata_path.write_text(json.dumps(metadata))
        events_path = base / "events.jsonl"
        events_path.write_text("".join(
            json.dumps(unit) + "\n" for unit in units)
            + '{"reason":"build-finished","success":true}\n')

        spec = importlib.util.spec_from_file_location(
            "engine_artifact_scratch", root / "inguest-engine" / "artifact.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        args = types.SimpleNamespace(
            hermit_revision=hermit_revision,
            reverie_revision=reverie_revision,
            cargo_metadata=str(metadata_path),
            cargo_events=str(events_path),
            target_dir=str(target),
            destination=str(base / "installed"),
        )
        return module, root, args, metadata, metadata_path

    def rewrite_metadata(self, path: Path, metadata: dict) -> None:
        path.write_text(json.dumps(metadata))

    def test_install_accepts_the_workspace_layout_and_refuses_each_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()

            module, _root, args, _metadata, _path = self.build_fixture(
                base / "valid")
            module.install(args)
            self.assertTrue(
                (Path(args.destination) / module.MANIFEST).is_file())

            # A wrong workspace path, committed, is refused by the
            # layout check (the old rev-key loop would KeyError instead).
            module, _root, args, _m, _p = self.build_fixture(
                base / "wrong-path",
                workspace_paths={"reverie-core": "elsewhere/reverie"})
            with self.assertRaises(ValueError):
                module.install(args)

            # An untracked file inside the submodule refuses.
            module, root, args, _m, _p = self.build_fixture(base / "untracked")
            (root / "reverie" / "reverie" / "untracked.txt").write_text("x")
            with self.assertRaises(ValueError):
                module.install(args)

            # rpc-transport compiled from git at another revision refuses.
            module, _root, args, metadata, path = self.build_fixture(
                base / "git-rpc")
            for package in metadata["packages"]:
                if package["name"] == "reverie-rpc-transport":
                    package["source"] = "git+https://example.invalid/r.git#" + "c" * 40
            self.rewrite_metadata(path, metadata)
            with self.assertRaises(ValueError):
                module.install(args)

            # A manifest merely inside the submodule is not the package.
            module, _root, args, metadata, path = self.build_fixture(
                base / "wrong-manifest")
            for package in metadata["packages"]:
                if package["name"] == "reverie-core":
                    package["manifest_path"] = str(
                        Path(package["manifest_path"]).parent.parent
                        / "reverie-inguest" / "Cargo.toml")
            self.rewrite_metadata(path, metadata)
            with self.assertRaises(ValueError):
                module.install(args)


if __name__ == "__main__":
    unittest.main()
