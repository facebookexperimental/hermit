#!/usr/bin/env python3
"""Contract tests for host-visible, checkout-scoped QEMU demo paths."""

import os
import runpy
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
QEMU_PATHS = LIB_DIR / "qemu-paths.sh"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402


def _shell_default(root: Path) -> Path:
    return Path(
        subprocess.check_output(
            [str(QEMU_PATHS), str(root)],
            text=True,
        ).strip()
    )


def _make_default(extra_env=None) -> Path:
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)
    output = subprocess.check_output(
        [
            "make",
            "-C",
            str(DEMO_DIR),
            "--no-print-directory",
            "-s",
            "--eval=print-assets: ; @echo $(QEMU_ASSETS)",
            "print-assets",
        ],
        text=True,
        env=env,
    )
    return Path(output.strip())


class DefaultQemuAssetsTest(unittest.TestCase):
    def test_dependency_check_reports_the_hermit_on_path(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="hermit 0.2.0 (2026-09-30, g0123456789ab)\n",
            stderr="",
        )
        with mock.patch.object(dc.shutil, "which", return_value="/opt/bin/hermit"):
            with mock.patch.object(dc.subprocess, "run", return_value=completed):
                self.assertEqual(
                    dc.check_dependencies(DEMO_DIR.parent),
                    "Dependency check passed: hermit 0.2.0 "
                    "(2026-09-30, g0123456789ab) (/opt/bin/hermit)",
                )

    def test_dependency_check_refuses_unexpected_version_output(self):
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="something else\n", stderr=""
        )
        with mock.patch.object(dc.shutil, "which", return_value="/opt/bin/hermit"):
            with mock.patch.object(dc.subprocess, "run", return_value=completed):
                with self.assertRaisesRegex(
                    RuntimeError, "unexpected `hermit --version`"
                ):
                    dc.check_dependencies(DEMO_DIR.parent)

    def test_dependency_check_names_the_missing_hermit(self):
        with mock.patch.object(dc.shutil, "which", return_value=None):
            with mock.patch.object(dc.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "not on PATH"):
                    dc.check_dependencies(DEMO_DIR.parent)
                run.assert_not_called()

    def test_tmp_checkouts_are_host_visible_and_checkout_scoped(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as first, tempfile.TemporaryDirectory(
            dir="/tmp"
        ) as second:
            first_root = Path(first)
            second_root = Path(second)
            first_assets = dc.default_qemu_assets(first_root)
            second_assets = dc.default_qemu_assets(second_root)

            self.assertEqual(first_assets, _shell_default(first_root))
            self.assertEqual(second_assets, _shell_default(second_root))
            self.assertNotEqual(first_assets, second_assets)
            self.assertEqual(first_assets.parent, Path("/var/tmp"))
            self.assertTrue(
                first_assets.name.startswith(
                    "hermit-demo-qemu-{}-".format(os.getuid())
                )
            )

    def test_non_tmp_checkout_keeps_repo_local_ignored_directory(self):
        root_path = Path.home().resolve()
        expected = root_path / "ignored/qemu-linux"
        self.assertEqual(dc.default_qemu_assets(root_path), expected)
        self.assertEqual(_shell_default(root_path), expected)

    def test_symlinked_tmp_checkout_matches_python_canonicalization(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as physical, tempfile.TemporaryDirectory(
            dir="/tmp"
        ) as links:
            linked_root = Path(links) / "checkout"
            linked_root.symlink_to(physical, target_is_directory=True)
            self.assertEqual(
                dc.default_qemu_assets(linked_root), _shell_default(linked_root)
            )

    def test_tmp_checkout_adds_the_identity_tmp_mount(self):
        self.assertEqual(dc.hermit_tmp_args(Path("/tmp/work/hermit")), ["--tmp=/tmp"])
        self.assertEqual(dc.hermit_tmp_args(Path("/srv/hermit")), [])

    def test_external_paths_render_without_relative_to_failure(self):
        external = Path("/var/tmp/assets/run-metadata.json")
        self.assertEqual(dc.display_path(external, Path("/tmp/hermit")), str(external))
        self.assertEqual(
            dc.display_path(Path("/tmp/hermit/demos/out"), Path("/tmp/hermit")),
            "demos/out",
        )

    def test_python_entrypoints_preserve_explicit_override(self):
        chosen = "/var/tmp/operator-selected-qemu-assets"
        with mock.patch.dict(os.environ, {"QEMU_ASSETS": chosen}):
            for script in ("05-qemu-boot/run.py", "06-qemu-resume/run.py"):
                namespace = runpy.run_path(str(DEMO_DIR / script))
                self.assertEqual(namespace["ASSETS"], Path(chosen))

    def test_make_uses_shared_default_and_preserves_override(self):
        self.assertEqual(_make_default(), dc.default_qemu_assets(DEMO_DIR.parent))
        chosen = Path("/var/tmp/operator-selected-qemu-assets")
        self.assertEqual(_make_default({"QEMU_ASSETS": str(chosen)}), chosen)

    def test_every_shell_entrypoint_uses_the_shared_resolver(self):
        for relative in ("clean.sh", "07-drgn-kernel/run.sh", "lib/qemu-assets.sh"):
            text = (DEMO_DIR / relative).read_text()
            self.assertIn("qemu-paths.sh", text)
            self.assertIn("qemu_default_assets", text)

    def test_every_hermit_qemu_launch_uses_the_tmp_mount_helper(self):
        for relative in (
            "05-qemu-boot/run.py",
            "06-qemu-resume/run.py",
            "lib/drgn_hermit.py",
        ):
            text = (DEMO_DIR / relative).read_text()
            self.assertIn("hermit_tmp_args(", text)

    def test_qemu_controllers_disable_bytecode_writes_before_launch(self):
        setting = 'environment["PYTHONDONTWRITEBYTECODE"] = "1"'
        for relative in ("05-qemu-boot/run.py", "06-qemu-resume/run.py"):
            with self.subTest(relative=relative):
                text = (DEMO_DIR / relative).read_text()
                environment_start = text.index(
                    "environment = hermit_log_environment(LOG_FILTER)"
                )
                launch = text.index("process = subprocess.Popen(", environment_start)
                self.assertIn(setting, text[environment_start:launch])
                self.assertIn("env=environment", text[launch:])


class GuestControllerStagingTest(unittest.TestCase):
    """The guest runs a private copy of the controller, never demos/lib itself.

    A bytecode cache in demos/lib records its source's absolute path, so a guest
    importing from there read a checkout-dependent file.
    """

    def test_staged_copy_has_only_the_sources_with_fixed_metadata(self):
        with tempfile.TemporaryDirectory() as parent:
            old_umask = os.umask(0o077)
            try:
                staged = dc.stage_guest_controller(Path(parent) / "controller")
            finally:
                os.umask(old_umask)
            self.assertEqual(
                sorted(entry.name for entry in staged.iterdir()),
                sorted(dc.GUEST_CONTROLLER_SOURCES),
            )
            directory_status = staged.stat()
            self.assertEqual(directory_status.st_mode & 0o7777, 0o755)
            self.assertEqual(int(directory_status.st_mtime), dc.GUEST_CONTROLLER_MTIME)
            for name in dc.GUEST_CONTROLLER_SOURCES:
                with self.subTest(name=name):
                    copy = staged / name
                    self.assertEqual(copy.read_bytes(), (LIB_DIR / name).read_bytes())
                    status = copy.stat()
                    self.assertEqual(status.st_mode & 0o7777, 0o644)
                    self.assertEqual(int(status.st_mtime), dc.GUEST_CONTROLLER_MTIME)

    def test_staging_refuses_an_existing_directory(self):
        with tempfile.TemporaryDirectory() as parent:
            with self.assertRaises(FileExistsError):
                dc.stage_guest_controller(Path(parent))

    def test_the_controller_imports_nothing_else_from_demos_lib(self):
        local_modules = {path.stem for path in LIB_DIR.glob("*.py")}
        for name in dc.GUEST_CONTROLLER_SOURCES:
            with self.subTest(name=name):
                imported = set()
                for line in (LIB_DIR / name).read_text().splitlines():
                    words = line.split()
                    if len(words) >= 2 and words[0] in ("import", "from"):
                        imported.add(words[1].split(".")[0])
                missing = (imported & local_modules) - {
                    Path(source).stem for source in dc.GUEST_CONTROLLER_SOURCES
                }
                self.assertEqual(missing, set())

    def test_qemu_demos_bind_the_staged_copy_not_demos_lib(self):
        for relative in ("05-qemu-boot/run.py", "06-qemu-resume/run.py"):
            with self.subTest(relative=relative):
                text = (DEMO_DIR / relative).read_text()
                self.assertIn('stage_guest_controller(run_dir / "controller")', text)
                self.assertIn('"{}:{}".format(controller_dir, GUEST_CONTROLLER_DIR)', text)
                self.assertIn('str(GUEST_CONTROLLER_DIR / "qemu_controller.py")', text)
                self.assertNotIn('DEMOS_DIR / "lib", GUEST', text)


class SocketPathBoundTest(unittest.TestCase):
    """A Unix-domain socket path must fit AF_UNIX, and moves only if it must.

    A path that already fits stays where it is: on some CI runners the
    relocation directory is unusable, while the in-checkout path fits.
    """

    def test_a_path_that_fits_is_returned_unchanged(self):
        fits = Path("/runner-state/_work/hermit/hermit/ignored/q/.work/boot-ab/qmp.sock")
        self.assertLessEqual(len(str(fits).encode()), dc.AF_UNIX_PATH_MAX)
        self.assertEqual(dc.make_socket_path(fits, "boot"), fits)

    def test_a_path_over_the_bound_is_relocated_within_it(self):
        too_long = Path(
            "/srv/build/workspaces/continuous-integration/"
            "checkout-27be9dc9b2a6-3837008-c9ce93ba/hermit/ignored/"
            "qemu-linux/.work/boot-ab12cd34/qmp.sock"
        )
        self.assertGreater(len(str(too_long).encode()), dc.AF_UNIX_PATH_MAX)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("QEMU_SOCKET_DIR", None)
            got = dc.make_socket_path(too_long, "boot")
        self.assertNotEqual(got, too_long)
        self.assertLessEqual(len(str(got).encode()), dc.AF_UNIX_PATH_MAX)
        self.assertEqual(
            got.parent, Path("/var/tmp/hermit-qmp-{}".format(os.getuid()))
        )
        self.assertTrue(got.name.startswith("boot-"))
        self.assertTrue(got.name.endswith(".sock"))

    def test_relocation_is_stable_bindable_and_repeat_canonicalization_is_stable(self):
        prefix = "/a" + "/deep" * 24
        first_run = Path(prefix + "/boot-first")
        second_run = Path(prefix + "/boot-second")
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("QEMU_SOCKET_DIR", None)
            first = dc.make_socket_path(first_run / "qmp.sock", "boot")
            self.assertEqual(
                dc.make_socket_path(first_run / "qmp.sock", "boot"), first
            )
            second = dc.make_socket_path(second_run / "qmp.sock", "boot")
        self.assertNotEqual(first, second)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            first.unlink(missing_ok=True)
            listener.bind(str(first))
        first.unlink(missing_ok=True)

        first_arg = "unix:{},server=on,wait=off".format(first)
        second_arg = "unix:{},server=on,wait=off".format(second)
        self.assertEqual(
            dc.canonicalize_qemu_runtime_path(first_arg, first_run, first),
            dc.canonicalize_qemu_runtime_path(second_arg, second_run, second),
        )

    def test_override_must_still_leave_room_for_the_socket(self):
        too_long = Path("/a" + "/deep" * 24 + "/qmp.sock")
        override = "/var/tmp/" + "x" * 100
        with mock.patch.dict(os.environ, {"QEMU_SOCKET_DIR": override}):
            with self.assertRaisesRegex(RuntimeError, "AF_UNIX limit"):
                dc.make_socket_path(too_long, "boot")

    def test_host_tmp_override_is_refused_because_hermit_hides_it(self):
        too_long = Path("/a" + "/deep" * 24 + "/qmp.sock")
        with mock.patch.dict(os.environ, {"QEMU_SOCKET_DIR": "/tmp/qmp"}):
            with self.assertRaisesRegex(RuntimeError, "Hermit normally hides"):
                dc.make_socket_path(too_long, "boot")

    def test_the_bound_is_the_kernel_constant(self):
        # sockaddr_un.sun_path is 108 bytes including the NUL.
        self.assertEqual(dc.AF_UNIX_PATH_MAX, 107)


if __name__ == "__main__":
    unittest.main()
