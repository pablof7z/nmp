#!/usr/bin/env python3
"""Verify one exact core plus independently built optional component manifests."""

from __future__ import annotations

import ctypes
import hashlib
import json
import os
import pathlib
import re
import secrets
import stat
import subprocess
import sys
import tempfile
from contextlib import contextmanager
from collections.abc import Iterator
from typing import Any

COMMON_FIELDS = {
    "attestation_symbol",
    "binding_identity",
    "build_flags_digest",
    "cargo_package",
    "component_key",
    "graph_digest",
    "identity",
    "interface_dependency_digest",
    "interface_identity",
    "kind",
    "library_stem",
    "native_identity",
    "profile",
    "rustc_digest",
    "schema",
    "target",
    "uniffi_namespace",
}
OPTIONAL_FIELDS = COMMON_FIELDS | {
    "required_core_artifact_blake3",
    "required_core_identity",
    "required_core_manifest_blake3",
}
KEY = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
TOKEN = re.compile(r"^[A-Za-z0-9_.-]+$")
IDENTITY = re.compile(r"^[a-z0-9-]+-[0-9a-f]{64}$")
DIGEST = re.compile(r"^[0-9a-f]{64}$")


class Refusal(ValueError):
    pass


def refuse(message: str) -> None:
    raise Refusal(message)


class PinnedDirectory:
    def __init__(
        self,
        path: pathlib.Path,
        descriptor: int,
        identity: tuple[int, int],
        mode: int,
        parent: PinnedDirectory | None,
        name: str | None,
    ) -> None:
        self.path = path
        self.descriptor = descriptor
        self.identity = identity
        self.mode = mode
        self.parent = parent
        self.name = name

    def revalidate(self) -> None:
        try:
            current = (
                os.fstat(self.descriptor)
                if self.parent is None
                else os.stat(
                    self.name or "",
                    dir_fd=self.parent.descriptor,
                    follow_symlinks=False,
                )
            )
        except OSError as error:
            refuse(f"{self.path}: pinned directory binding disappeared: {error}")
        if not stat.S_ISDIR(current.st_mode):
            refuse(f"{self.path}: pinned directory binding is no longer a directory")
        if (current.st_dev, current.st_ino) != self.identity:
            refuse(f"{self.path}: pinned directory binding changed during verification")
        if stat.S_IMODE(current.st_mode) != stat.S_IMODE(self.mode):
            refuse(
                f"{self.path}: pinned directory mode changed during verification"
            )


class PinnedFile:
    def __init__(
        self,
        path: pathlib.Path,
        descriptor: int,
        status: os.stat_result,
        parent: PinnedDirectory,
    ) -> None:
        self.path = path
        self.descriptor = descriptor
        self.parent = parent
        self.identity = (status.st_dev, status.st_ino)
        self.size = status.st_size
        self.mode = status.st_mode
        self.mtime_ns = status.st_mtime_ns
        self.ctime_ns = status.st_ctime_ns
        self.nlink = status.st_nlink
        self.content_sha256 = self.current_sha256()

    @property
    def descriptor_path(self) -> str:
        return f"/dev/fd/{self.descriptor}"

    def current_sha256(self) -> bytes:
        digest = hashlib.sha256()
        offset = 0
        while offset < self.size:
            try:
                chunk = os.pread(
                    self.descriptor,
                    min(1024 * 1024, self.size - offset),
                    offset,
                )
            except OSError as error:
                refuse(f"{self.path}: cannot digest pinned file: {error}")
            if not chunk:
                refuse(f"{self.path}: pinned file became shorter while digesting")
            digest.update(chunk)
            offset += len(chunk)
        return digest.digest()

    def read_bytes(self) -> bytes:
        chunks: list[bytes] = []
        offset = 0
        while offset < self.size:
            try:
                chunk = os.pread(self.descriptor, min(1024 * 1024, self.size - offset), offset)
            except OSError as error:
                refuse(f"{self.path}: cannot read pinned file: {error}")
            if not chunk:
                refuse(f"{self.path}: pinned file became shorter during verification")
            chunks.append(chunk)
            offset += len(chunk)
        self.revalidate()
        result = b"".join(chunks)
        if hashlib.sha256(result).digest() != self.content_sha256:
            refuse(f"{self.path}: pinned file bytes changed during verification")
        return result

    def revalidate(self) -> None:
        try:
            descriptor_status = os.fstat(self.descriptor)
            path_status = os.stat(
                self.path.name,
                dir_fd=self.parent.descriptor,
                follow_symlinks=False,
            )
        except OSError as error:
            refuse(f"{self.path}: pinned file binding disappeared: {error}")
        for status, location in (
            (descriptor_status, "descriptor"),
            (path_status, "path"),
        ):
            if not stat.S_ISREG(status.st_mode):
                refuse(f"{self.path}: pinned {location} is no longer a regular file")
            if (status.st_dev, status.st_ino) != self.identity:
                refuse(f"{self.path}: pinned {location} identity changed during verification")
            if status.st_size != self.size:
                refuse(f"{self.path}: pinned {location} size changed during verification")
            if (
                stat.S_IMODE(status.st_mode),
                status.st_mtime_ns,
                status.st_ctime_ns,
                status.st_nlink,
            ) != (
                stat.S_IMODE(self.mode),
                self.mtime_ns,
                self.ctime_ns,
                self.nlink,
            ):
                refuse(
                    f"{self.path}: pinned {location} metadata changed during "
                    "verification"
                )


