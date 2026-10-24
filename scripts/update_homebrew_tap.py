#!/usr/bin/env python3
"""Bump the fj formula in the cloudiful Homebrew tap for a published release.

The script is trusted local code from the source repository. It never
executes or extracts release archives: it only hashes the downloaded files
and rewrites the tap formula text.

Validation fails closed. Every check runs before the formula is written, so a
rejected release or manifest leaves the tap formula untouched.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

REPO_SLUG = "cloudiful/forgejo-cli"
HOMEPAGE = "https://github.com/cloudiful/forgejo-cli"
DOWNLOAD_URL_PREFIX = f"https://github.com/{REPO_SLUG}/releases/download/"

TAG_RE = re.compile(
    r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-cloudiful\.([1-9][0-9]*)$"
)
VERSION_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-cloudiful\.([1-9][0-9]*)$"
)
CLASS_RE = re.compile(r"^\s*class\s+Fj\s+<\s+Formula\s*$")
HOMEPAGE_RE = re.compile(r'^\s*homepage\s+"([^"]*)"')
VERSION_RE_LINE = re.compile(r'^(\s*version\s+")([^"]*)(")')
URL_RE = re.compile(r'^\s*url\s+"([^"]*)"')
SHA256_RE_LINE = re.compile(r'^(\s*sha256\s+")([0-9a-f]{64})(")')
DOWNLOAD_URL_RE = re.compile(rf"^{re.escape(DOWNLOAD_URL_PREFIX)}(v[^/\s]+)/([^/\s]+)$")
MANIFEST_LINE_RE = re.compile(r"^([0-9a-f]{64}) [ *](\S+)$")

FORMULA_ASSETS = (
    "forgejo-cli-aarch64-linux.tar.gz",
    "forgejo-cli-aarch64-macos.tar.gz",
    "forgejo-cli-x86_64-linux.tar.gz",
)
MANIFEST_ASSETS = FORMULA_ASSETS + ("forgejo-cli-x86_64-windows.zip",)


class UpdateError(Exception):
    """A validation failure; the tap formula must stay untouched."""


@dataclass(frozen=True)
class AssetBinding:
    asset: str
    url: str
    url_index: int
    sha_index: int


@dataclass(frozen=True)
class FormulaState:
    lines: tuple[str, ...]
    version: str
    version_index: int
    bindings: tuple[AssetBinding, ...]


def parse_tag(tag: str) -> tuple[str, tuple[int, int, int, int]]:
    """Return the version string and numeric tuple of a strict fork tag."""
    if not TAG_RE.match(tag):
        raise UpdateError(f"tag {tag!r} is not strict vX.Y.Z-cloudiful.N")
    version = tag[1:]
    return version, parse_version(version)


def parse_version(version: str) -> tuple[int, int, int, int]:
    match = VERSION_RE.match(version)
    if not match:
        raise UpdateError(f"version {version!r} is not strict X.Y.Z-cloudiful.N")
    return tuple(int(part) for part in match.groups())


def validate_formula(text: str) -> FormulaState:
    """Validate the fj formula shape and return its parsed state."""
    lines = tuple(text.splitlines(keepends=True))

    class_lines = [index for index, line in enumerate(lines) if CLASS_RE.match(line)]
    if len(class_lines) != 1:
        raise UpdateError(
            f"expected exactly one 'class Fj < Formula' line, found {len(class_lines)}"
        )
    first_content = next(
        (index for index, line in enumerate(lines) if line.strip()), None
    )
    if first_content != class_lines[0]:
        raise UpdateError("'class Fj < Formula' must be the first non-blank line")

    homepage_lines = [line for line in lines if HOMEPAGE_RE.match(line)]
    if len(homepage_lines) != 1:
        raise UpdateError(
            f"expected exactly one homepage line, found {len(homepage_lines)}"
        )
    homepage = HOMEPAGE_RE.match(homepage_lines[0]).group(1)
    if homepage != HOMEPAGE:
        raise UpdateError(f"unexpected homepage {homepage!r}")

    version_lines = [
        (index, VERSION_RE_LINE.match(line))
        for index, line in enumerate(lines)
        if VERSION_RE_LINE.match(line)
    ]
    if len(version_lines) != 1:
        raise UpdateError(
            f"expected exactly one version line, found {len(version_lines)}"
        )
    version_index, version_match = version_lines[0]
    version = version_match.group(2)
    parse_version(version)

    url_lines = [(index, line) for index, line in enumerate(lines) if URL_RE.match(line)]
    if len(url_lines) != 3:
        raise UpdateError(f"expected exactly 3 download url lines, found {len(url_lines)}")
    sha_lines = [
        (index, line) for index, line in enumerate(lines) if SHA256_RE_LINE.match(line)
    ]
    if len(sha_lines) != 3:
        raise UpdateError(f"expected exactly 3 sha256 lines, found {len(sha_lines)}")

    bindings: list[AssetBinding] = []
    assets: set[str] = set()
    for url_index, url_line in url_lines:
        url = URL_RE.match(url_line).group(1)
        url_match = DOWNLOAD_URL_RE.match(url)
        if not url_match:
            raise UpdateError(f"unexpected download url {url!r}")
        url_tag, asset = url_match.group(1), url_match.group(2)
        if url_tag != f"v{version}":
            raise UpdateError(
                f"download url {url!r} does not match formula version {version!r}"
            )
        if asset not in FORMULA_ASSETS:
            raise UpdateError(f"unexpected download asset {asset!r}")
        if asset in assets:
            raise UpdateError(f"duplicate download asset {asset!r}")
        assets.add(asset)
        sha_index = url_index + 1
        if sha_index >= len(lines) or not SHA256_RE_LINE.match(lines[sha_index]):
            raise UpdateError(
                f"download url {url!r} is not followed by an adjacent sha256 line"
            )
        bindings.append(
            AssetBinding(asset=asset, url=url, url_index=url_index, sha_index=sha_index)
        )
    if assets != set(FORMULA_ASSETS):
        missing = sorted(set(FORMULA_ASSETS) - assets)
        raise UpdateError(f"formula is missing download assets: {', '.join(missing)}")

    return FormulaState(
        lines=lines,
        version=version,
        version_index=version_index,
        bindings=tuple(bindings),
    )


def parse_manifest(text: str) -> dict[str, str]:
    """Parse a SHA256SUMS manifest into an asset name to digest mapping."""
    digests: dict[str, str] = {}
    for number, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.strip()
        if not line:
            continue
        match = MANIFEST_LINE_RE.match(line)
        if not match:
            raise UpdateError(f"SHA256SUMS line {number} is not a valid checksum entry")
        digest, name = match.group(1), match.group(2)
        if name in digests:
            raise UpdateError(f"SHA256SUMS contains a duplicate entry for {name}")
        if "/" in name or "\\" in name or name in (".", "..") or ".." in name:
            raise UpdateError(f"SHA256SUMS entry {name!r} is not a plain file name")
        if name not in MANIFEST_ASSETS:
            raise UpdateError(f"SHA256SUMS contains an unexpected asset {name!r}")
        digests[name] = digest
    missing = [asset for asset in MANIFEST_ASSETS if asset not in digests]
    if missing:
        raise UpdateError(f"SHA256SUMS is missing entries: {', '.join(missing)}")
    return digests


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def render_formula(
    state: FormulaState,
    version: str,
    tag: str,
    digests: dict[str, str],
) -> str:
    """Return the formula text with only version, urls, and checksums changed."""
    lines = list(state.lines)

    version_line = lines[state.version_index]
    lines[state.version_index] = VERSION_RE_LINE.sub(
        lambda match: match.group(1) + version + match.group(3), version_line, count=1
    )

    for binding in state.bindings:
        new_url = f"{DOWNLOAD_URL_PREFIX}{tag}/{binding.asset}"
        lines[binding.url_index] = lines[binding.url_index].replace(
            binding.url, new_url, 1
        )
        sha_line = lines[binding.sha_index]
        lines[binding.sha_index] = SHA256_RE_LINE.sub(
            lambda match: match.group(1) + digests[binding.asset] + match.group(3),
            sha_line,
            count=1,
        )

    return "".join(lines)


def write_text_atomic(path: Path, content: str) -> None:
    handle, temporary = tempfile.mkstemp(
        dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp"
    )
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as stream:
            stream.write(content)
        os.replace(temporary, path)
    except BaseException:
        if os.path.exists(temporary):
            os.unlink(temporary)
        raise


def update(
    formula_path: Path,
    tag: str,
    sums_path: Path,
    assets_dir: Path,
) -> str:
    """Validate the release inputs and rewrite the formula when needed."""
    version, target = parse_tag(tag)

    original = formula_path.read_text(encoding="utf-8")
    state = validate_formula(original)
    existing = parse_version(state.version)
    if target < existing:
        raise UpdateError(
            f"refusing to downgrade the formula from {state.version} to {version}"
        )

    digests = parse_manifest(sums_path.read_text(encoding="utf-8"))

    for asset in FORMULA_ASSETS:
        asset_path = assets_dir / asset
        if not asset_path.is_file():
            raise UpdateError(f"missing downloaded asset {asset}")
        actual = sha256_file(asset_path)
        if actual != digests[asset]:
            raise UpdateError(
                f"checksum mismatch for {asset}: manifest {digests[asset]}, actual {actual}"
            )

    updated = render_formula(state, version, tag, digests)
    if updated == original:
        return f"formula already at {state.version}; no change"
    write_text_atomic(formula_path, updated)
    return f"updated formula to {version}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Update the fj Homebrew tap formula for a published fork release."
    )
    parser.add_argument("--formula", required=True, type=Path, help="path to Formula/fj.rb")
    parser.add_argument("--tag", required=True, help="release tag like v0.7.0-cloudiful.2")
    parser.add_argument("--sums", required=True, type=Path, help="path to SHA256SUMS")
    parser.add_argument(
        "--assets-dir", required=True, type=Path, help="directory with the downloaded assets"
    )
    args = parser.parse_args(argv)

    try:
        message = update(args.formula, args.tag, args.sums, args.assets_dir)
    except (UpdateError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(message)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
