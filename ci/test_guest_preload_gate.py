#!/usr/bin/env python3
"""Real-ELF tests for the guest-preload gate in the E2E artifact scripts.

ci/publish-hermit-e2e-artifact.sh and ci/verify-hermit-e2e-artifact.sh each carry
require_portable_guest_preload, which refuses a shipped guest preload that a guest's own
loader could not load (libdetcore_sabre.so and libdetcore_liteinst.so, which Hermit
preloads, and libreverie_liteinst.so, which Hermit ships but has not preloaded since
2d8bada7e9 and Reverie's own tools preload): one that records an RPATH or RUNPATH,
needs a library glibc does not provide, or requires a glibc symbol version newer than
GUEST_PRELOAD_GLIBC_MINOR_FLOOR
(https://github.com/rrnewton/hermit/issues/3652,
https://github.com/rrnewton/hermit/issues/3967,
https://github.com/rrnewton/reverie/issues/980). The publication fixtures in
scripts/validate.rs use shell scripts in place of the libraries, which the gate skips
as non-ELF, so they never reach these checks.

Each case here compiles a tiny shared object with the C compiler, without the C library
(-nostdlib), so its dynamic section holds only what the case puts there. A newer glibc
symbol version comes from a stub libc.so.6 whose version script defines it, and a
non-glibc dependency from a stub libgcc_s.so.1, so the cases do not depend on the glibc
or gcc of the machine running them. Every fixture is first checked with readelf to have
exactly the shape the case means (a gate that accepts a fixture missing its defect
would prove nothing). The gate function is then run, as it is written in each of the
two scripts, on the fixture: the clean object and one requiring exactly GLIBC_2.<floor>
are accepted; a RUNPATH, an RPATH, a libgcc_s.so.1 dependency, a requirement one minor
version above the floor and a requirement of GLIBC_ABI_DT_RELR (a version name outside
GLIBC_2.<n>, which only a newer glibc defines) are each refused with exit status 2 and
the gate's message for that defect.

GuestPreloadCallSiteTest checks that the scripts apply the gate to every shipped guest
preload. It publishes a bundle with the real publish script, using the script stand-ins
of scripts/validate.rs's publication fixtures, and checks that the real verify script
accepts it. Then, for each of libdetcore_sabre.so, libdetcore_liteinst.so and
libreverie_liteinst.so: publishing with that file replaced by the RUNPATH fixture is
refused by the publish script's own gate on the source install, naming the file (the
verify script that publish runs on its staged bundle would also refuse, so the message's
script name is checked); and replacing that file in the published bundle makes verify
refuse it at the gate, before its manifest check could.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = (
    REPO / "ci" / "publish-hermit-e2e-artifact.sh",
    REPO / "ci" / "verify-hermit-e2e-artifact.sh",
)
LABEL = "Fixture preload"


def gate_source(script: Path) -> str:
    """The floor assignment and require_portable_guest_preload, as SCRIPT has them."""
    text = script.read_text()
    floor = re.search(r"^GUEST_PRELOAD_GLIBC_MINOR_FLOOR=\d+$", text, re.MULTILINE)
    function = re.search(
        r"^function require_portable_guest_preload \{\n.*?^\}$", text, re.MULTILINE | re.DOTALL
    )
    if floor is None or function is None:
        raise AssertionError(f"{script} has no guest-preload gate to test")
    return f"{floor.group(0)}\n{function.group(0)}\n"


def floor_minor() -> int:
    minors = set()
    for script in SCRIPTS:
        match = re.search(r"^GUEST_PRELOAD_GLIBC_MINOR_FLOOR=(\d+)$", script.read_text(), re.MULTILINE)
        assert match is not None, script
        minors.add(int(match.group(1)))
    assert len(minors) == 1, f"the two scripts disagree on the floor: {sorted(minors)}"
    return minors.pop()


def run_gate(script: Path, preload: Path) -> subprocess.CompletedProcess[str]:
    program = (
        'function fail { echo "gate: $*" >&2; exit 2; }\n'
        + gate_source(script)
        + 'require_portable_guest_preload "$1" "$2"\n'
    )
    return subprocess.run(
        ["bash", "-c", program, "gate", LABEL, str(preload)],
        capture_output=True,
        text=True,
        check=False,
    )


def readelf(*args: str) -> str:
    return subprocess.run(["readelf", *args], capture_output=True, text=True, check=True).stdout


class GuestPreloadGateTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        for tool in ("cc", "readelf", "bash"):
            if shutil.which(tool) is None:
                raise AssertionError(f"the guest-preload gate test needs {tool} on PATH")
        cls.floor = floor_minor()
        cls.scratch = tempfile.TemporaryDirectory()
        cls.dir = Path(cls.scratch.name)
        # The Nix compiler wrapper of the pinned root records a RUNPATH for every
        # directory it links a library from. These names turn that off for the
        # wrapper's target; every other compiler ignores them.
        cls.env = dict(os.environ)
        cls.env["NIX_DONT_SET_RPATH"] = "1"
        cls.env["NIX_DONT_SET_RPATH_FOR_TARGET"] = "1"
        cls.env["NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu"] = "1"
        (cls.dir / "body.c").write_text("int fixture_preload(void) { return 0; }\n")
        (cls.dir / "caller.c").write_text(
            "extern int fixture_versioned(void);\n"
            "int fixture_preload(void) { return fixture_versioned(); }\n"
        )
        (cls.dir / "stub.c").write_text("int fixture_versioned(void) { return 0; }\n")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.scratch.cleanup()

    def cc(self, output: str, *args: str) -> Path:
        path = self.dir / output
        path.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["cc", "-shared", "-fPIC", "-nostdlib", "-o", str(path), *args],
            cwd=self.dir,
            env=self.env,
            check=True,
        )
        return path

    def stub(self, soname: str, version: str | None) -> Path:
        """A stub shared library named SONAME defining fixture_versioned at VERSION."""
        directory = self.dir / f"stub-{soname}-{version}"
        args = [f"-Wl,-soname,{soname}"]
        if version is not None:
            script = self.dir / f"{soname}-{version}.map"
            script.write_text(f"{version} {{ global: fixture_versioned; local: *; }};\n")
            args.append(f"-Wl,--version-script={script}")
        self.cc(str(directory.relative_to(self.dir) / soname), *args, "stub.c")
        return directory

    def linked_against(self, output: str, soname: str, version: str | None) -> Path:
        directory = self.stub(soname, version)
        return self.cc(output, "caller.c", f"-L{directory}", f"-l:{soname}")

    def assert_accepted(self, preload: Path) -> None:
        for script in SCRIPTS:
            result = run_gate(script, preload)
            self.assertEqual(
                result.returncode, 0, f"{script.name} refused {preload.name}: {result.stderr}"
            )

    def assert_refused(self, preload: Path, message: str) -> None:
        for script in SCRIPTS:
            result = run_gate(script, preload)
            self.assertEqual(
                result.returncode,
                2,
                f"{script.name} did not refuse {preload.name}: {result.stdout}{result.stderr}",
            )
            self.assertIn(f"gate: {LABEL} {message}", result.stderr, script.name)

    def test_a_clean_preload_is_accepted(self) -> None:
        preload = self.cc("clean.so", "body.c")
        dynamic = readelf("-d", str(preload))
        self.assertNotRegex(dynamic, r"\((RPATH|RUNPATH)\)")
        self.assertNotIn("(NEEDED)", dynamic)
        self.assert_accepted(preload)

    def test_a_runpath_is_refused(self) -> None:
        preload = self.cc(
            "runpath.so", "body.c", "-Wl,--enable-new-dtags", "-Wl,-rpath,/nix/store/fixture-glibc/lib"
        )
        self.assertIn("(RUNPATH)", readelf("-d", str(preload)))
        self.assert_refused(preload, "records a library search path")

    def test_an_rpath_is_refused(self) -> None:
        preload = self.cc(
            "rpath.so", "body.c", "-Wl,--disable-new-dtags", "-Wl,-rpath,/nix/store/fixture-glibc/lib"
        )
        dynamic = readelf("-d", str(preload))
        self.assertIn("(RPATH)", dynamic)
        self.assertNotIn("(RUNPATH)", dynamic)
        self.assert_refused(preload, "records a library search path")

    def test_a_library_outside_glibc_is_refused(self) -> None:
        preload = self.linked_against("gcc-s.so", "libgcc_s.so.1", None)
        dynamic = readelf("-d", str(preload))
        self.assertIn("Shared library: [libgcc_s.so.1]", dynamic)
        self.assertNotRegex(dynamic, r"\((RPATH|RUNPATH)\)")
        self.assert_refused(preload, "needs libgcc_s.so.1, which not every guest's glibc provides")

    def test_the_floor_version_is_accepted(self) -> None:
        version = f"GLIBC_2.{self.floor}"
        preload = self.linked_against("floor.so", "libc.so.6", version)
        needs = readelf("-V", "--wide", str(preload))
        self.assertIn(f"Name: {version} ", needs)
        self.assertNotRegex(readelf("-d", str(preload)), r"\((RPATH|RUNPATH)\)")
        self.assert_accepted(preload)

    def test_a_version_above_the_floor_is_refused(self) -> None:
        version = f"GLIBC_2.{self.floor + 1}"
        preload = self.linked_against("newer.so", "libc.so.6", version)
        needs = readelf("-V", "--wide", str(preload))
        self.assertIn(f"Name: {version} ", needs)
        self.assertNotRegex(readelf("-d", str(preload)), r"\((RPATH|RUNPATH)\)")
        self.assert_refused(preload, f"requires symbol version {version}")

    def test_a_version_name_outside_glibc_2_n_is_refused(self) -> None:
        # The exact need that broke libdetcore_liteinst.so for host guests
        # (https://github.com/rrnewton/hermit/issues/3967).
        version = "GLIBC_ABI_DT_RELR"
        preload = self.linked_against("relr.so", "libc.so.6", version)
        needs = readelf("-V", "--wide", str(preload))
        self.assertIn(f"Name: {version} ", needs)
        self.assertNotRegex(readelf("-d", str(preload)), r"\((RPATH|RUNPATH)\)")
        self.assert_refused(preload, f"requires symbol version {version}")


# The shipped guest preloads and the label each script's gate gives them.
GUEST_PRELOADS = {
    "libdetcore_sabre.so": "SaBRe plugin",
    "libdetcore_liteinst.so": "In-guest LiteInst runtime",
    "libreverie_liteinst.so": "LiteInst preload",
}
# Every file the scripts require in a complete bundle's rsrcs/.
BUNDLE_RESOURCES = (
    "libdetcore_dbt.so",
    "libdetcore_sabre.so",
    "libdetcore_liteinst.so",
    "libreverie_dbt_client.so",
    "libreverie_liteinst.so",
    "dynamorio/bin64/drrun",
    "sabre",
    "e9patch",
    "e9tool",
)


class GuestPreloadCallSiteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        for tool in ("cc", "readelf", "bash"):
            if shutil.which(tool) is None:
                raise AssertionError(f"the guest-preload gate test needs {tool} on PATH")
        cls.scratch = tempfile.TemporaryDirectory()
        cls.dir = Path(cls.scratch.name)
        (cls.dir / "ci").mkdir()
        for name in (
            "publish-hermit-e2e-artifact.sh",
            "verify-hermit-e2e-artifact.sh",
            "run-with-hermit-e2e-artifact.sh",
        ):
            shutil.copy2(REPO / "ci" / name, cls.dir / "ci" / name)
        env = dict(os.environ)
        env["NIX_DONT_SET_RPATH"] = "1"
        env["NIX_DONT_SET_RPATH_FOR_TARGET"] = "1"
        env["NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu"] = "1"
        (cls.dir / "body.c").write_text("int fixture_preload(void) { return 0; }\n")
        cls.runpath = cls.dir / "runpath.so"
        subprocess.run(
            [
                "cc", "-shared", "-fPIC", "-nostdlib", "-o", str(cls.runpath), "body.c",
                "-Wl,--enable-new-dtags", "-Wl,-rpath,/nix/store/fixture-glibc/lib",
            ],
            cwd=cls.dir,
            env=env,
            check=True,
        )
        assert "(RUNPATH)" in readelf("-d", str(cls.runpath))
        cls.binary = cls.dir / "fixture-hermit"
        cls.binary.write_text("#!/bin/sh\n# guest-preload call-site fixture\nexit 0\n")
        cls.binary.chmod(0o700)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.scratch.cleanup()

    def install(self, name: str, bad: str | None = None) -> Path:
        """An install tree of script stand-ins, with BAD replaced by the RUNPATH fixture."""
        install = self.dir / name
        for resource in BUNDLE_RESOURCES:
            path = install / "rsrcs" / resource
            path.parent.mkdir(parents=True, exist_ok=True)
            if resource == bad:
                shutil.copyfile(self.runpath, path)
            else:
                path.write_text(f"#!/bin/sh\n# {name} {resource}\nexit 0\n")
            path.chmod(0o700)
        return install

    def publish(self, name: str, install: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                str(self.dir / "ci" / "publish-hermit-e2e-artifact.sh"),
                str(self.binary),
                str(self.dir / name / "artifacts"),
                str(self.dir / name / "artifact.path"),
                str(install),
            ],
            capture_output=True,
            text=True,
            check=False,
        )

    def verify(self, target: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.dir / "ci" / "verify-hermit-e2e-artifact.sh"), str(target)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_publish_refuses_each_guest_preload_with_a_runpath(self) -> None:
        for preload, label in GUEST_PRELOADS.items():
            with self.subTest(preload=preload):
                result = self.publish(f"publish-{preload}", self.install(f"install-{preload}", preload))
                self.assertEqual(result.returncode, 2, result.stderr)
                # The publish script's own gate, on the source install: the
                # verify script it runs on the staged bundle would refuse too,
                # and must not be what this case measures.
                self.assertIn(
                    f"publish-hermit-e2e-artifact.sh: {label} records a library search path",
                    result.stderr,
                )
                self.assertIn(f"/install-{preload}/rsrcs/{preload}", result.stderr)

    def test_verify_refuses_each_guest_preload_with_a_runpath(self) -> None:
        published = self.publish("clean", self.install("install-clean"))
        self.assertEqual(published.returncode, 0, published.stderr)
        pointer = self.dir / "clean" / "artifact.path"
        accepted = self.verify(pointer)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        bundle = Path(pointer.read_text().strip())
        for preload, label in GUEST_PRELOADS.items():
            with self.subTest(preload=preload):
                path = bundle / "install" / "rsrcs" / preload
                original = path.read_bytes()
                mode = path.stat().st_mode
                path.chmod(0o700)
                path.write_bytes(self.runpath.read_bytes())
                try:
                    result = self.verify(pointer)
                finally:
                    path.write_bytes(original)
                    path.chmod(mode)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(
                    f"verify-hermit-e2e-artifact.sh: {label} records a library search path",
                    result.stderr,
                )
                self.assertIn(f"/rsrcs/{preload}", result.stderr)
        self.assertEqual(self.verify(pointer).returncode, 0, "the restored bundle verifies again")


if __name__ == "__main__":
    unittest.main()