class PinRegistry:
    def __init__(self) -> None:
        self.directories: dict[pathlib.Path, PinnedDirectory] = {}
        self.files: dict[pathlib.Path, PinnedFile] = {}

    @staticmethod
    def normalized(path: pathlib.Path) -> pathlib.Path:
        absolute = pathlib.Path(os.path.abspath(os.fspath(path)))
        if absolute == pathlib.Path("/"):
            return absolute
        # macOS conventionally exposes /tmp and /var through symlinked
        # prefixes. Canonicalize the caller-trusted parent once, retain the
        # leaf spelling, then use only openat/fstat bindings below that
        # physical parent. A symlink at the authority leaf is still refused.
        physical_parent = pathlib.Path(os.path.realpath(os.fspath(absolute.parent)))
        return physical_parent / absolute.name

    def pin_directory(
        self,
        path: pathlib.Path,
        *,
        sealed: bool = False,
    ) -> PinnedDirectory:
        absolute = self.normalized(path)
        existing = self.directories.get(absolute)
        if existing is not None:
            if sealed:
                status = os.fstat(existing.descriptor)
                if status.st_mode & 0o222:
                    refuse(f"{absolute}: directory must be read-only before verification")
            return existing
        if absolute == pathlib.Path("/"):
            try:
                descriptor = os.open(
                    "/",
                    os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_CLOEXEC", 0),
                )
                status = os.fstat(descriptor)
            except OSError as error:
                refuse(f"/: cannot pin root directory: {error}")
            pinned = PinnedDirectory(
                absolute,
                descriptor,
                (status.st_dev, status.st_ino),
                status.st_mode,
                None,
                None,
            )
            self.directories[absolute] = pinned
            return pinned
        parent = self.pin_directory(absolute.parent)
        flags = (
            os.O_RDONLY
            | os.O_DIRECTORY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0)
        )
        try:
            descriptor = os.open(absolute.name, flags, dir_fd=parent.descriptor)
            status = os.fstat(descriptor)
        except OSError as error:
            refuse(f"{absolute}: cannot pin directory without following links: {error}")
        if not stat.S_ISDIR(status.st_mode):
            os.close(descriptor)
            refuse(f"{absolute}: pinned path is not a directory")
        if sealed and status.st_mode & 0o222:
            os.close(descriptor)
            refuse(f"{absolute}: directory must be read-only before verification")
        pinned = PinnedDirectory(
            absolute,
            descriptor,
            (status.st_dev, status.st_ino),
            status.st_mode,
            parent,
            absolute.name,
        )
        self.directories[absolute] = pinned
        return pinned

    def pin_file(
        self,
        path: pathlib.Path,
        *,
        sealed: bool = False,
        executable: bool = False,
    ) -> PinnedFile:
        absolute = self.normalized(path)
        existing = self.files.get(absolute)
        if existing is not None:
            status = os.fstat(existing.descriptor)
            if sealed and status.st_mode & 0o222:
                refuse(f"{absolute}: file must be read-only before verification")
            if executable and status.st_mode & 0o111 == 0:
                refuse(f"{absolute}: pinned tool is not executable")
            return existing
        parent = self.pin_directory(absolute.parent)
        flags = (
            os.O_RDONLY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0)
        )
        try:
            descriptor = os.open(absolute.name, flags, dir_fd=parent.descriptor)
            status = os.fstat(descriptor)
        except OSError as error:
            refuse(f"{absolute}: cannot pin file without following links: {error}")
        if not stat.S_ISREG(status.st_mode):
            os.close(descriptor)
            refuse(f"{absolute}: pinned path is not a regular file")
        if sealed and status.st_mode & 0o222:
            os.close(descriptor)
            refuse(f"{absolute}: file must be read-only before verification")
        if executable and status.st_mode & 0o111 == 0:
            os.close(descriptor)
            refuse(f"{absolute}: pinned tool is not executable")
        pinned = PinnedFile(absolute, descriptor, status, parent)
        self.files[absolute] = pinned
        return pinned

    def revalidate(self) -> None:
        for pinned in self.files.values():
            pinned.revalidate()
        for pinned in sorted(
            self.directories.values(),
            key=lambda directory: len(directory.path.parts),
            reverse=True,
        ):
            pinned.revalidate()

    def close(self) -> None:
        for pinned in self.files.values():
            os.close(pinned.descriptor)
        for pinned in sorted(
            self.directories.values(),
            key=lambda directory: len(directory.path.parts),
            reverse=True,
        ):
            os.close(pinned.descriptor)


class PinnedTree:
    def __init__(
        self,
        root: PinnedDirectory,
        directories: list[tuple[pathlib.PurePath, PinnedDirectory]],
        files: list[tuple[pathlib.PurePath, PinnedFile]],
    ) -> None:
        self.root = root
        self.directories = directories
        self.files = files

    def file_identities(self) -> set[tuple[int, int]]:
        return {source.identity for _, source in self.files}


def pin_tree(
    registry: PinRegistry,
    path: pathlib.Path,
    *,
    writable_root: bool = False,
) -> PinnedTree:
    # A staged publication root stays owner-writable until its rename, so the
    # caller verifying that tree may waive the seal for the root alone. Every
    # descendant is still required to be read-only.
    root = registry.pin_directory(path, sealed=not writable_root)
    directories: list[tuple[pathlib.PurePath, PinnedDirectory]] = [
        (pathlib.PurePath("."), root)
    ]
    files: list[tuple[pathlib.PurePath, PinnedFile]] = []

    def visit(
        relative: pathlib.PurePath,
        directory: PinnedDirectory,
    ) -> None:
        try:
            names = sorted(os.listdir(directory.descriptor))
        except OSError as error:
            refuse(f"{directory.path}: cannot enumerate pinned tree: {error}")
        for name in names:
            if name in ("", ".", "..") or "/" in name or "\0" in name:
                refuse(f"{directory.path}: invalid tree entry name {name!r}")
            try:
                status = os.stat(
                    name,
                    dir_fd=directory.descriptor,
                    follow_symlinks=False,
                )
            except OSError as error:
                refuse(f"{directory.path / name}: cannot pin tree entry: {error}")
            child_relative = (
                pathlib.PurePath(name)
                if relative == pathlib.PurePath(".")
                else relative / name
            )
            child_path = directory.path / name
            if stat.S_ISDIR(status.st_mode):
                child = registry.pin_directory(child_path, sealed=True)
                directories.append((child_relative, child))
                visit(child_relative, child)
            elif stat.S_ISREG(status.st_mode):
                child = registry.pin_file(child_path, sealed=True)
                if child.nlink != 1:
                    refuse(
                        f"{child_path}: publication source files must have "
                        "exactly one link"
                    )
                files.append((child_relative, child))
            else:
                refuse(
                    f"{child_path}: publish tree entries must be regular files "
                    "or directories; links and special files are refused"
                )

    visit(pathlib.PurePath("."), root)
    return PinnedTree(root, directories, files)


def copy_pinned_file(source: PinnedFile, destination: int) -> None:
    digest = hashlib.sha256()
    offset = 0
    while offset < source.size:
        try:
            chunk = os.pread(
                source.descriptor,
                min(1024 * 1024, source.size - offset),
                offset,
            )
        except OSError as error:
            refuse(f"{source.path}: cannot read pinned publication source: {error}")
        if not chunk:
            refuse(f"{source.path}: publication source became shorter")
        digest.update(chunk)
        written = 0
        while written < len(chunk):
            try:
                count = os.write(destination, chunk[written:])
            except OSError as error:
                refuse(f"{source.path}: cannot write publication copy: {error}")
            if count <= 0:
                refuse(f"{source.path}: publication copy made no write progress")
            written += count
        offset += len(chunk)
    source.revalidate()
    if digest.digest() != source.content_sha256:
        refuse(
            f"{source.path}: publication source bytes changed after being pinned"
        )


def pinned_bytes_equal(first: PinnedFile, second: PinnedFile) -> bool:
    if first.size != second.size:
        return False
    if first.content_sha256 != second.content_sha256:
        return False
    offset = 0
    while offset < first.size:
        length = min(1024 * 1024, first.size - offset)
        try:
            left = os.pread(first.descriptor, length, offset)
            right = os.pread(second.descriptor, length, offset)
        except OSError as error:
            refuse(f"cannot compare published pinned bytes: {error}")
        if left != right or not left:
            return False
        offset += len(left)
    return True


def normalized_publication_mode(mode: int, *, directory: bool) -> int:
    sealed_mode = stat.S_IMODE(mode) & ~0o222
    if sealed_mode:
        return sealed_mode
    return 0o555 if directory else 0o444


def require_exact_tree_copy(
    source: PinnedTree,
    destination: PinnedTree,
    *,
    skip_root_mode: bool = False,
) -> None:
    source_directories = dict(source.directories)
    destination_directories = dict(destination.directories)
    if source_directories.keys() != destination_directories.keys():
        refuse(
            f"{destination.root.path}: published directory set disagrees with "
            f"pinned source: "
            f"{sorted(source_directories.keys() ^ destination_directories.keys(), key=str)}"
        )
    for relative, source_directory in source_directories.items():
        if skip_root_mode and relative == pathlib.PurePath("."):
            # The staged root stays owner-writable until its publication
            # rename; its sealed mode is proven once the final name exists.
            continue
        source_mode = normalized_publication_mode(
            source_directory.mode,
            directory=True,
        )
        destination_mode = stat.S_IMODE(
            os.fstat(destination_directories[relative].descriptor).st_mode
        )
        if destination_mode != source_mode:
            refuse(
                f"{destination.root.path / relative}: published mode disagrees "
                f"with pinned source: {destination_mode:#05o} != "
                f"{source_mode:#05o}"
            )
    source_files = dict(source.files)
    destination_files = dict(destination.files)
    if source_files.keys() != destination_files.keys():
        refuse(
            f"{destination.root.path}: published file set disagrees with "
            f"pinned source: "
            f"{sorted(source_files.keys() ^ destination_files.keys(), key=str)}"
        )
    for relative, source_file in source_files.items():
        destination_file = destination_files[relative]
        source_mode = normalized_publication_mode(
            source_file.mode,
            directory=False,
        )
        destination_mode = stat.S_IMODE(
            os.fstat(destination_file.descriptor).st_mode
        )
        if destination_mode != source_mode:
            refuse(
                f"{destination.root.path / relative}: published mode disagrees "
                f"with pinned source: {destination_mode:#05o} != "
                f"{source_mode:#05o}"
            )
        if not pinned_bytes_equal(source_file, destination_file):
            refuse(
                f"{destination.root.path / relative}: published bytes disagree "
                "with pinned source"
            )


