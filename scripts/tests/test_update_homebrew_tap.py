#!/usr/bin/env python3
"""Offline tests for the Homebrew tap bump script."""

from __future__ import annotations

import hashlib
import io
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import update_homebrew_tap as tap  # noqa: E402

FORMULA_ASSETS = tap.FORMULA_ASSETS
MANIFEST_ASSETS = tap.MANIFEST_ASSETS

PROJECT_ROOT = Path(__file__).resolve().parents[2]
FIXTURE_ROOT = PROJECT_ROOT / ".cache" / "ai" / "ci-release"

FORMULA_TEMPLATE = (
    '''class Fj < Formula
  desc "Command-line interface for Forgejo (cloudiful fork)"
  homepage "https://github.com/cloudiful/forgejo-cli"
  version "@version@"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    depends_on arch: :arm64

    url "https://github.com/cloudiful/forgejo-cli/releases/download/@tag@/forgejo-cli-aarch64-macos.tar.gz"
    sha256 "@macos_sha@"
    depends_on "openssl@3"
  end

  on_linux do
    on_intel do
      url "https://github.com/cloudiful/forgejo-cli/releases/download/@tag@/forgejo-cli-x86_64-linux.tar.gz"
      sha256 "@linux_sha@"
    end

    on_arm do
      url "https://github.com/cloudiful/forgejo-cli/releases/download/@tag@/forgejo-cli-aarch64-linux.tar.gz"
      sha256 "@arm_sha@"
    end
  end

  conflicts_with "forgejo-cli", because: "both install the fj executable"

  livecheck do
    url :stable
    strategy :github_latest
    regex(/^v?(\\d+\\.\\d+\\.\\d+-cloudiful\\.[1-9]\\d*)$/i)
  end

  def install
    bin.install "fj"
    doc.install "LICENSE-MIT", "LICENSE-APACHE"
    generate_completions_from_executable(bin/"fj", "completion")
  end

  def caveats
    if OS.mac?
      <<~EOS
        The macOS binary links Homebrew's openssl@3 at the standard
        /opt/homebrew prefix; custom Homebrew prefixes are not supported.
      EOS
    elsif OS.linux?
      <<~EOS
        The Linux binary requires glibc 2.39 or newer plus system OpenSSL 3
        and zlib libraries.
      EOS
    end
  end

  test do
    assert_match "v#@version@", shell_output("#{bin}/fj version")
  end
end
'''
)


def fake_digest(seed: str) -> str:
    return hashlib.sha256(seed.encode("utf-8")).hexdigest()


