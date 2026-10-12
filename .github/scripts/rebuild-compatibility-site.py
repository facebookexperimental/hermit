#!/usr/bin/env python3
"""GitHub Actions adapter for the maintained, source-pinned website builder.

Python keeps this adapter on the existing builder/publisher API. It contains no
cell reducer, comparison policy, measurement producer, or second history store.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import resource
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPOSITORY = "rrnewton/hermit"
LEDGER = "https://github.com/rrnewton/hermit_test_ledger.git"
PUBLIC = "https://rrnewton.github.io/hermit/compatibility/latest/"
PUBLIC_FILES = (
    "build.json",
    "index.html",
    "assets/site.css",
    "assets/site.js",
    "data/site-manifest.json.gz",
    "data/site-summary.json.gz",
)
ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / ".github/scripts/publish-compatibility-site.py"
CONFIG = ROOT / ".github/compatibility-site-builder.json"
CHECKED_IN = ROOT / ".github/compatibility-site-releases.json"


def run(argv, *, cwd=None, timeout=120):
    return subprocess.run(
        list(map(str, argv)),
        cwd=cwd,
        check=True,
        stdout=subprocess.PIPE,
        timeout=timeout,
    ).stdout


def git(root, *args):
    return run(["git", "--no-replace-objects", "-C", root, *args]).decode().strip()


def gh(*args):
    return run(["gh", *args])


def canonical(value):
    return (
        json.dumps(
            value, sort_keys=True, separators=(",", ":"), allow_nan=False
        ).encode()
        + b"\n"
    )


def exact_commit(value, label):
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError(
            f"{label} needs the captured full 40-hex commit; rerun the workflow capture steps"
        )
    return value


def capture_ledger():
    """Capture public data before the authenticated private-source checkout."""
    try:
        rows = (
            run(["git", "ls-remote", "--exit-code", LEDGER, "refs/heads/main"])
            .decode()
            .splitlines()
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ValueError(
            "cannot capture public ledger main; install Git or repair GitHub access, then rerun the workflow"
        ) from error
    if (
        len(rows) != 1
        or len(rows[0].split()) != 2
        or rows[0].split()[1] != "refs/heads/main"
    ):
        raise ValueError(
            "cell-ledger main did not resolve uniquely; retry the workflow after checking its repository"
        )
    return {
        "repository": LEDGER,
        "commit": exact_commit(rows[0].split()[0], "cell ledger"),
    }


def runner_resources():
    """Observed host capacity and per-process peaks, not a capacity guarantee."""
    memory = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        name, value = line.split(":", 1)
        if name in {"MemTotal", "MemAvailable"}:
            memory[name + "_bytes"] = int(value.split()[0]) * 1024
    cgroup = {}
    for line in Path("/proc/self/cgroup").read_text().splitlines():
        if line.startswith("0::"):
            relative = Path(line[3:].lstrip("/"))
            if ".." not in relative.parts:
                root = Path("/sys/fs/cgroup") / relative
                for name in ("cpu.max", "memory.max", "memory.peak"):
                    try:
                        cgroup[name] = (root / name).read_text().strip()
                    except OSError:
                        cgroup[name] = None
    return {
        "cpu_affinity_count": len(os.sched_getaffinity(0)),
        **memory,
        "enclosing_cgroup": cgroup,
        "cgroup_peak_scope": "enclosing cgroup lifetime, may include other job steps",
        "adapter_peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        * 1024,
        "largest_child_peak_rss_bytes": resource.getrusage(
            resource.RUSAGE_CHILDREN
        ).ru_maxrss
        * 1024,
        "peak_scope": "RSS fields are the largest individual process, not aggregate job or cgroup memory",
    }


def configuration(path=CONFIG):
    value = json.loads(path.read_bytes())
    if (
        not isinstance(value, dict)
        or set(value)
        != {
            "schema",
            "parent_repository",
            "parent_commit",
            "hermit_commit",
            "retention",
        }
        or value["schema"] != 2
        or value["parent_repository"] != "rrnewton/dev-hermit"
        or any(
            not isinstance(value[key], str)
            or not re.fullmatch(r"[0-9a-f]{40}", value[key])
            for key in ("parent_commit", "hermit_commit")
        )
    ):
        raise ValueError(
            "website builder requires exact reviewed parent and Hermit source commits"
        )
    # The publisher applies the retention rule, so ask it whether it accepts
    # this block before an expensive rebuild reaches publication. One function
    # (load_retention in the publisher) decides; this adapter keeps no copy.
    checked = subprocess.run(
        [sys.executable, str(HELPER), "check-retention", str(path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=60,
    )
    if checked.returncode != 0:
        reason = checked.stderr.strip().splitlines() or ["no reason given"]
        raise ValueError(
            "website builder retention settings are refused by the publisher: "
            + reason[-1]
        )
    return value


def load_module(name, path):
    specification = importlib.util.spec_from_file_location(name, path)
    if specification is None or specification.loader is None:
        raise ValueError(f"cannot load maintained website module {path.name}")
    module = importlib.util.module_from_spec(specification)
    sys.modules[name] = module
    specification.loader.exec_module(module)
    return module


def require_sources(parent, config):
    hermit = parent / "hermit"
    if git(parent, "rev-parse", "HEAD^{commit}") != config["parent_commit"]:
        raise ValueError("parent builder checkout differs from the reviewed source pin")
    if git(hermit, "rev-parse", "HEAD^{commit}") != config["hermit_commit"]:
        raise ValueError(
            "Hermit metadata checkout differs from the reviewed source pin"
        )
    recorded = git(parent, "rev-parse", "HEAD:hermit")
    if recorded != config["hermit_commit"]:
        raise ValueError(
            "parent source's Hermit gitlink differs from the reviewed source pin"
        )
    for root in (parent, hermit):
        if git(
            root,
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ):
            raise ValueError("website builder sources have local changes")
    ledger = parent / "hermit_test_ledger"
    if git(ledger, "config", "--get", "remote.origin.url") != LEDGER:
        raise ValueError("cell ledger origin is not the existing canonical repository")
    return ledger


# The builder reads the cell ledger at one commit (ls-tree and cat-file of that
# tree) plus, for the parity panel, the commit its scorecard/parity.json
# summarized and that commit's ancestry. It never walks older history, and the
# full ledger history no longer fits the workflow's clone budget: on
# https://github.com/rrnewton/hermit/actions/runs/38167459473 the full-history
# submodule clone ran past its 15-minute step timeout (1,358 commits, 44 GB as
# a local repository on 2026-10-11, 121 commits that day). So the ledger is
# fetched at depth 1 and deepened only until the summarized commit is an
# ancestor, a few commits in practice.
# 2,700 s since 2026-10-12. The depth-1 fetch of the published commit was
# done in the workflow's prepare step until hermit afa00a30, and took 527,
# 592 and 793 s there on the runs that finished (it then timed out at 900 s on
# three runs); fetched here it ran past the old 1,200 s on
# https://github.com/rrnewton/hermit/actions/runs/38256949845. The fetch moves
# about 711 MiB at a GitHub serving rate near 1 MB/s; its bytes shrink by the
# scorecard history parts (265.9 MiB of the pack at ledger f8361f72) once
# those leave the ledger's main branch (dev-hermit option (a),
# https://github.com/rrnewton/dev-hermit/issues/558). The rebuild step's own
# 101-minute bound still holds the fetch plus the 36-42 minute build.
LEDGER_FETCH_TIMEOUT = 2700
LEDGER_DEEPEN_STEP = 50
LEDGER_DEEPEN_LIMIT = 2000


def _parity_summary_commit(ledger, ledger_commit):
    """The series_commit scorecard/parity.json at ledger_commit names, if any."""
    try:
        text = run(
            ["git", "--no-replace-objects", "-C", ledger, "show",
             f"{ledger_commit}:scorecard/parity.json"]
        )
        value = json.loads(text)["generated_from"]["series_commit"]
    except (subprocess.CalledProcessError, ValueError, KeyError, TypeError):
        return None  # the builder reports a missing or malformed summary itself
    return value if isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) else None


def _is_ancestor(ledger, ancestor, descendant):
    result = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(ledger), "merge-base",
         "--is-ancestor", ancestor, descendant],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=120,
    )
    return result.returncode == 0


def fetch_ledger(ledger, ledger_commit):
    """Fetch ledger_commit and only the history the builder reads."""
    fetch = ["git", "-C", ledger, "fetch", "--no-tags", "--no-recurse-submodules"]
    depth = 1
    run([*fetch, "--depth", str(depth), "origin", ledger_commit], timeout=LEDGER_FETCH_TIMEOUT)
    summarized = _parity_summary_commit(ledger, ledger_commit)
    while (
        summarized not in (None, ledger_commit)
        and not _is_ancestor(ledger, summarized, ledger_commit)
        and depth < LEDGER_DEEPEN_LIMIT
    ):
        # Past the limit the builder still refuses only the parity panel,
        # naming the summarized commit it could not place.
        depth += LEDGER_DEEPEN_STEP
        run([*fetch, "--depth", str(depth), "origin", ledger_commit], timeout=LEDGER_FETCH_TIMEOUT)
    return depth


def published_registry(scratch):
    pages = json.loads(gh("api", f"repos/{REPOSITORY}/git/ref/heads/gh-pages"))[
        "object"
    ]["sha"]
    entries = json.loads(
        gh("api", f"repos/{REPOSITORY}/contents/compatibility?ref={pages}")
    )
    previous = scratch / "previous-registry.json"
    if any(
        item["name"] == "releases.json" and item["type"] == "file" for item in entries
    ):
        previous.write_bytes(
            gh(
                "api",
                "-H",
                "Accept: application/vnd.github.raw+json",
                f"repos/{REPOSITORY}/contents/compatibility/releases.json?ref={pages}",
            )
        )
    else:
        baseline = json.loads(CHECKED_IN.read_bytes())
        expected = {"latest", *(pin["identity"] for pin in baseline["releases"])}
        if {entry["name"] for entry in entries} != expected:
            raise ValueError(
                "checked-in registry does not account for existing public history"
            )
        previous.write_bytes(CHECKED_IN.read_bytes())
    selected = run(
        [sys.executable, HELPER, "select-registry", CHECKED_IN, ROOT, previous]
    )
    return json.loads(selected), previous


def updated_registry(previous, pin):
    result = json.loads(json.dumps(previous))
    if result["release_repository"] != REPOSITORY:
        raise ValueError(
            "website release repository differs from the workflow repository"
        )
    retained = {item["identity"]: item for item in result["releases"]}
    if pin["identity"] in retained and retained[pin["identity"]] != pin:
        raise ValueError(
            "an immutable website identity already has different published bytes"
        )
    retained[pin["identity"]] = pin
    result.update(
        latest_identity=pin["identity"],
        releases=[retained[key] for key in sorted(retained)],
    )
    return result


def release_by_tag(tag):
    result = subprocess.run(
        ["gh", "api", "--include", f"repos/{REPOSITORY}/releases/tags/{tag}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120,
    )
    header, separator, body = result.stdout.replace(b"\r\n", b"\n").partition(b"\n\n")
    status = header.splitlines()[0].split()[1:2] if header else []
    if separator and status == [b"404"] and result.returncode:
        return None
    if result.returncode or not separator or status != [b"200"]:
        raise ValueError(
            "release lookup failed; an unreadable release is not an absent release"
        )
    return json.loads(body)


def asset_id(release, pin, name, sha, size):
    if (
        not isinstance(release, dict)
        or release.get("tag_name") != pin["tag"]
        or release.get("name") != pin["release_title"]
        or release.get("draft") is not False
        or release.get("prerelease") is not False
    ):
        raise ValueError(
            "release identity or stable publication state differs from its pin"
        )
    assets = [item for item in release.get("assets", []) if item["name"] == name]
    if (
        len(assets) != 1
        or assets[0].get("state") != "uploaded"
        or assets[0].get("size") != size
        or assets[0].get("digest") != "sha256:" + sha
        or type(assets[0].get("id")) is not int
    ):
        raise ValueError(
            "release asset bytes or identity differ from the validated build"
        )
    return assets[0]["id"]


def publish_release(pin, archive, registry, ledger_commit, hermit_commit):
    """Create stable data assets without replacing any existing published bytes."""
    tag = pin["tag"]
    registry_bytes = registry.read_bytes()
    registry_sha = hashlib.sha256(registry_bytes).hexdigest()
    release = release_by_tag(tag)
    if release is None:
        gh(
            "release",
            "create",
            tag,
            "--repo",
            REPOSITORY,
            "--target",
            hermit_commit,
            "--latest=false",
            "--title",
            pin["release_title"],
            "--notes",
            "Website rebuilt from existing cell ledger "
            + ledger_commit
            + ". This rebuild did not run Hermit tests or refresh measurement dates.",
            archive,
            registry,
        )
        release = release_by_tag(tag)
    asset_id(release, pin, archive.name, pin["archive_sha256"], pin["archive_bytes"])
    if not any(item["name"] == registry.name for item in release["assets"]):
        gh("release", "upload", tag, registry, "--repo", REPOSITORY)
        release = release_by_tag(tag)
    return asset_id(release, pin, registry.name, registry_sha, len(registry_bytes))


def rebuild(parent, scratch, receipt, *, ledger_commit, validation_history_commit):
    config = configuration()
    ledger_commit = exact_commit(ledger_commit, "cell ledger")
    validation_history_commit = exact_commit(
        validation_history_commit, "parent validation history"
    )
    parent, scratch = parent.resolve(), scratch.resolve()
    scratch.mkdir(mode=0o700, parents=True, exist_ok=False)
    if receipt.exists():
        raise ValueError("build receipt already exists; use a fresh run path")
    ledger = require_sources(parent, config)
    if (
        git(parent, "rev-parse", "refs/remotes/origin/main^{commit}")
        != validation_history_commit
    ):
        raise ValueError(
            "parent history differs from the captured checkout main; rerun the workflow capture steps"
        )
    # Fetch objects/ref only: moving the ledger submodule HEAD would dirty the
    # immutable parent reader. Capture one commit for every subsequent read.
    fetch_ledger(ledger, ledger_commit)
    if git(ledger, "rev-parse", ledger_commit + "^{commit}") != ledger_commit:
        raise ValueError(
            "fetched cell ledger differs from its captured commit; rerun the workflow capture steps"
        )
    builder = load_module(
        "github_compatibility_builder",
        parent / "ci-hub/compatibility-website/build_and_serve.py",
    )
    archive_helper = load_module(
        "github_compatibility_archive",
        parent / "ci-hub/compatibility-website/nightly_publication.py",
    )
    prior, previous_file = published_registry(scratch)
    website = builder.build(
        scratch / "builds",
        series_ref=ledger_commit,
        published_only=True,
        state_root=parent,
        validation_history_commit=validation_history_commit,
    )
    try:
        website.recheck()
        provenance = website.manifest["provenance"]["canonical_input_provenance"]
        if (
            provenance.get("series_commit") != ledger_commit
            or provenance.get("series_repository") != LEDGER
            or provenance.get("published_only") is not True
            or provenance.get("parent_commit") != validation_history_commit
            or provenance.get("reader_commit") != config["parent_commit"]
        ):
            raise ValueError(
                "website source/data bindings differ from captured inputs; keep the prior deployment and inspect the builder receipt"
            )
        pin = json.loads(run([sys.executable, HELPER, "describe", website.path]))
        tag = "compatibility-website-" + pin["identity"]
        archive = scratch / (tag + ".tar.gz")
        archive_sha, archive_size = archive_helper.make_archive(website.path, archive)
        pin.update(
            tag=tag,
            asset=archive.name,
            release_title="Compatibility website " + pin["identity"],
            archive_sha256=archive_sha,
            archive_bytes=archive_size,
            archive_member_count=pin["file_count"] + len(pin["directories"]) + 1,
        )
        registry_bytes = canonical(updated_registry(prior, pin))
        registry_sha = hashlib.sha256(registry_bytes).hexdigest()
        registry = scratch / ("registry-" + registry_sha + ".json")
        registry.write_bytes(registry_bytes)
        run(
            [
                sys.executable,
                HELPER,
                "validate-update",
                registry,
                CHECKED_IN,
                ROOT,
                previous_file,
            ]
        )
        require_sources(parent, config)
        website.recheck()
        registry_asset = publish_release(
            pin, archive, registry, ledger_commit, config["hermit_commit"]
        )
        result = {
            "state": "built-and-released",
            "deployed": False,
            "website": str(website.path),
            "identity": pin["identity"],
            "ledger_repository": LEDGER,
            "ledger_commit": ledger_commit,
            "builder": config,
            "validation_history_commit": provenance["parent_commit"],
            "measurement_dates_changed": False,
            "tests_executed": False,
            "registry_asset": registry_asset,
            "registry_sha256": registry_sha,
        }
        receipt.write_bytes(canonical(result))
        return result
    finally:
        website.close()


def verify_public(receipt, *, timeout=600):
    result = json.loads(receipt.read_bytes())
    root = Path(result["website"])
    expected = {name: (root / name).read_bytes() for name in PUBLIC_FILES}
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            for name, content in expected.items():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ValueError("public byte verification deadline expired")
                with urllib.request.urlopen(
                    PUBLIC + name, timeout=min(30, remaining)
                ) as response:
                    if response.read(len(content) + 1) != content:
                        raise ValueError(
                            "public bytes still differ from the built website"
                        )
            result.update(
                state="deployed-and-verified",
                deployed=True,
                public_url=PUBLIC,
                public_files_sha256={
                    name: hashlib.sha256(content).hexdigest()
                    for name, content in expected.items()
                },
            )
            receipt.write_bytes(canonical(result))
            return result
        except (OSError, ValueError):
            time.sleep(min(15, max(0, deadline - time.monotonic())))
    raise ValueError(
        "Pages did not serve this exact rebuilt identity within ten minutes"
    )


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Rebuild the compatibility website from published cell-ledger evidence, "
        "or verify its public deployment. Does not run Hermit tests.",
        allow_abbrev=False,
    )
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser(
        "source-config",
        allow_abbrev=False,
        help="print the exact reviewed builder checkout configuration",
    )
    commands.add_parser(
        "capture-ledger",
        allow_abbrev=False,
        help="resolve public ledger main before the private source checkout",
    )
    build = commands.add_parser(
        "rebuild",
        allow_abbrev=False,
        help="rebuild from published ledger bytes and release the verified artifact",
    )
    build.add_argument("--builder-parent", type=Path, required=True)
    build.add_argument("--scratch", type=Path, required=True)
    build.add_argument("--receipt", type=Path, required=True)
    build.add_argument(
        "--ledger-commit",
        required=True,
        help="full public ledger SHA captured before the private source checkout",
    )
    build.add_argument(
        "--validation-history-commit",
        required=True,
        help="full origin/main SHA from the later full-history private checkout",
    )
    verify = commands.add_parser(
        "verify-public",
        allow_abbrev=False,
        help="require ordinary public URLs to serve the exact rebuilt bytes",
    )
    verify.add_argument("--receipt", type=Path, required=True)
    arguments = parser.parse_args(argv)
    try:
        if arguments.command == "source-config":
            result = configuration()
        elif arguments.command == "capture-ledger":
            result = capture_ledger()
        elif arguments.command == "rebuild":
            resource_path = arguments.receipt.with_suffix(".resources.json")
            if resource_path.exists():
                raise ValueError(
                    "resource receipt already exists; use a fresh receipt path"
                )
            started = time.monotonic()
            resources = {"before": runner_resources()}
            try:
                result = rebuild(
                    arguments.builder_parent,
                    arguments.scratch,
                    arguments.receipt,
                    ledger_commit=arguments.ledger_commit,
                    validation_history_commit=arguments.validation_history_commit,
                )
            finally:
                resources.update(
                    after=runner_resources(), elapsed_seconds=time.monotonic() - started
                )
                with resource_path.open("xb") as output:
                    output.write(canonical(resources))
        else:
            result = verify_public(arguments.receipt)
        print(canonical(result).decode(), end="")
        return 0
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"compatibility website: REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