def rename_noreplace(
    parent_descriptor: int,
    source_name: str,
    destination_name: str,
) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    source = os.fsencode(source_name)
    destination = os.fsencode(destination_name)
    if sys.platform == "darwin":
        rename = libc.renameatx_np
        rename.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        rename.restype = ctypes.c_int
        result = rename(
            parent_descriptor,
            source,
            parent_descriptor,
            destination,
            0x00000004 | 0x00000010,  # RENAME_EXCL | RENAME_NOFOLLOW_ANY
        )
    elif sys.platform.startswith("linux"):
        try:
            rename = libc.renameat2
        except AttributeError:
            refuse("atomic no-replace publication requires renameat2 on Linux")
        rename.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        rename.restype = ctypes.c_int
        result = rename(
            parent_descriptor,
            source,
            parent_descriptor,
            destination,
            0x00000001,  # RENAME_NOREPLACE
        )
    else:
        refuse(
            f"atomic no-replace publication is unsupported on {sys.platform}"
        )
    if result != 0:
        error = ctypes.get_errno()
        detail = ""
        try:
            source_status = os.stat(
                source_name,
                dir_fd=parent_descriptor,
                follow_symlinks=False,
            )
            parent_status = os.fstat(parent_descriptor)
        except OSError:
            pass
        else:
            detail = (
                f" (source mode {stat.S_IMODE(source_status.st_mode):#05o},"
                f" parent mode {stat.S_IMODE(parent_status.st_mode):#05o})"
            )
        raise OSError(error, os.strerror(error) + detail, destination_name)


def remove_directory_contents(descriptor: int, path: pathlib.Path) -> None:
    try:
        os.fchmod(descriptor, 0o700)
        names = sorted(os.listdir(descriptor))
    except OSError as error:
        refuse(f"{path}: cannot open failed publication for cleanup: {error}")
    for name in names:
        try:
            status = os.stat(
                name,
                dir_fd=descriptor,
                follow_symlinks=False,
            )
        except OSError as error:
            refuse(f"{path / name}: cannot inspect failed publication: {error}")
        if stat.S_ISDIR(status.st_mode):
            try:
                child = os.open(
                    name,
                    os.O_RDONLY
                    | os.O_DIRECTORY
                    | getattr(os, "O_CLOEXEC", 0)
                    | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=descriptor,
                )
            except OSError as error:
                refuse(
                    f"{path / name}: cannot pin failed publication directory: "
                    f"{error}"
                )
            try:
                child_status = os.fstat(child)
                if (child_status.st_dev, child_status.st_ino) != (
                    status.st_dev,
                    status.st_ino,
                ):
                    refuse(
                        f"{path / name}: failed publication directory changed "
                        "during cleanup"
                    )
                remove_directory_contents(child, path / name)
            finally:
                os.close(child)
            try:
                os.rmdir(name, dir_fd=descriptor)
            except OSError as error:
                refuse(
                    f"{path / name}: cannot remove failed publication directory: "
                    f"{error}"
                )
        else:
            try:
                os.unlink(name, dir_fd=descriptor)
            except OSError as error:
                refuse(
                    f"{path / name}: cannot remove failed publication entry: {error}"
                )
    try:
        os.fsync(descriptor)
    except OSError as error:
        refuse(f"{path}: cannot sync failed publication cleanup: {error}")


def cleanup_hidden_publication(
    parent: PinnedDirectory,
    hidden_name: str,
    hidden_identity: tuple[int, int],
) -> None:
    try:
        current = os.stat(
            hidden_name,
            dir_fd=parent.descriptor,
            follow_symlinks=False,
        )
    except FileNotFoundError:
        return
    except OSError as error:
        refuse(f"{parent.path / hidden_name}: cannot inspect cleanup target: {error}")
    if (
        not stat.S_ISDIR(current.st_mode)
        or (current.st_dev, current.st_ino) != hidden_identity
    ):
        # The verifier no longer owns this binding. Refusing to touch it is
        # safer than deleting an attacker-created replacement.
        return

    cleanup_name = f".nmp-cleanup-{os.getpid()}-{secrets.token_hex(8)}"
    try:
        rename_noreplace(parent.descriptor, hidden_name, cleanup_name)
        current = os.stat(
            cleanup_name,
            dir_fd=parent.descriptor,
            follow_symlinks=False,
        )
    except OSError as error:
        refuse(
            f"{parent.path / hidden_name}: cannot quarantine failed publication: "
            f"{error}"
        )
    if (
        not stat.S_ISDIR(current.st_mode)
        or (current.st_dev, current.st_ino) != hidden_identity
    ):
        refuse(
            f"{parent.path / cleanup_name}: cleanup quarantine identity changed"
        )
    try:
        descriptor = os.open(
            cleanup_name,
            os.O_RDONLY
            | os.O_DIRECTORY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent.descriptor,
        )
    except OSError as error:
        refuse(
            f"{parent.path / cleanup_name}: cannot pin cleanup quarantine: {error}"
        )
    try:
        status = os.fstat(descriptor)
        if (status.st_dev, status.st_ino) != hidden_identity:
            refuse(
                f"{parent.path / cleanup_name}: cleanup descriptor identity changed"
            )
        remove_directory_contents(descriptor, parent.path / cleanup_name)
    finally:
        os.close(descriptor)
    try:
        os.rmdir(cleanup_name, dir_fd=parent.descriptor)
        os.fsync(parent.descriptor)
    except OSError as error:
        refuse(
            f"{parent.path / cleanup_name}: cannot remove cleanup quarantine: {error}"
        )


def mutation_hook(phase: str) -> None:
    hook_directory_value = os.environ.get("NMP_COMPONENT_VERIFIER_HOOK_DIR")
    if hook_directory_value is None:
        return
    hook_directory = pathlib.Path(hook_directory_value)
    if not hook_directory.is_dir():
        refuse(
            "NMP_COMPONENT_VERIFIER_HOOK_DIR must name an existing test directory"
        )
    ready = hook_directory / f"{phase}.ready"
    release = hook_directory / f"{phase}.release"
    try:
        os.mkfifo(release, 0o600)
        ready_descriptor = os.open(
            ready,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0),
            0o600,
        )
        os.close(ready_descriptor)
        release_descriptor = os.open(
            release,
            os.O_RDONLY | getattr(os, "O_CLOEXEC", 0),
        )
        try:
            if os.read(release_descriptor, 1) != b"1":
                refuse(f"test mutation hook {phase!r} received an invalid release")
        finally:
            os.close(release_descriptor)
    except OSError as error:
        refuse(f"test mutation hook {phase!r} failed: {error}")
    finally:
        try:
            os.unlink(release)
        except FileNotFoundError:
            pass


