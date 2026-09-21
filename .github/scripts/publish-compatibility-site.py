#!/usr/bin/env python3
"""Validate and assemble release-pinned compatibility-site publications.

This standard-library Python is a semantic-preserving extraction of the
security-sensitive publisher previously embedded in docs.yml. A Rust port
would be a separate behavior change and is deliberately not part of this fix.
"""

import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
from pathlib import Path, PurePosixPath


def refuse(message: str) -> None:
    raise SystemExit(f"compatibility website refused: {message}")


PIN_FIELDS = {
    "archive_bytes",
    "archive_member_count",
    "archive_sha256",
    "artifacts_sha256",
    "asset",
    "build_sha256",
    "content_tree_sha256",
    "counts",
    "directories",
    "file_bytes",
    "file_count",
    "identity",
    "manifest_tree_sha256",
    "mode_sha256",
    "recursive_identity_sha256",
    "release_title",
    "tag",
}
HEX_FIELDS = {
    "archive_sha256",
    "artifacts_sha256",
    "build_sha256",
    "content_tree_sha256",
    "identity",
    "manifest_tree_sha256",
    "mode_sha256",
    "recursive_identity_sha256",
}
BOOTSTRAP_PIN_SHA256 = {
    "8feef179bed2bb48c81dd0bc8186d81df47c255d8c015dc4b0eb139eab439edc":
        "eb75c9063ed5e55c26037d6e3c6d29cf4ece47d4664966462b4bb690427c3db3",
}
RELEASE_REPOSITORY_PATTERN = re.compile(
    r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$"
)
DIRECTORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+$")