@dataclass
class Workspace:
    formula_path: Path
    assets_dir: Path
    sums_path: Path

    def write_assets(self, seed: str = "release") -> dict[str, str]:
        digests: dict[str, str] = {}
        for asset in MANIFEST_ASSETS:
            data = f"{seed}-{asset}".encode("utf-8")
            (self.assets_dir / asset).write_bytes(data)
            digests[asset] = hashlib.sha256(data).hexdigest()
        return digests

    def write_manifest(self, digests: dict[str, str]) -> None:
        lines = [f"{digests[asset]}  {asset}" for asset in MANIFEST_ASSETS]
        self.sums_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

    def write_formula(
        self,
        version: str = "0.7.0-cloudiful.1",
        digests: dict[str, str] | None = None,
        tag: str | None = None,
    ) -> str:
        if digests is None:
            digests = {asset: fake_digest(f"old-{asset}") for asset in FORMULA_ASSETS}
        text = (
            FORMULA_TEMPLATE.replace("@version@", version)
            .replace("@tag@", tag if tag is not None else f"v{version}")
            .replace("@macos_sha@", digests["forgejo-cli-aarch64-macos.tar.gz"])
            .replace("@linux_sha@", digests["forgejo-cli-x86_64-linux.tar.gz"])
            .replace("@arm_sha@", digests["forgejo-cli-aarch64-linux.tar.gz"])
        )
        self.formula_path.write_text(text, encoding="utf-8")
        return text

    def run_update(self, tag: str) -> str:
        return tap.update(self.formula_path, tag, self.sums_path, self.assets_dir)

    def run_cli(self, tag: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = tap.main(
                [
                    "--formula",
                    str(self.formula_path),
                    "--tag",
                    tag,
                    "--sums",
                    str(self.sums_path),
                    "--assets-dir",
                    str(self.assets_dir),
                ]
            )
        return code, stdout.getvalue(), stderr.getvalue()


class UpdateHomebrewTapTest(unittest.TestCase):
    def setUp(self) -> None:
        FIXTURE_ROOT.mkdir(parents=True, exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(prefix="tap-test-", dir=FIXTURE_ROOT)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.counter = 0

    def workspace(self) -> Workspace:
        self.counter += 1
        base = self.root / f"ws-{self.counter}"
        (base / "Formula").mkdir(parents=True)
        (base / "assets").mkdir()
        return Workspace(
            formula_path=base / "Formula" / "fj.rb",
            assets_dir=base / "assets",
            sums_path=base / "SHA256SUMS",
        )

    def test_bump_updates_only_version_urls_and_hashes(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)

        message = workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("0.7.0-cloudiful.2", message)

        updated = workspace.formula_path.read_text(encoding="utf-8")
        before = original.splitlines()
        after = updated.splitlines()
        self.assertEqual(len(before), len(after))
        changed = [
            index for index, pair in enumerate(zip(before, after)) if pair[0] != pair[1]
        ]
        self.assertEqual(len(changed), 7)
        self.assertIn('version "0.7.0-cloudiful.2"', updated)
        self.assertNotIn("download/v0.7.0-cloudiful.1/", updated)
        for asset in FORMULA_ASSETS:
            self.assertIn(f"download/v0.7.0-cloudiful.2/{asset}", updated)
            self.assertIn(f'"{digests[asset]}"', updated)

    def test_full_suffix_and_upstream_next_are_allowed(self) -> None:
        for tag in ("v0.8.0-cloudiful.1", "v0.7.1-cloudiful.1", "v1.0.0-cloudiful.10"):
            with self.subTest(tag=tag):
                workspace = self.workspace()
                workspace.write_formula()
                digests = workspace.write_assets()
                workspace.write_manifest(digests)
                workspace.run_update(tag)
                updated = workspace.formula_path.read_text(encoding="utf-8")
                self.assertIn(f'version "{tag[1:]}"', updated)
                self.assertIn(f"download/{tag}/", updated)

    def test_same_version_is_a_noop(self) -> None:
        workspace = self.workspace()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        original = workspace.write_formula(digests=digests)
        before = workspace.formula_path.stat().st_mtime_ns

        message = workspace.run_update("v0.7.0-cloudiful.1")

        self.assertIn("no change", message)
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)
        self.assertEqual(workspace.formula_path.stat().st_mtime_ns, before)

    def test_downgrade_is_rejected(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula(version="0.7.0-cloudiful.2")
        digests = workspace.write_assets()
        workspace.write_manifest(digests)

        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.1")
        self.assertIn("downgrade", str(caught.exception))
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)

    def test_invalid_tags_are_rejected(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        for tag in (
            "v0.7.0-cloudiful.01",
            "v0.7.0-cloudiful.0",
            "v00.7.0-cloudiful.1",
            "v0.07.0-cloudiful.1",
            "0.7.0-cloudiful.1",
            "v0.7.0-cloudiful",
            "v0.7.0-fork.1",
            "v0.7.0-cloudiful.1-extra",
            "v0.7.0+build",
            "",
        ):
            with self.subTest(tag=tag):
                with self.assertRaises(tap.UpdateError):
                    workspace.run_update(tag)
                self.assertEqual(
                    workspace.formula_path.read_text(encoding="utf-8"), original
                )

    def test_terminal_newline_and_crlf_tags_are_rejected(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        before = workspace.formula_path.stat().st_mtime_ns

        for tag in (
            "v0.7.0-cloudiful.2\n",
            "v0.7.0-cloudiful.2\r\n",
            "v0.7.0-cloudiful.2\r",
            "v0.7.0-cloudiful.2 ",
            " v0.7.0-cloudiful.2",
        ):
            with self.subTest(tag=tag):
                with self.assertRaises(tap.UpdateError):
                    tap.parse_tag(tag)
                with self.assertRaises(tap.UpdateError):
                    workspace.run_update(tag)
                self.assertEqual(
                    workspace.formula_path.read_text(encoding="utf-8"), original
                )

        for version in (
            "0.7.0-cloudiful.2\n",
            "0.7.0-cloudiful.2\r\n",
            "0.7.0-cloudiful.2\r",
        ):
            with self.subTest(version=version):
                with self.assertRaises(tap.UpdateError):
                    tap.parse_version(version)

        self.assertEqual(workspace.formula_path.stat().st_mtime_ns, before)
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)

    def test_manifest_problems_are_rejected(self) -> None:
        workspace = self.workspace()
        workspace.write_formula()
        digests = workspace.write_assets()

        missing = {
            asset: digest
            for asset, digest in digests.items()
            if asset != MANIFEST_ASSETS[0]
        }
        workspace.sums_path.write_text(
            "\n".join(f"{digest}  {asset}" for asset, digest in missing.items()) + "\n",
            encoding="utf-8",
        )
        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("missing entries", str(caught.exception))

        duplicated = "\n".join(
            [f"{digests[asset]}  {asset}" for asset in MANIFEST_ASSETS]
            + [f"{digests[MANIFEST_ASSETS[0]]}  {MANIFEST_ASSETS[0]}"]
        )
        workspace.sums_path.write_text(duplicated + "\n", encoding="utf-8")
        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("duplicate", str(caught.exception))

        workspace.sums_path.write_text("not-a-checksum-line\n", encoding="utf-8")
        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("valid checksum entry", str(caught.exception))

        workspace.sums_path.write_text(
            f"{'z' * 64}  {MANIFEST_ASSETS[0]}\n", encoding="utf-8"
        )
        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("valid checksum entry", str(caught.exception))

        workspace.sums_path.write_text(
            f"{digests[MANIFEST_ASSETS[0]]}  ../{MANIFEST_ASSETS[0]}\n", encoding="utf-8"
        )
        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("plain file name", str(caught.exception))

    def test_asset_digest_mismatch_is_rejected(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        (workspace.assets_dir / "forgejo-cli-x86_64-linux.tar.gz").write_bytes(b"tampered")

        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("checksum mismatch", str(caught.exception))
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)

    def test_missing_asset_is_rejected(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        (workspace.assets_dir / "forgejo-cli-aarch64-macos.tar.gz").unlink()

        with self.assertRaises(tap.UpdateError) as caught:
            workspace.run_update("v0.7.0-cloudiful.2")
        self.assertIn("missing downloaded asset", str(caught.exception))
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)

    def test_unexpected_formula_shapes_are_rejected(self) -> None:
        cases = {
            "wrong class": (
                lambda text: text.replace("class Fj < Formula", "class Fj2 < Formula"),
                "class Fj < Formula",
            ),
            "wrong homepage": (
                lambda text: text.replace(
                    'homepage "https://github.com/cloudiful/forgejo-cli"',
                    'homepage "https://github.com/forgejo-contrib/forgejo-cli"',
                ),
                "homepage",
            ),
            "old url version": (
                lambda text: text.replace(
                    "download/v0.7.0-cloudiful.1/forgejo-cli-aarch64-linux.tar.gz",
                    "download/v0.7.0-cloudiful.9/forgejo-cli-aarch64-linux.tar.gz",
                ),
                "does not match formula version",
            ),
            "extra url": (
                lambda text: text.replace(
                    '  conflicts_with "forgejo-cli"',
                    '    url "https://example.com/extra.tar.gz"\n'
                    f'    sha256 "{fake_digest("extra")}"\n'
                    '  end\n\n  conflicts_with "forgejo-cli"',
                ),
                "exactly 3 download url lines",
            ),
            "duplicate asset": (
                lambda text: text.replace(
                    "download/v0.7.0-cloudiful.1/forgejo-cli-aarch64-linux.tar.gz",
                    "download/v0.7.0-cloudiful.1/forgejo-cli-aarch64-macos.tar.gz",
                ),
                "duplicate download asset",
            ),
            "extra sha256": (
                lambda text: text.replace(
                    '  conflicts_with "forgejo-cli"',
                    f'    sha256 "{fake_digest("stray")}"\n'
                    '  end\n\n  conflicts_with "forgejo-cli"',
                ),
                "exactly 3 sha256 lines",
            ),
        }

        for name, (mutate, expected) in cases.items():
            with self.subTest(case=name):
                workspace = self.workspace()
                digests = workspace.write_assets()
                workspace.write_manifest(digests)
                original = workspace.write_formula()
                mutated = mutate(original)
                self.assertNotEqual(original, mutated)
                workspace.formula_path.write_text(mutated, encoding="utf-8")
                with self.assertRaises(tap.UpdateError) as caught:
                    workspace.run_update("v0.7.0-cloudiful.2")
                self.assertIn(expected, str(caught.exception))
                self.assertEqual(
                    workspace.formula_path.read_text(encoding="utf-8"), mutated
                )

    def test_failure_leaves_the_formula_untouched(self) -> None:
        workspace = self.workspace()
        original = workspace.write_formula()
        workspace.write_assets()
        workspace.sums_path.write_text("broken\n", encoding="utf-8")

        code, _, stderr = workspace.run_cli("v0.7.0-cloudiful.2")

        self.assertEqual(code, 1)
        self.assertIn("error:", stderr)
        self.assertEqual(workspace.formula_path.read_text(encoding="utf-8"), original)

    def test_cli_success_returns_zero(self) -> None:
        workspace = self.workspace()
        digests = workspace.write_assets()
        workspace.write_manifest(digests)
        workspace.write_formula()

        code, stdout, _ = workspace.run_cli("v0.7.0-cloudiful.2")

        self.assertEqual(code, 0)
        self.assertIn("0.7.0-cloudiful.2", stdout)
        self.assertIn(
            'version "0.7.0-cloudiful.2"',
            workspace.formula_path.read_text(encoding="utf-8"),
        )


if __name__ == "__main__":
    unittest.main()