def publish_tree(
    registry: PinRegistry,
    tree: PinnedTree,
    destination_path: pathlib.Path,
    required_identities: list[tuple[int, int]],
) -> None:
    tree_identities = [source.identity for _, source in tree.files]
    missing = [
        identity
        for identity in required_identities
        if tree_identities.count(identity) != 1
    ]
    if missing:
        refuse(
            f"{tree.root.path}: publish tree does not contain every verified payload "
            f"identity with exact multiplicity: {sorted(missing)}"
        )
    required_set = set(required_identities)
    unverified_native = [
        str(relative)
        for relative, source in tree.files
        if relative.suffix in (".a", ".so", ".dylib")
        and source.identity not in required_set
    ]
    if unverified_native:
        refuse(
            f"{tree.root.path}: publish tree contains unverified native payloads: "
            f"{unverified_native}"
        )

    destination = registry.normalized(destination_path)
    if destination.name in ("", ".", ".."):
        refuse(f"{destination}: invalid publication destination")
    parent = registry.pin_directory(destination.parent)
    try:
        os.stat(
            destination.name,
            dir_fd=parent.descriptor,
            follow_symlinks=False,
        )
    except FileNotFoundError:
        pass
    except OSError as error:
        refuse(f"{destination}: cannot inspect publication destination: {error}")
    else:
        refuse(f"{destination}: publication destination already exists")

    hidden_name = (
        f".{destination.name}.nmp-publish-{os.getpid()}-{secrets.token_hex(8)}"
    )
    try:
        os.mkdir(hidden_name, 0o700, dir_fd=parent.descriptor)
        hidden_descriptor = os.open(
            hidden_name,
            os.O_RDONLY
            | os.O_DIRECTORY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent.descriptor,
        )
    except OSError as error:
        refuse(f"{destination}: cannot create fresh hidden publication: {error}")

    hidden_status = os.fstat(hidden_descriptor)
    hidden_identity = (hidden_status.st_dev, hidden_status.st_ino)
    publication_succeeded = False
    destination_directories: dict[pathlib.PurePath, int] = {
        pathlib.PurePath("."): hidden_descriptor
    }
    try:
        for relative, source_directory in tree.directories[1:]:
            parent_relative = relative.parent
            parent_descriptor = destination_directories[parent_relative]
            mode = normalized_publication_mode(
                source_directory.mode,
                directory=True,
            )
            staging_mode = mode | 0o700
            try:
                os.mkdir(relative.name, staging_mode, dir_fd=parent_descriptor)
                descriptor = os.open(
                    relative.name,
                    os.O_RDONLY
                    | os.O_DIRECTORY
                    | getattr(os, "O_CLOEXEC", 0)
                    | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=parent_descriptor,
                )
            except OSError as error:
                refuse(
                    f"{destination / relative}: cannot create publication "
                    f"directory: {error}"
                )
            destination_directories[relative] = descriptor

        for relative, source in tree.files:
            directory_descriptor = destination_directories[relative.parent]
            mode = normalized_publication_mode(source.mode, directory=False)
            try:
                destination_descriptor = os.open(
                    relative.name,
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | getattr(os, "O_CLOEXEC", 0)
                    | getattr(os, "O_NOFOLLOW", 0),
                    mode,
                    dir_fd=directory_descriptor,
                )
            except OSError as error:
                refuse(
                    f"{destination / relative}: cannot create publication file: {error}"
                )
            try:
                copy_pinned_file(source, destination_descriptor)
                os.fsync(destination_descriptor)
                os.fchmod(destination_descriptor, mode)
            finally:
                os.close(destination_descriptor)

        for relative, source_directory in reversed(tree.directories):
            descriptor = destination_directories[relative]
            mode = normalized_publication_mode(
                source_directory.mode,
                directory=True,
            )
            if relative == pathlib.PurePath("."):
                # Renaming a directory requires write permission on that
                # directory itself, because the kernel may rewrite its parent
                # link. Sealing the staged root before publication therefore
                # makes the rename fail with EACCES. Keep the root writable by
                # its owner across the rename and seal it at its final name.
                mode |= 0o700
            os.fchmod(descriptor, mode)
            os.fsync(descriptor)
        mutation_hook("destination-staged")
        parent.revalidate()
        try:
            os.stat(
                destination.name,
                dir_fd=parent.descriptor,
                follow_symlinks=False,
            )
        except FileNotFoundError:
            pass
        except OSError as error:
            refuse(
                f"{destination}: cannot recheck final publication binding: {error}"
            )
        else:
            refuse(f"{destination}: final publication binding appeared during staging")

        # Some macOS filesystems refuse to rename a directory while it or a
        # descendant is open. Seal and fsync the complete tree first, capture
        # its inode, then close every staging descriptor before the one atomic
        # no-replace rename. A fresh exact-tree pin after the deterministic
        # race hook proves the closed tree was not changed before publication.
        staged_identity = hidden_identity
        for relative, descriptor in sorted(
            destination_directories.items(),
            key=lambda item: len(item[0].parts),
            reverse=True,
        ):
            del relative
            os.close(descriptor)
        destination_directories.clear()
        mutation_hook("destination-ready")
        parent.revalidate()
        staging_registry = PinRegistry()
        try:
            staged_tree = pin_tree(
                staging_registry,
                destination.parent / hidden_name,
                writable_root=True,
            )
            require_exact_tree_copy(tree, staged_tree, skip_root_mode=True)
            if staged_tree.root.identity != staged_identity:
                refuse(
                    f"{destination}: staged publication inode changed before rename"
                )
            staging_registry.revalidate()
        finally:
            staging_registry.close()
        parent.revalidate()
        try:
            rename_noreplace(
                parent.descriptor,
                hidden_name,
                destination.name,
            )
            publication_succeeded = True
            os.fsync(parent.descriptor)
            mutation_hook("destination-published")
            published_status = os.stat(
                destination.name,
                dir_fd=parent.descriptor,
                follow_symlinks=False,
            )
            if not stat.S_ISDIR(published_status.st_mode):
                refuse(f"{destination}: published binding is not a directory")
            if (
                published_status.st_dev,
                published_status.st_ino,
            ) != (
                staged_identity[0],
                staged_identity[1],
            ):
                refuse(
                    f"{destination}: published binding is not the staged directory inode"
                )
            # The root stayed owner-writable so the rename could proceed. Seal
            # it now that it carries its final name, and prove the descriptor
            # is still the published inode before changing its mode.
            published_descriptor = os.open(
                destination.name,
                os.O_RDONLY
                | os.O_DIRECTORY
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=parent.descriptor,
            )
            try:
                sealed_status = os.fstat(published_descriptor)
                if (sealed_status.st_dev, sealed_status.st_ino) != staged_identity:
                    refuse(
                        f"{destination}: published root changed before it was sealed"
                    )
                os.fchmod(
                    published_descriptor,
                    normalized_publication_mode(
                        tree.directories[0][1].mode,
                        directory=True,
                    ),
                )
                os.fsync(published_descriptor)
            finally:
                os.close(published_descriptor)
        except OSError as error:
            refuse(f"{destination}: cannot atomically publish pinned tree: {error}")
    finally:
        for relative, descriptor in sorted(
            destination_directories.items(),
            key=lambda item: len(item[0].parts),
            reverse=True,
        ):
            del relative
            os.close(descriptor)
        if not publication_succeeded:
            failing = sys.exc_info()[0] is not None
            try:
                cleanup_hidden_publication(
                    parent,
                    hidden_name,
                    hidden_identity,
                )
            except Refusal as cleanup_error:
                # A cleanup refusal must never replace the refusal that caused
                # it, or the real publication failure becomes invisible.
                if not failing:
                    raise
                sys.stderr.write(
                    f"component-manifests: cleanup after the failed publication "
                    f"of {destination} also failed: {cleanup_error}\n"
                )

    published_tree = pin_tree(registry, destination)
    require_exact_tree_copy(tree, published_tree)
    registry.revalidate()