def is_hex_digest(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def reject_duplicate_object(pairs: list[tuple[str, object]]) -> dict:
    value = {}
    for key, member in pairs:
        if key in value:
            refuse(f"release-pin registry contains duplicate object key {key!r}")
        value[key] = member
    return value


def load_registry_bytes(
    payload: bytes,
    source: str,
    require_plain_latest_title: bool = True,
) -> dict:
    try:
        registry = json.loads(payload, object_pairs_hook=reject_duplicate_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"cannot parse release-pin registry {source}: {error}")
    if not isinstance(registry, dict) or set(registry) != {
        "latest_identity",
        "release_repository",
        "releases",
        "schema_version",
    }:
        refuse("release-pin registry fields are not exact")
    if registry["schema_version"] != 1:
        refuse("release-pin registry schema is unsupported")
    repository = registry["release_repository"]
    if (
        not isinstance(repository, str)
        or RELEASE_REPOSITORY_PATTERN.fullmatch(repository) is None
    ):
        refuse("release repository is invalid")
    latest_identity = registry["latest_identity"]
    if not is_hex_digest(latest_identity):
        refuse("latest identity is not a lowercase SHA-256 digest")
    releases = registry["releases"]
    if not isinstance(releases, list) or not releases:
        refuse("release-pin registry has no releases")

    for pin in releases:
        if not isinstance(pin, dict) or set(pin) != PIN_FIELDS:
            refuse("release pin fields are not exact")
        if any(not is_hex_digest(pin[field]) for field in HEX_FIELDS):
            refuse("release pin contains an invalid digest")
        identity = pin["identity"]
        if pin["tag"] != f"compatibility-website-{identity}":
            refuse("release tag disagrees with its identity")
        if pin["asset"] != f"{pin['tag']}.tar.gz":
            refuse("release asset name disagrees with its tag")
        if not isinstance(pin["release_title"], str) or not pin["release_title"]:
            refuse("release title is empty")
        for field in (
            "archive_bytes",
            "archive_member_count",
            "file_bytes",
            "file_count",
        ):
            if (
                not isinstance(pin[field], int)
                or isinstance(pin[field], bool)
                or pin[field] <= 0
            ):
                refuse(f"release pin {field} is not a positive integer")
        directories = pin["directories"]
        if (
            not isinstance(directories, list)
            or not directories
            or directories != sorted(set(directories))
            or any(
                not isinstance(directory, str)
                or DIRECTORY_PATTERN.fullmatch(directory) is None
                for directory in directories
            )
        ):
            refuse("release directory inventory is invalid")
        counts = pin["counts"]
        if (
            not isinstance(counts, dict)
            or not counts
            or any(
                not isinstance(key, str)
                or not key
                or not isinstance(value, int)
                or isinstance(value, bool)
                or value < 0
                for key, value in counts.items()
            )
        ):
            refuse("release count inventory is invalid")
        if pin["archive_member_count"] != (
            pin["file_count"] + len(directories) + 1
        ):
            refuse("archive member count disagrees with files and directories")

    identities = [pin["identity"] for pin in releases]
    if identities != sorted(set(identities)):
        refuse("release identities are not sorted and unique")
    if len({pin["tag"] for pin in releases}) != len(releases):
        refuse("release tags are not unique")
    if len({pin["asset"] for pin in releases}) != len(releases):
        refuse("release assets are not unique")
    if identities.count(latest_identity) != 1:
        refuse("latest identity does not name exactly one release pin")
    latest_pin = next(pin for pin in releases if pin["identity"] == latest_identity)
    if (
        require_plain_latest_title
        and "historical" in latest_pin["release_title"].casefold()
    ):
        refuse("latest release title must not use Historical wording")
    return registry


def load_registry(path: Path) -> dict:
    try:
        payload = path.read_bytes()
    except OSError as error:
        refuse(f"cannot read release-pin registry {path}: {error}")
    return load_registry_bytes(payload, str(path))


def pin_sha256(pin: dict) -> str:
    encoded = json.dumps(
        pin, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode() + b"\n"
    return hashlib.sha256(encoded).hexdigest()


def require_append_only(
    current: dict,
    historical: list[dict],
    bootstrap: dict[str, str] = BOOTSTRAP_PIN_SHA256,
) -> None:
    current_by_identity = {
        pin["identity"]: pin for pin in current["releases"]
    }
    for identity, expected_sha256 in bootstrap.items():
        pin = current_by_identity.get(identity)
        if pin is None:
            refuse(f"current registry removed bootstrap identity {identity}")
        if pin_sha256(pin) != expected_sha256:
            refuse(f"current registry changed bootstrap pin {identity}")

    retained = {}
    # A committed registry row is permanent even if a manual Pages run
    # never published it. Git history is the only durable authority
    # available to distinguish old pins from newly proposed ones.
    for registry in historical:
        if registry["release_repository"] != current["release_repository"]:
            refuse("historical release repository disagrees with the current registry")
        for pin in registry["releases"]:
            identity = pin["identity"]
            if identity in retained and retained[identity] != pin:
                refuse(f"historical release pin changed for {identity}")
            retained[identity] = pin

    for identity, pin in retained.items():
        if identity not in current_by_identity:
            refuse(f"current registry removed published identity {identity}")
        if current_by_identity[identity] != pin:
            refuse(f"current registry changed published pin {identity}")


def load_historical_registries(path: Path, repository_root: Path) -> list[dict]:
    root = repository_root.resolve()
    try:
        relative = path.resolve().relative_to(root).as_posix()
    except (OSError, ValueError) as error:
        refuse(f"release-pin registry is outside the repository: {error}")

    shallow = subprocess.run(
        ["git", "rev-parse", "--is-shallow-repository"],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if shallow.returncode != 0 or shallow.stdout.strip() != b"false":
        refuse("full git history is required to preserve published release pins")

    parent = subprocess.run(
        ["git", "rev-parse", "--verify", "HEAD^"],
        cwd=root,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if parent.returncode != 0:
        return []
    history = subprocess.run(
        [
            "git",
            "log",
            "--first-parent",
            "--format=%H",
            "--diff-filter=AM",
            "HEAD^",
            "--",
            relative,
        ],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if history.returncode != 0:
        refuse(f"cannot list historical release-pin registries: {history.stderr.decode(errors='replace')}")

    registries = []
    for commit in history.stdout.decode().splitlines():
        if (
            len(commit) not in (40, 64)
            or any(
                character not in "0123456789abcdef" for character in commit
            )
        ):
            refuse("git returned an invalid historical registry commit")
        object_name = f"{commit}:{relative}"
        exists = subprocess.run(
            ["git", "cat-file", "-e", object_name],
            cwd=root,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if exists.returncode != 0:
            refuse(f"historical registry object is missing at {commit}")
        shown = subprocess.run(
            ["git", "show", object_name],
            cwd=root,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if shown.returncode != 0:
            refuse(f"cannot read historical release-pin registry at {commit}")
        registries.append(
            load_registry_bytes(
                shown.stdout,
                f"{commit}:{relative}",
                require_plain_latest_title=False,
            )
        )
    return registries


MODE = sys.argv[1] if len(sys.argv) > 1 else ""
if MODE == "extract":
    if len(sys.argv) != 6:
        refuse(
            "extract mode requires archive, target, release-pin registry, "
            "and release index"
        )
    ARCHIVE = Path(sys.argv[2])
    TARGET = Path(sys.argv[3])
    try:
        release_index = int(sys.argv[5])
    except ValueError:
        refuse("release index is not an integer")
    registry = load_registry(Path(sys.argv[4]))
    releases = registry["releases"]
    if release_index < 0 or release_index >= len(releases):
        refuse("release index is out of bounds")
    PIN = releases[release_index]
    IDENTITY = PIN["identity"]
    BUILD_SHA256 = PIN["build_sha256"]
    MANIFEST_TREE_SHA256 = PIN["manifest_tree_sha256"]
    ARTIFACTS_SHA256 = PIN["artifacts_sha256"]
    CONTENT_TREE_SHA256 = PIN["content_tree_sha256"]
    MODE_SHA256 = PIN["mode_sha256"]
    RECURSIVE_IDENTITY_SHA256 = PIN["recursive_identity_sha256"]
    FILE_COUNT = PIN["file_count"]
    FILE_BYTES = PIN["file_bytes"]
    DIRECTORIES = tuple(PIN["directories"])
    EXPECTED_COUNTS = PIN["counts"]
elif MODE not in ("finalize", "validate"):
    refuse("expected validate, extract, or finalize mode")


def canonical(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode()


def digest(value: object) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def tree_inventory(root: Path) -> tuple[tuple[str, str, int, int, str], ...]:
    try:
        root_metadata = root.stat(follow_symlinks=False)
    except OSError as error:
        refuse(f"cannot inspect published tree root {root}: {error}")
    if not stat.S_ISDIR(root_metadata.st_mode):
        refuse(f"published tree root is not a regular directory: {root}")

    inventory = []
    for path in [root, *sorted(root.rglob("*"))]:
        relative = "" if path == root else path.relative_to(root).as_posix()
        metadata = path.stat(follow_symlinks=False)
        mode = stat.S_IMODE(metadata.st_mode)
        if stat.S_ISDIR(metadata.st_mode):
            inventory.append((relative, "d", mode, 0, ""))
            continue
        if not stat.S_ISREG(metadata.st_mode):
            refuse(f"published tree contains a link or special file: {relative!r}")

        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        size = 0
        sha256 = hashlib.sha256()
        with os.fdopen(descriptor, "rb") as source:
            opened = os.fstat(source.fileno())
            if not stat.S_ISREG(opened.st_mode):
                refuse(f"published tree file is not regular: {relative!r}")
            while chunk := source.read(1 << 20):
                size += len(chunk)
                sha256.update(chunk)
        inventory.append((relative, "f", mode, size, sha256.hexdigest()))
    return tuple(inventory)


def tree_measurements(root: Path) -> dict[str, object]:
    inventory = tree_inventory(root)
    files = [row for row in inventory if row[1] == "f"]

    content = hashlib.sha256()
    for relative, _, _, _, file_sha256 in files:
        content.update(file_sha256.encode())
        content.update(b"  ./")
        content.update(relative.encode())
        content.update(b"\n")

    mode_rows = [
        f"{relative}\t{kind}\t{mode:o}\n"
        for relative, kind, mode, _, _ in inventory
    ]
    identity_rows = [
        f"{relative}\t{kind}\t{mode:o}\t{file_sha256}\n"
        for relative, kind, mode, _, file_sha256 in inventory
    ]
    return {
        "content_tree_sha256": content.hexdigest(),
        "file_bytes": sum(row[3] for row in files),
        "file_count": len(files),
        "mode_sha256": hashlib.sha256(
            "".join(sorted(mode_rows)).encode()
        ).hexdigest(),
        "recursive_identity_sha256": hashlib.sha256(
            "".join(identity_rows).encode()
        ).hexdigest(),
    }


def verify_pinned_tree(root: Path, pin: dict) -> None:
    observed = tree_measurements(root)
    for field in (
        "content_tree_sha256",
        "file_bytes",
        "file_count",
        "mode_sha256",
        "recursive_identity_sha256",
    ):
        if observed[field] != pin[field]:
            refuse(
                f"retained tree {pin['identity']} disagrees with its {field} pin"
            )


def copy_regular_tree(source_root: Path, destination_root: Path) -> None:
    if destination_root.exists() or destination_root.is_symlink():
        refuse(f"publication destination already exists: {destination_root}")

    source_entries = [source_root, *sorted(source_root.rglob("*"))]
    source_directories = []
    source_files = []
    for source in source_entries:
        metadata = source.stat(follow_symlinks=False)
        if stat.S_ISDIR(metadata.st_mode):
            source_directories.append(source)
        elif stat.S_ISREG(metadata.st_mode):
            source_files.append(source)
        else:
            relative = source.relative_to(source_root).as_posix()
            refuse(f"cannot copy link or special publication path: {relative!r}")

    destination_root.mkdir(mode=0o700)
    for source in sorted(
        source_directories[1:],
        key=lambda path: (
            len(path.relative_to(source_root).parts),
            path.relative_to(source_root).as_posix(),
        ),
    ):
        relative = source.relative_to(source_root)
        (destination_root / relative).mkdir(mode=0o700)

    for source in source_files:
        relative = source.relative_to(source_root)
        destination = destination_root / relative
        source_descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
        destination_descriptor = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o400,
        )
        with (
            os.fdopen(source_descriptor, "rb") as input_file,
            os.fdopen(destination_descriptor, "wb") as output_file,
        ):
            opened = os.fstat(input_file.fileno())
            if not stat.S_ISREG(opened.st_mode):
                refuse(f"copy source is not a regular file: {relative.as_posix()!r}")
            while chunk := input_file.read(1 << 20):
                output_file.write(chunk)
        os.chmod(
            destination,
            stat.S_IMODE(opened.st_mode),
            follow_symlinks=False,
        )

    for source in sorted(
        source_directories,
        key=lambda path: len(path.relative_to(source_root).parts),
        reverse=True,
    ):
        relative = source.relative_to(source_root)
        destination = (
            destination_root
            if not relative.parts
            else destination_root / relative
        )
        source_mode = stat.S_IMODE(
            source.stat(follow_symlinks=False).st_mode
        )
        os.chmod(destination, source_mode, follow_symlinks=False)


def finalize_publication(registry: dict, publication_root: Path) -> None:
    releases = registry["releases"]
    identities = [pin["identity"] for pin in releases]
    latest_identity = registry["latest_identity"]
    if not publication_root.is_dir() or publication_root.is_symlink():
        refuse("compatibility publication root is not a regular directory")
    observed_children = sorted(path.name for path in publication_root.iterdir())
    if observed_children != identities:
        refuse("retained publication paths do not exactly match the release registry")

    for pin in releases:
        verify_pinned_tree(publication_root / pin["identity"], pin)

    pinned_latest = publication_root / latest_identity
    latest = publication_root / "latest"
    pinned_inventory = tree_inventory(pinned_latest)
    copy_regular_tree(pinned_latest, latest)
    if tree_inventory(latest) != pinned_inventory:
        refuse(
            "latest copy differs from the selected pinned tree in path, type, "
            "size, content, or mode"
        )
    if sorted(path.name for path in publication_root.iterdir()) != sorted(
        [*identities, "latest"]
    ):
        refuse("final publication path inventory is not exact")


if MODE == "validate":
    if len(sys.argv) != 4:
        refuse("validate mode requires release-pin and repository-root paths")
    registry_path = Path(sys.argv[2])
    repository_root = Path(sys.argv[3])
    registry = load_registry(registry_path)
    historical = load_historical_registries(registry_path, repository_root)
    require_append_only(registry, historical)
    print(
        f"verified append-only release registry against {len(historical)} "
        "historical versions"
    )
    raise SystemExit(0)


if MODE == "finalize":
    if len(sys.argv) != 4:
        refuse("finalize mode requires release-pin and publication-root paths")
    registry = load_registry(Path(sys.argv[2]))
    finalize_publication(registry, Path(sys.argv[3]))
    print(
        f"verified {len(registry['releases'])} retained compatibility websites; "
        f"latest is {registry['latest_identity']}"
    )
    raise SystemExit(0)


def normalized_member_name(name: str) -> str:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts:
        refuse(f"unsafe archive path {name!r}")
    parts = tuple(part for part in path.parts if part not in ("", "."))
    normalized = "/".join(parts)
    expected = "." if not normalized else f"./{normalized}"
    if name != expected:
        refuse(f"non-canonical archive path {name!r}")
    return normalized


def artifact_path(value: object) -> str:
    if not isinstance(value, str):
        refuse("manifest contains a non-string artifact path")
    path = PurePosixPath(value)
    if (
        path.is_absolute()
        or ".." in path.parts
        or len(path.parts) not in (1, 2)
        or (len(path.parts) == 2 and path.parts[0] not in DIRECTORIES)
        or value != path.as_posix()
    ):
        refuse(f"manifest contains unsafe artifact path {value!r}")
    return value


try:
    archive_metadata = ARCHIVE.stat(follow_symlinks=False)
except OSError as error:
    refuse(f"cannot inspect release archive: {error}")
if not stat.S_ISREG(archive_metadata.st_mode):
    refuse("release archive is not a regular file")
if archive_metadata.st_size != PIN["archive_bytes"]:
    refuse("release archive byte count disagrees with the pin")
archive_descriptor = os.open(ARCHIVE, os.O_RDONLY | os.O_NOFOLLOW)
archive_sha256 = hashlib.sha256()
with os.fdopen(archive_descriptor, "rb") as archive_stream:
    while chunk := archive_stream.read(1 << 20):
        archive_sha256.update(chunk)
if archive_sha256.hexdigest() != PIN["archive_sha256"]:
    refuse("release archive digest disagrees with the pin")


landing_contract = (
    "<strong>Compatibility snapshot:</strong>\n"
    '        <a href="compatibility/latest/">'
    "Open the real-ledger compatibility website</a>"
).encode()
landing_path = TARGET.parent.parent / "index.html"
try:
    landing_bytes = landing_path.read_bytes()
except OSError as error:
    refuse(f"cannot read the installed landing page: {error}")
if landing_bytes.count(landing_contract) != 1:
    refuse("landing page does not contain exactly one compatibility/latest link")


with tarfile.open(ARCHIVE, mode="r:gz") as archive:
    members = archive.getmembers()
    by_name = {}
    for member in members:
        name = normalized_member_name(member.name)
        if name in by_name:
            refuse(f"duplicate archive path {name!r}")
        if not member.isdir() and not member.isreg():
            refuse(f"archive path {name!r} is a link or special file")
        expected_mode = 0o555 if member.isdir() else 0o444
        if stat.S_IMODE(member.mode) != expected_mode:
            refuse(f"archive path {name!r} has mode {member.mode:o}")
        by_name[name] = member

    build_member = by_name.get("build.json")
    if build_member is None or not build_member.isreg():
        refuse("archive has no regular build.json")
    build_stream = archive.extractfile(build_member)
    if build_stream is None:
        refuse("archive build.json cannot be read")
    build_bytes = build_stream.read()
    if hashlib.sha256(build_bytes).hexdigest() != BUILD_SHA256:
        refuse("build.json digest disagrees with the reviewed build")
    try:
        manifest = json.loads(build_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"build.json is invalid: {error}")
    if canonical(manifest) + b"\n" != build_bytes:
        refuse("build.json is not canonical JSON plus one newline")

    expected_manifest_fields = {
        "artifacts",
        "artifacts_sha256",
        "counts",
        "directories",
        "directories_sha256",
        "freshness_sha256",
        "generator",
        "inputs",
        "provenance",
        "schema_version",
        "tree_sha256",
    }
    if not isinstance(manifest, dict) or set(manifest) != expected_manifest_fields:
        refuse("build.json field inventory is not exact")
    if manifest["schema_version"] != 1:
        refuse("build.json schema is unsupported")
    if manifest["counts"] != EXPECTED_COUNTS:
        refuse("build.json counts disagree with the reviewed build")
    if manifest["freshness_sha256"] != IDENTITY or TARGET.name != IDENTITY:
        refuse("artifact identity disagrees with the publication path")
    if manifest["tree_sha256"] != MANIFEST_TREE_SHA256:
        refuse("manifest tree digest disagrees with the reviewed build")
    if manifest["artifacts_sha256"] != ARTIFACTS_SHA256:
        refuse("artifact-list digest disagrees with the reviewed build")

    artifacts = manifest["artifacts"]
    if not isinstance(artifacts, list):
        refuse("manifest artifacts is not an array")
    paths = []
    rows = {}
    expected_row_fields = {
        "bytes",
        "content_encoding",
        "content_type",
        "path",
        "sha256",
    }
    for row in artifacts:
        if not isinstance(row, dict) or set(row) != expected_row_fields:
            refuse("manifest artifact row fields are not exact")
        path = artifact_path(row["path"])
        if (
            path == "build.json"
            or not isinstance(row["bytes"], int)
            or isinstance(row["bytes"], bool)
            or row["bytes"] < 0
            or not isinstance(row["sha256"], str)
            or len(row["sha256"]) != 64
        ):
            refuse(f"manifest artifact row is invalid: {path!r}")
        try:
            bytes.fromhex(row["sha256"])
        except ValueError:
            refuse(f"manifest artifact digest is invalid: {path!r}")
        paths.append(path)
        rows[path] = row
    if paths != sorted(set(paths)) or len(rows) != FILE_COUNT - 1:
        refuse("manifest artifact inventory is not sorted, unique, and exact")
    if digest(artifacts) != ARTIFACTS_SHA256:
        refuse("artifact-list digest disagrees with its rows")

    grouped = {".": []}
    grouped.update({directory: [] for directory in DIRECTORIES})
    for row in artifacts:
        parts = PurePosixPath(row["path"]).parts
        grouped["." if len(parts) == 1 else parts[0]].append(row)
    directories = [
        {
            "artifacts_sha256": digest(grouped[directory]),
            "file_count": len(grouped[directory]),
            "path": directory,
        }
        for directory in sorted(grouped)
    ]
    if manifest["directories"] != directories:
        refuse("manifest directory inventory disagrees with artifact rows")
    if manifest["directories_sha256"] != digest(directories):
        refuse("manifest directory digest disagrees with its rows")
    if digest(
        {
            "artifacts_sha256": manifest["artifacts_sha256"],
            "directories_sha256": manifest["directories_sha256"],
        }
    ) != MANIFEST_TREE_SHA256:
        refuse("manifest tree digest disagrees with its inventories")

    expected_names = {"", "build.json", *DIRECTORIES, *paths}
    if (
        set(by_name) != expected_names
        or len(members) != PIN["archive_member_count"]
    ):
        refuse("archive member inventory disagrees with build.json")
    for directory in ("", *DIRECTORIES):
        if not by_name[directory].isdir():
            refuse(f"archive directory is not a directory: {directory!r}")
    for path in ("build.json", *paths):
        if not by_name[path].isreg():
            refuse(f"archive file is not regular: {path!r}")

    if TARGET.exists() or TARGET.is_symlink():
        refuse("publication target already exists")
    publication_root = TARGET.parent
    if publication_root.exists():
        if not publication_root.is_dir() or publication_root.is_symlink():
            refuse("compatibility publication root is not a regular directory")
    else:
        if (
            not publication_root.parent.is_dir()
            or publication_root.parent.is_symlink()
        ):
            refuse("documentation target is not a regular directory")
        publication_root.mkdir()
        os.chmod(publication_root, 0o755, follow_symlinks=False)
    TARGET.mkdir(mode=0o700)
    for directory in sorted(DIRECTORIES):
        (TARGET / directory).mkdir(mode=0o700)

    observed = {}
    for path in ("build.json", *paths):
        member = by_name[path]
        source = archive.extractfile(member)
        if source is None:
            refuse(f"archive file cannot be read: {path!r}")
        destination = TARGET.joinpath(*PurePosixPath(path).parts)
        descriptor = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o400,
        )
        size = 0
        sha256 = hashlib.sha256()
        with os.fdopen(descriptor, "wb") as output:
            while chunk := source.read(1 << 20):
                output.write(chunk)
                size += len(chunk)
                sha256.update(chunk)
        os.chmod(destination, 0o444, follow_symlinks=False)
        observed[path] = (size, sha256.hexdigest())

    if observed["build.json"] != (len(build_bytes), BUILD_SHA256):
        refuse("extracted build.json differs from the inspected member")
    for path, row in rows.items():
        if observed[path] != (row["bytes"], row["sha256"]):
            refuse(f"extracted artifact bytes disagree with build.json: {path}")
    for directory in (*DIRECTORIES, ""):
        os.chmod(TARGET / directory, 0o555, follow_symlinks=False)

verify_pinned_tree(TARGET, PIN)
print(f"verified compatibility website {IDENTITY}: {FILE_COUNT} files, {FILE_BYTES} bytes")