def read_manifest(source: PinnedFile) -> dict[str, Any]:
    raw = source.read_bytes()
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"{source.path}: invalid JSON: {error}")
    if not isinstance(value, dict):
        refuse(f"{source.path}: manifest must be an object")
    canonical = (
        json.dumps(value, separators=(",", ":"), sort_keys=True).encode("utf-8") + b"\n"
    )
    if raw != canonical:
        refuse(
            f"{source.path}: manifest is not canonical sorted JSON with one trailing newline"
        )
    return value


def read_witness(source: PinnedFile) -> tuple[bytes, dict[str, Any]]:
    raw = source.read_bytes()
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"{source.path}: invalid witness JSON: {error}")
    if not isinstance(value, dict):
        refuse(f"{source.path}: witness must be an object")
    canonical = (
        json.dumps(value, separators=(",", ":"), sort_keys=True).encode("utf-8")
        + b"\n"
    )
    if raw != canonical:
        refuse(
            f"{source.path}: witness is not canonical sorted JSON with one trailing newline"
        )
    return raw, value


def string(manifest: dict[str, Any], field: str, source: pathlib.Path) -> str:
    value = manifest.get(field)
    if not isinstance(value, str) or not value:
        refuse(f"{source}: {field} must be a non-empty string")
    return value


def validate_shape(manifest: dict[str, Any], source: pathlib.Path) -> None:
    if manifest.get("schema") != 2:
        refuse(f"{source}: schema must be exactly 2")
    kind = manifest.get("kind")
    if kind not in ("core", "optional"):
        refuse(f"{source}: kind must be core or optional")
    expected = COMMON_FIELDS if kind == "core" else OPTIONAL_FIELDS
    actual = set(manifest)
    if actual != expected:
        refuse(
            f"{source}: exact fields disagree; missing={sorted(expected - actual)}, "
            f"unknown={sorted(actual - expected)}"
        )
    key = string(manifest, "component_key", source)
    if not KEY.fullmatch(key):
        refuse(f"{source}: component_key is not a stable kebab-case key: {key!r}")
    for field in ("cargo_package", "library_stem", "uniffi_namespace"):
        value = string(manifest, field, source)
        if not TOKEN.fullmatch(value):
            refuse(f"{source}: invalid {field}: {value!r}")
    attestation_symbol = string(manifest, "attestation_symbol", source)
    if not TOKEN.fullmatch(attestation_symbol):
        refuse(f"{source}: invalid attestation_symbol: {attestation_symbol!r}")
    for field in (
        "identity",
        "binding_identity",
        "native_identity",
        "interface_identity",
    ):
        value = string(manifest, field, source)
        if not IDENTITY.fullmatch(value):
            refuse(f"{source}: invalid {field}: {value!r}")
    for field in (
        "build_flags_digest",
        "graph_digest",
        "interface_dependency_digest",
        "rustc_digest",
    ):
        value = string(manifest, field, source)
        if not DIGEST.fullmatch(value):
            refuse(f"{source}: invalid {field}: {value!r}")
    if kind == "optional":
        required = string(manifest, "required_core_identity", source)
        if not IDENTITY.fullmatch(required):
            refuse(f"{source}: invalid required_core_identity: {required!r}")
        for field in (
            "required_core_artifact_blake3",
            "required_core_manifest_blake3",
        ):
            value = string(manifest, field, source)
            if not DIGEST.fullmatch(value):
                refuse(f"{source}: invalid {field}: {value!r}")
    identity = string(manifest, "identity", source)
    if string(manifest, "binding_identity", source) != identity:
        refuse(f"{source}: binding identity does not equal native manifest identity")
    if string(manifest, "native_identity", source) != identity:
        refuse(f"{source}: native identity does not equal manifest identity")


def verify(sources: list[PinnedFile]) -> dict[str, Any]:
    if not sources:
        refuse("at least one component manifest is required")
    entries: list[tuple[PinnedFile, dict[str, Any]]] = []
    keys: dict[str, tuple[PinnedFile, str]] = {}
    for source in sources:
        manifest = read_manifest(source)
        validate_shape(manifest, source.path)
        key = string(manifest, "component_key", source.path)
        identity = string(manifest, "identity", source.path)
        if key in keys:
            first_source, first_identity = keys[key]
            refuse(
                f"duplicate component_key {key!r}: {first_source.path} "
                f"({first_identity}) and {source.path} ({identity})"
            )
        keys[key] = (source, identity)
        entries.append((source, manifest))

    cores = [
        (source, manifest)
        for source, manifest in entries
        if manifest["kind"] == "core"
    ]
    if len(cores) != 1:
        refuse(f"manifest set must contain exactly one core; found {len(cores)}")
    core_source, core = cores[0]
    core_identity = string(core, "identity", core_source.path)
    # Every component in one set is built from one toolchain, one flag set,
    # one crossing contract -- and one resolution of that contract's own
    # dependencies. The last is not implied by the others: each component is
    # an independent Cargo resolution under its own target directory, so the
    # Tokio (and every other interface dependency) it links can feature-unify
    # differently while all the identities still agree. Values crossing the
    # seam would then have two layouts.
    tuple_fields = (
        "target",
        "profile",
        "rustc_digest",
        "build_flags_digest",
        "interface_identity",
        "interface_dependency_digest",
    )
    for source, manifest in entries:
        for field in tuple_fields:
            if manifest[field] != core[field]:
                refuse(
                    f"{source.path}: {field} disagrees with core: "
                    f"{manifest[field]!r} != {core[field]!r}"
                )
        if manifest["kind"] == "optional":
            required = manifest["required_core_identity"]
            if required != core_identity:
                refuse(
                    f"{source.path}: required_core_identity {required!r} does not match "
                    f"core {core_identity!r}"
                )

    ordered = [
        manifest
        for _, manifest in sorted(
            entries,
            key=lambda item: item[1]["component_key"],
        )
    ]
    return {
        "components": ordered,
        "core_identity": core_identity,
        "interface_identity": core["interface_identity"],
        "profile": core["profile"],
        "schema": 1,
        "target": core["target"],
    }


@contextmanager
def executable_snapshot(source: PinnedFile) -> Iterator[pathlib.Path]:
    source.revalidate()
    with tempfile.TemporaryDirectory(prefix="nmp-pinned-executable-") as temporary:
        directory = pathlib.Path(temporary)
        destination = directory / source.path.name
        try:
            descriptor = os.open(
                destination,
                os.O_WRONLY
                | os.O_CREAT
                | os.O_EXCL
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
                0o500,
            )
        except OSError as error:
            refuse(f"{source.path}: cannot create pinned executable snapshot: {error}")
        try:
            copy_pinned_file(source, descriptor)
            os.fsync(descriptor)
            os.fchmod(descriptor, 0o500)
        finally:
            os.close(descriptor)
        try:
            snapshot_descriptor = os.open(
                destination,
                os.O_RDONLY
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
            )
            status = os.fstat(snapshot_descriptor)
        except OSError as error:
            refuse(f"{source.path}: cannot pin executable snapshot: {error}")
        snapshot = PinnedFile(destination, snapshot_descriptor, status, source.parent)
        try:
            if not pinned_bytes_equal(source, snapshot):
                refuse(
                    f"{source.path}: executable snapshot bytes disagree with pinned tool"
                )
            yield destination
        finally:
            os.close(snapshot_descriptor)


def run_tool(
    tool: PinnedFile,
    arguments: list[str],
    purpose: str,
    *,
    inputs: list[PinnedFile] | None = None,
) -> bytes:
    pass_fds = {source.descriptor for source in inputs or []}
    for source in inputs or []:
        try:
            os.lseek(source.descriptor, 0, os.SEEK_SET)
        except OSError as error:
            refuse(f"{source.path}: cannot rewind pinned tool input: {error}")
    with executable_snapshot(tool) as executable:
        try:
            result = subprocess.run(
                [str(executable), *arguments],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                pass_fds=tuple(sorted(pass_fds)),
            )
        except OSError as error:
            refuse(f"{purpose}: cannot execute artifact witness tool: {error}")
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        refuse(f"{purpose}: artifact witness tool refused: {detail}")
    return result.stdout


def tool_digest(tool: PinnedFile, source: PinnedFile) -> str:
    output = run_tool(
        tool,
        ["digest", "--file", source.descriptor_path],
        f"{source.path} digest",
        inputs=[source],
    )
    try:
        digest = output.decode("ascii").strip()
    except UnicodeDecodeError:
        refuse(f"{source.path}: artifact witness tool emitted a non-ASCII digest")
    if not DIGEST.fullmatch(digest):
        refuse(
            f"{source.path}: artifact witness tool emitted an invalid BLAKE3 digest"
        )
    return digest


def run_metadata_audit(
    tool: PinnedFile,
    artifact: PinnedFile,
) -> None:
    try:
        os.lseek(artifact.descriptor, 0, os.SEEK_SET)
    except OSError as error:
        refuse(f"{artifact.path}: cannot rewind pinned metadata input: {error}")
    with executable_snapshot(tool) as executable:
        try:
            result = subprocess.run(
                [str(executable), artifact.descriptor_path],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                pass_fds=(artifact.descriptor,),
            )
        except OSError as error:
            refuse(f"{artifact.path}: cannot execute pinned metadata audit: {error}")
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        refuse(f"{artifact.path}: pinned metadata audit refused: {detail}")


def pin_artifact_pairs(
    registry: PinRegistry,
    path_pairs: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    if not path_pairs:
        refuse("artifact-backed verification requires at least one artifact")
    pairs: list[dict[str, Any]] = []
    for path_pair in path_pairs:
        artifact_path = path_pair["artifact"]
        registry.pin_directory(artifact_path.parent, sealed=True)
        witness_path = path_pair.get(
            "witness",
            pathlib.Path(f"{artifact_path}.witness.json"),
        )
        pair: dict[str, Any] = {
            "artifact": registry.pin_file(artifact_path, sealed=True),
            "manifest": registry.pin_file(path_pair["manifest"], sealed=True),
            "witness": registry.pin_file(witness_path, sealed=True),
        }
        for field in (
            "forbid_symbols",
            "localization_source",
            "localization_plan",
            "metadata_audit",
        ):
            path = path_pair.get(field)
            if path is not None:
                pair[field] = registry.pin_file(
                    path,
                    sealed=True,
                    executable=field == "metadata_audit",
                )
        pairs.append(pair)
    return pairs


def validate_artifact_pairs(
    tool: PinnedFile,
    pairs: list[dict[str, Any]],
) -> dict[str, Any]:
    unique_manifests = list(
        dict.fromkeys(pair["manifest"] for pair in pairs)
    )
    result = verify(unique_manifests)
    manifests = {
        source: read_manifest(source)
        for source in unique_manifests
    }
    entries: list[dict[str, Any]] = []

    witness_fields = {
        "architecture",
        "artifact_blake3",
        "artifact_size",
        "attestation",
        "component_key",
        "format",
        "public_symbols",
        "schema",
        "target",
        "uniffi_components",
    }
    common_attestation_fields = {
        "component_key",
        "identity",
        "interface_identity",
        "kind",
        "schema",
    }

    supplied_artifacts = {
        pair["artifact"].identity
        for pair in pairs
    }
    for pair in pairs:
        artifact: PinnedFile = pair["artifact"]
        manifest_source: PinnedFile = pair["manifest"]
        witness_source: PinnedFile = pair["witness"]
        manifest = manifests[manifest_source]
        expected_name = f"lib{manifest['library_stem']}{artifact.path.suffix}"
        if artifact.path.name != expected_name:
            refuse(
                f"{artifact.path}: artifact name does not match manifest library_stem "
                f"{manifest['library_stem']!r}"
            )
        stored_bytes, witness = read_witness(witness_source)
        metadata_audit = pair.get("metadata_audit")
        if metadata_audit is not None:
            run_metadata_audit(metadata_audit, artifact)
        witness_arguments = [
            "witness",
            "--artifact",
            artifact.descriptor_path,
            "--target",
            manifest["target"],
            "--component-key",
            manifest["component_key"],
            "--attestation-symbol",
            manifest["attestation_symbol"],
        ]
        forbidden = pair.get("forbid_symbols")
        localization_source = pair.get("localization_source")
        localization_plan = pair.get("localization_plan")
        if forbidden is None:
            if manifest["kind"] == "optional":
                refuse(
                    f"{artifact.path}: optional artifacts require exact "
                    "localization provenance"
                )
            if localization_source is not None or localization_plan is not None:
                refuse(
                    f"{artifact.path}: localization provenance requires "
                    "--forbid-symbols"
                )
        else:
            if manifest["kind"] != "optional":
                refuse(
                    f"{artifact.path}: only optional artifacts may localize shared symbols"
                )
            if localization_source is None or localization_plan is None:
                refuse(
                    f"{artifact.path}: forbidden symbols require source and plan provenance"
                )
            if localization_source.identity not in supplied_artifacts:
                refuse(
                    f"{artifact.path}: localization source is not a supplied "
                    "witnessed artifact"
                )
            source_pair = next(
                candidate
                for candidate in pairs
                if candidate["artifact"].identity == localization_source.identity
            )
            source_manifest = manifests[source_pair["manifest"]]
            if (
                source_manifest["kind"] != "core"
                or source_manifest["target"] != manifest["target"]
                or source_manifest["interface_identity"]
                != manifest["interface_identity"]
            ):
                refuse(
                    f"{artifact.path}: localization source is not the matching "
                    "core interface"
                )
            saved_symbols = forbidden.read_bytes()
            saved_plan = localization_plan.read_bytes()
            with tempfile.TemporaryDirectory(
                prefix="nmp-component-localization-"
            ) as temporary:
                derived_symbols = pathlib.Path(temporary) / "symbols.nul"
                derived_plan = run_tool(
                    tool,
                    [
                        "plan-localization",
                        "--artifact",
                        localization_source.descriptor_path,
                        "--target",
                        manifest["target"],
                        "--interface-namespace",
                        "nmp_component_interface",
                        "--out",
                        str(derived_symbols),
                    ],
                    f"{artifact.path} localization plan",
                    inputs=[localization_source],
                )
                if derived_plan != saved_plan:
                    refuse(
                        f"{artifact.path}: saved localization plan disagrees with "
                        "the witnessed core source"
                    )
                if derived_symbols.read_bytes() != saved_symbols:
                    refuse(
                        f"{artifact.path}: forbidden symbol set disagrees with "
                        "the witnessed core source"
                    )
                witness_arguments.extend(
                    ["--forbid-symbols", str(derived_symbols)]
                )
                rebuilt_bytes = run_tool(
                    tool,
                    witness_arguments,
                    f"{artifact.path} witness",
                    inputs=[artifact],
                )
        if forbidden is None:
            rebuilt_bytes = run_tool(
                tool,
                witness_arguments,
                f"{artifact.path} witness",
                inputs=[artifact],
            )
        if rebuilt_bytes != stored_bytes:
            refuse(
                f"{artifact.path}: stored witness disagrees with a fresh "
                "structural witness"
            )
        if set(witness) != witness_fields:
            refuse(
                f"{witness_source.path}: exact fields disagree; "
                f"missing={sorted(witness_fields - set(witness))}, "
                f"unknown={sorted(set(witness) - witness_fields)}"
            )
        if witness.get("schema") != 1:
            refuse(f"{witness_source.path}: schema must be exactly 1")
        if witness.get("component_key") != manifest["component_key"]:
            refuse(f"{artifact.path}: witness component_key disagrees with manifest")
        if witness.get("target") != manifest["target"]:
            refuse(f"{artifact.path}: witness target disagrees with manifest")
        structural_format = witness.get("format")
        expected_format = {
            ".a": "archive-macho"
            if "-apple-" in manifest["target"]
            else "archive-elf",
            ".dylib": "macho-dylib",
            ".so": "elf-shared-object",
        }.get(artifact.path.suffix)
        if structural_format != expected_format:
            refuse(
                f"{artifact.path}: structural format {structural_format!r} disagrees "
                f"with suffix {artifact.path.suffix!r} and target "
                f"{manifest['target']!r}"
            )
        artifact_digest = witness.get("artifact_blake3")
        if not isinstance(artifact_digest, str) or not DIGEST.fullmatch(artifact_digest):
            refuse(f"{artifact.path}: witness artifact_blake3 is invalid")
        if witness.get("artifact_size") != artifact.size:
            refuse(
                f"{artifact.path}: witness artifact_size disagrees with final bytes"
            )
        public_symbols = witness.get("public_symbols")
        components = witness.get("uniffi_components")
        if not isinstance(public_symbols, list) or not all(
            isinstance(symbol, str) for symbol in public_symbols
        ):
            refuse(f"{artifact.path}: witness public_symbols must be a string array")
        if not isinstance(components, list):
            refuse(f"{artifact.path}: witness uniffi_components must be an array")
        own_components = [
            component
            for component in components
            if isinstance(component, dict)
            and component.get("namespace") == manifest["uniffi_namespace"]
        ]
        if len(own_components) != 1:
            refuse(
                f"{artifact.path}: expected one compiled component for manifest namespace "
                f"{manifest['uniffi_namespace']!r}, found {len(own_components)}"
            )
        callables = own_components[0].get("callables")
        if not isinstance(callables, list) or not all(
            isinstance(callable_name, str) for callable_name in callables
        ):
            refuse(
                f"{artifact.path}: compiled component callables must be a string array"
            )
        normalized_public = {
            symbol[1:] if "macho" in witness.get("format", "") else symbol
            for symbol in public_symbols
        }
        missing_callables = sorted(set(callables) - normalized_public)
        if missing_callables:
            refuse(
                f"{artifact.path}: manifest component callables are not public: "
                f"{missing_callables}"
            )

        attestation = witness.get("attestation")
        if not isinstance(attestation, dict):
            refuse(f"{artifact.path}: witness attestation must be an object")
        build_attestation_fields = {
            "build_flags_digest",
            "cargo_package",
            "graph_digest",
            "interface_dependency_digest",
            "library_stem",
            "profile",
            "rustc_digest",
            "target",
            "uniffi_namespace",
        }
        expected_attestation_fields = (
            set(common_attestation_fields) | build_attestation_fields
        )
        if manifest["kind"] == "optional":
            expected_attestation_fields |= {
                "required_core_artifact_blake3",
                "required_core_identity",
                "required_core_manifest_blake3",
            }
        if set(attestation) != expected_attestation_fields:
            refuse(
                f"{artifact.path}: attestation exact fields disagree; "
                f"missing={sorted(expected_attestation_fields - set(attestation))}, "
                f"unknown={sorted(set(attestation) - expected_attestation_fields)}"
            )
        for field in ("component_key", "identity", "interface_identity", "kind"):
            if attestation.get(field) != manifest[field]:
                refuse(
                    f"{artifact.path}: attestation {field} disagrees with manifest"
                )
        if attestation.get("schema") != 1:
            refuse(f"{artifact.path}: attestation schema must be exactly 1")
        for field in build_attestation_fields:
            if attestation.get(field) != manifest[field]:
                refuse(
                    f"{artifact.path}: attestation {field} disagrees with manifest"
                )
        if manifest["kind"] == "optional":
            for field in (
                "required_core_artifact_blake3",
                "required_core_identity",
                "required_core_manifest_blake3",
            ):
                if attestation.get(field) != manifest[field]:
                    refuse(
                        f"{artifact.path}: attestation {field} disagrees with manifest"
                    )

        entries.append(
            {
                "artifact": artifact,
                "artifact_blake3": artifact_digest,
                "manifest": manifest,
                "manifest_blake3": tool_digest(tool, manifest_source),
                "structural_format": structural_format,
            }
        )

    cores = [entry for entry in entries if entry["manifest"]["kind"] == "core"]
    for entry in entries:
        manifest = entry["manifest"]
        if manifest["kind"] != "optional":
            continue
        matching = [
            core
            for core in cores
            if core["artifact_blake3"]
            == manifest["required_core_artifact_blake3"]
        ]
        if len(matching) != 1:
            refuse(
                f"{entry['artifact'].path}: required core artifact digest selects "
                f"{len(matching)} supplied core artifacts"
            )
        core = matching[0]
        if entry["structural_format"] != core["structural_format"]:
            refuse(
                f"{entry['artifact'].path}: provider/core structural formats disagree"
            )
        if core["manifest_blake3"] != manifest["required_core_manifest_blake3"]:
            refuse(
                f"{entry['artifact'].path}: required_core_manifest_blake3 disagrees "
                "with the supplied core manifest bytes"
            )

    return result


def validate_artifact_groups(
    tool: PinnedFile,
    pairs: list[dict[str, Any]],
) -> dict[str, Any]:
    groups: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for pair in pairs:
        manifest = read_manifest(pair["manifest"])
        key = (
            string(manifest, "target", pair["manifest"].path),
            string(manifest, "profile", pair["manifest"].path),
        )
        groups.setdefault(key, []).append(pair)
    results = [
        validate_artifact_pairs(tool, group)
        for _, group in sorted(groups.items())
    ]
    if len(results) == 1:
        return results[0]
    return {
        "component_sets": results,
        "schema": 1,
    }


def validate_derived_lipo(
    tool: PinnedFile,
    artifact: PinnedFile,
    inputs: list[PinnedFile],
) -> None:
    if len(inputs) < 2:
        refuse(f"{artifact.path}: derived lipo payload requires at least two inputs")
    with tempfile.TemporaryDirectory(prefix="nmp-pinned-lipo-") as temporary:
        directory = pathlib.Path(temporary)
        snapshot_paths: list[pathlib.Path] = []
        for index, source in enumerate(inputs):
            destination = directory / f"input-{index}{source.path.suffix}"
            try:
                descriptor = os.open(
                    destination,
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | getattr(os, "O_CLOEXEC", 0)
                    | getattr(os, "O_NOFOLLOW", 0),
                    0o400,
                )
            except OSError as error:
                refuse(
                    f"{source.path}: cannot create lipo input snapshot: {error}"
                )
            try:
                copy_pinned_file(source, descriptor)
                os.fsync(descriptor)
                os.fchmod(descriptor, 0o400)
            finally:
                os.close(descriptor)
            snapshot_paths.append(destination)
        output = directory / artifact.path.name
        with executable_snapshot(tool) as executable:
            try:
                result = subprocess.run(
                    [
                        str(executable),
                        "-create",
                        *(str(path) for path in snapshot_paths),
                        "-output",
                        str(output),
                    ],
                    check=False,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
            except OSError as error:
                refuse(f"{artifact.path}: cannot execute pinned lipo tool: {error}")
        if result.returncode != 0:
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            refuse(f"{artifact.path}: pinned lipo reconstruction refused: {detail}")
        try:
            descriptor = os.open(
                output,
                os.O_RDONLY
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
            )
            status = os.fstat(descriptor)
        except OSError as error:
            refuse(f"{artifact.path}: cannot pin reconstructed lipo payload: {error}")
        reconstructed = PinnedFile(output, descriptor, status, artifact.parent)
        try:
            if not pinned_bytes_equal(artifact, reconstructed):
                refuse(
                    f"{artifact.path}: final payload disagrees with pinned thin-input "
                    "lipo reconstruction"
                )
        finally:
            os.close(descriptor)


def main(arguments: list[str]) -> int:
    if not arguments:
        print(
            f"usage: {pathlib.Path(sys.argv[0]).name} MANIFEST...\n"
            f"   or: {pathlib.Path(sys.argv[0]).name} --witness-tool TOOL "
            "--artifact ARTIFACT MANIFEST...\n"
            f"   or: {pathlib.Path(sys.argv[0]).name} --metadata-audit-tool "
            "TOOL --artifact ARTIFACT",
            file=sys.stderr,
        )
        return 2
    registry = PinRegistry()
    try:
        if arguments[0] == "--metadata-audit-tool":
            if len(arguments) != 4 or arguments[2] != "--artifact":
                refuse(
                    "metadata audit usage is --metadata-audit-tool TOOL "
                    "--artifact ARTIFACT"
                )
            audit = registry.pin_file(
                pathlib.Path(arguments[1]),
                executable=True,
            )
            artifact = registry.pin_file(pathlib.Path(arguments[3]))
            mutation_hook("sources-pinned")
            registry.revalidate()
            run_metadata_audit(audit, artifact)
            registry.revalidate()
            result = {
                "artifact": str(artifact.path),
                "metadata_audit": str(audit.path),
                "schema": 1,
            }
        elif arguments[0] == "--witness-tool":
            if len(arguments) < 5:
                refuse("artifact-backed verification requires TOOL and artifact pairs")
            tool = registry.pin_file(
                pathlib.Path(arguments[1]),
                executable=True,
            )
            remainder = arguments[2:]
            path_pairs: list[dict[str, Any]] = []
            derived_paths: list[dict[str, Any]] = []
            publish_source: pathlib.Path | None = None
            publish_destination: pathlib.Path | None = None
            while remainder:
                if remainder[0] == "--publish-tree":
                    if publish_source is not None:
                        refuse("duplicate --publish-tree")
                    if len(remainder) != 3:
                        refuse("--publish-tree SOURCE_DIR DEST_DIR must be last")
                    publish_source = pathlib.Path(remainder[1])
                    publish_destination = pathlib.Path(remainder[2])
                    remainder = []
                    break
                if remainder[0] == "--derived-lipo-payload":
                    if (
                        len(remainder) < 7
                        or remainder[2] != "--lipo-tool"
                    ):
                        refuse(
                            "derived lipo usage is --derived-lipo-payload ARTIFACT "
                            "--lipo-tool TOOL --lipo-input INPUT..."
                        )
                    derived: dict[str, Any] = {
                        "artifact": pathlib.Path(remainder[1]),
                        "tool": pathlib.Path(remainder[3]),
                        "inputs": [],
                    }
                    remainder = remainder[4:]
                    while remainder and remainder[0] == "--lipo-input":
                        if len(remainder) < 2:
                            refuse("--lipo-input requires one path")
                        derived["inputs"].append(pathlib.Path(remainder[1]))
                        remainder = remainder[2:]
                    if len(derived["inputs"]) < 2:
                        refuse("derived lipo payload requires at least two inputs")
                    derived_paths.append(derived)
                    continue
                if len(remainder) < 3 or remainder[0] != "--artifact":
                    refuse("expected repeated --artifact ARTIFACT MANIFEST pairs")
                pair: dict[str, Any] = {
                    "artifact": pathlib.Path(remainder[1]),
                    "manifest": pathlib.Path(remainder[2]),
                }
                remainder = remainder[3:]
                while (
                    remainder
                    and remainder[0]
                    not in (
                        "--artifact",
                        "--derived-lipo-payload",
                        "--publish-tree",
                    )
                ):
                    if remainder[0] == "--publish-payload":
                        if pair.get("publish_payload"):
                            refuse("duplicate --publish-payload")
                        pair["publish_payload"] = True
                        remainder = remainder[1:]
                        continue
                    if len(remainder) < 2:
                        refuse("artifact option requires one path")
                    option = remainder[0]
                    field = {
                        "--witness": "witness",
                        "--metadata-audit": "metadata_audit",
                        "--forbid-symbols": "forbid_symbols",
                        "--localization-source": "localization_source",
                        "--localization-plan": "localization_plan",
                    }.get(option)
                    if field is None:
                        refuse(f"unknown artifact verification option {option!r}")
                    if field in pair:
                        refuse(f"duplicate artifact verification option {option}")
                    pair[field] = pathlib.Path(remainder[1])
                    remainder = remainder[2:]
                path_pairs.append(pair)
            if (publish_source is None) != (publish_destination is None):
                refuse("--publish-tree requires both source and destination")
            if publish_source is None and any(
                pair.get("publish_payload")
                for pair in path_pairs
            ):
                refuse("--publish-payload requires --publish-tree")
            pairs = pin_artifact_pairs(registry, path_pairs)
            derived_lipos = [
                {
                    "artifact": registry.pin_file(
                        derived["artifact"],
                        sealed=True,
                    ),
                    "tool": registry.pin_file(
                        derived["tool"],
                        executable=True,
                    ),
                    "inputs": [
                        registry.pin_file(path, sealed=True)
                        for path in derived["inputs"]
                    ],
                }
                for derived in derived_paths
            ]
            tree = (
                pin_tree(registry, publish_source)
                if publish_source is not None
                else None
            )
            mutation_hook("sources-pinned")
            registry.revalidate()
            result = validate_artifact_groups(tool, pairs)
            verified_artifact_identities = {
                pair["artifact"].identity
                for pair in pairs
            }
            for derived in derived_lipos:
                unknown_inputs = [
                    source.path
                    for source in derived["inputs"]
                    if source.identity not in verified_artifact_identities
                ]
                if unknown_inputs:
                    refuse(
                        f"{derived['artifact'].path}: lipo inputs are not verified "
                        f"component artifacts: {unknown_inputs}"
                    )
                validate_derived_lipo(
                    derived["tool"],
                    derived["artifact"],
                    derived["inputs"],
                )
            if tree is not None and publish_destination is not None:
                mutation_hook("sources-verified")
                registry.revalidate()
                if len(path_pairs) != len(pairs):
                    refuse("internal artifact pin count disagrees with parsed pairs")
                payload_identities = [
                    pair["artifact"].identity
                    for path_pair, pair in zip(path_pairs, pairs)
                    if path_pair.get("publish_payload")
                ]
                payload_identities.extend(
                    derived["artifact"].identity
                    for derived in derived_lipos
                )
                if not payload_identities:
                    refuse("--publish-tree requires at least one --publish-payload")
                publish_tree(
                    registry,
                    tree,
                    publish_destination,
                    payload_identities,
                )
            registry.revalidate()
        else:
            manifests = [
                registry.pin_file(pathlib.Path(argument))
                for argument in arguments
            ]
            result = verify(manifests)
            registry.revalidate()
    except Refusal as error:
        print(f"component-manifests: refused: {error}", file=sys.stderr)
        return 1
    finally:
        registry.close()
    print(json.dumps(result, separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
