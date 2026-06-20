#!/usr/bin/env python3
"""Opt-in integration tests for Git LFS behavior through the cache.

These tests use only Python's standard library and shell out to ``cargo``
and ``git``.  They are skipped unless ``RUN_GITHUB_INTEGRATION=1`` is set.

The suite verifies:
- LFS repos clone correctly through the cache (pointer files present)
- The LFS batch API returns 405 (unsupported)
- The upstream-URL workaround lets ``git lfs pull`` succeed
"""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]

DEFAULT_LFS_TEST_REPO = "github.com/charmbracelet/vhs"
LFS_TEST_REPO = os.environ.get("GIT_CACHE_LFS_TEST_REPO", DEFAULT_LFS_TEST_REPO)

LFS_POINTER_RE = re.compile(
    r"^version https://git-lfs\.github\.com/spec/v1\n"
    r"oid sha256:[0-9a-f]{64}\n"
    r"size \d+\n\Z",
)


def _run(
    cmd: list[str],
    *,
    cwd: Path = REPO_ROOT,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd))
    completed = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=300,
    )
    combined = (completed.stdout or "") + (completed.stderr or "")
    for line in combined.strip().splitlines()[:20]:
        print(line)
    if combined.strip().count("\n") > 20:
        print(f"  ... ({combined.strip().count(chr(10)) - 20} more lines)")
    if check:
        completed.check_returncode()
    return completed


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _is_lfs_pointer(content: str) -> bool:
    return bool(LFS_POINTER_RE.match(content))


def _find_lfs_pointer_files(tree: Path) -> list[Path]:
    """Return paths of files that contain LFS pointer text."""
    pointers: list[Path] = []
    for path in tree.rglob("*"):
        if not path.is_file():
            continue
        if path.parts and ".git" in path.parts:
            continue
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        if _is_lfs_pointer(text):
            pointers.append(path)
    return pointers


@unittest.skipUnless(
    os.environ.get("RUN_GITHUB_INTEGRATION") == "1",
    "set RUN_GITHUB_INTEGRATION=1 to test LFS behavior through the cache",
)
class LfsSmokeTest(unittest.TestCase):
    """Test Git LFS behavior through the read-through /git/ endpoint."""

    @classmethod
    def setUpClass(cls) -> None:
        # Gate on git-lfs being installed
        try:
            subprocess.run(
                ["git", "lfs", "version"],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=10,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired):
            raise unittest.SkipTest("git-lfs is not installed")

        base = Path(os.environ.get("TEST_TMPDIR", tempfile.gettempdir()))
        base.mkdir(parents=True, exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="git-cache-lfs-smoke-", dir=base))
        cls.port = _free_port()
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        cls.cache_root = cls.tmp / "cache"
        cls.object_root = cls.tmp / "object-store"
        cls.repo = LFS_TEST_REPO
        cls.owner_repo = cls.repo.removeprefix("github.com/")

        config_path = cls.tmp / "config.toml"
        config_path.write_text(
            f"""\
bind_addr = "127.0.0.1:{cls.port}"
cache_root = "{cls.cache_root}"
git_timeout_seconds = 300
max_git_output_bytes = 1073741824
rate_limit_per_minute = 0
allowed_upstream_hosts = ["github.com"]

[object_store]
kind = "local"
root = "{cls.object_root}"

[disk]
quota_bytes = 10737418240
min_free_bytes = 0

[git_remote]
commit_read_through = true
"""
        )

        _run(["cargo", "build", "-p", "git-cache-api"])

        git_tmp = cls.tmp / "git-tmp"
        git_tmp.mkdir(parents=True, exist_ok=True)

        env = os.environ.copy()
        env["RUST_LOG"] = "info"
        env["GIT_CACHE_CONFIG"] = str(config_path)
        env["TMPDIR"] = str(git_tmp)
        cls.server = subprocess.Popen(
            [str(REPO_ROOT / "target/debug/git-cache-api")],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        cls._wait_for_healthz()

    @classmethod
    def tearDownClass(cls) -> None:
        server = getattr(cls, "server", None)
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=10)
            if server.stdout is not None:
                tail = server.stdout.read()
                if tail.strip():
                    for line in tail.strip().splitlines()[-30:]:
                        print(line)

        tmp = getattr(cls, "tmp", None)
        if tmp is not None:
            shutil.rmtree(tmp, ignore_errors=True)

    @classmethod
    def _wait_for_healthz(cls) -> None:
        deadline = time.time() + 30
        url = f"{cls.base_url}/healthz"
        last_error: Exception | None = None

        while time.time() < deadline:
            if cls.server.poll() is not None:
                output = cls.server.stdout.read() if cls.server.stdout is not None else ""
                raise RuntimeError(f"git-cache-api exited early:\n{output}")
            try:
                with urllib.request.urlopen(url, timeout=1) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.URLError) as error:
                last_error = error
                time.sleep(0.25)

        raise TimeoutError(f"timed out waiting for {url}: {last_error}")

    def _git_url(self, repo: str | None = None) -> str:
        r = repo or self.repo
        return f"{self.base_url}/git/{r}.git"

    def _clone_env(self, *, skip_smudge: bool = False) -> dict[str, str]:
        env = os.environ.copy()
        if skip_smudge:
            env["GIT_LFS_SKIP_SMUDGE"] = "1"
        else:
            env.pop("GIT_LFS_SKIP_SMUDGE", None)
        return env

    # ── Test cases ────────────────────────────────────────────────────

    def test_clone_with_lfs_skip_smudge(self) -> None:
        """Clone with GIT_LFS_SKIP_SMUDGE=1: pointer files should remain."""
        clone_dir = self.tmp / "clone-skip-smudge"
        result = _run(
            [
                "git", "clone", "--depth", "1",
                self._git_url(), str(clone_dir),
            ],
            env=self._clone_env(skip_smudge=True),
            check=False,
        )
        self.assertEqual(result.returncode, 0, f"clone failed:\n{result.stderr}")

        entries = list(clone_dir.iterdir())
        self.assertTrue(
            len(entries) > 1,  # at least more than just .git
            f"working tree is empty: {entries}",
        )

        pointers = _find_lfs_pointer_files(clone_dir)
        self.assertTrue(
            len(pointers) > 0,
            "expected at least one LFS pointer file in the working tree",
        )

    def test_clone_without_skip_smudge(self) -> None:
        """Clone without skip-smudge: clone succeeds, LFS content not fetched."""
        clone_dir = self.tmp / "clone-no-skip"
        result = _run(
            [
                "git", "clone", "--depth", "1",
                self._git_url(), str(clone_dir),
            ],
            env=self._clone_env(skip_smudge=False),
            check=False,
        )
        # git-lfs may or may not cause a non-zero exit depending on version;
        # the key assertion is that pointer files remain as pointers.
        if result.returncode != 0:
            print(f"clone exited {result.returncode} (acceptable for some git-lfs versions)")

        stderr_combined = (result.stdout or "") + (result.stderr or "")
        stderr_lower = stderr_combined.lower()
        has_lfs_errors = any(
            marker in stderr_lower
            for marker in ["batch response", "405", "smudge filter", "smudge error", "lfs"]
        )
        self.assertTrue(
            has_lfs_errors,
            "expected LFS-related error messages in clone output",
        )
        print("LFS error messages detected (expected):")
        for line in stderr_combined.splitlines():
            if any(m in line.lower() for m in ["lfs", "smudge", "batch", "405"]):
                print(f"  {line}")

        self.assertTrue(
            clone_dir.exists() and any(clone_dir.iterdir()),
            "clone directory should exist and be non-empty",
        )

        # Some git-lfs versions make the checkout fail entirely; recover the
        # working tree with GIT_LFS_SKIP_SMUDGE=1 so we can verify pointers.
        pointers = _find_lfs_pointer_files(clone_dir)
        if not pointers and (clone_dir / ".git").is_dir():
            _run(
                ["git", "restore", "--source=HEAD", ":/"],
                cwd=clone_dir,
                env=self._clone_env(skip_smudge=True),
                check=False,
            )
            pointers = _find_lfs_pointer_files(clone_dir)

        self.assertTrue(
            len(pointers) > 0,
            "expected LFS-tracked files to remain as pointers (not real content)",
        )

    def test_lfs_batch_api_returns_405(self) -> None:
        """The LFS batch endpoint should return 405 Method Not Allowed."""
        url = f"{self._git_url()}/info/lfs/objects/batch"
        body = json.dumps({
            "operation": "download",
            "transfers": ["basic"],
            "objects": [{"oid": "abc123", "size": 100}],
        }).encode()
        req = urllib.request.Request(
            url,
            data=body,
            method="POST",
            headers={"Content-Type": "application/vnd.git-lfs+json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                self.fail(
                    f"expected 405 but got {resp.status}"
                )
        except urllib.error.HTTPError as exc:
            self.assertEqual(
                exc.code, 405,
                f"expected 405 Method Not Allowed, got {exc.code}: {exc.read().decode(errors='replace')[:500]}",
            )

    def test_lfs_pull_with_upstream_url_workaround(self) -> None:
        """Clone with skip-smudge, set lfs.url to upstream, then git lfs pull."""
        clone_dir = self.tmp / "clone-lfs-pull-workaround"
        result = _run(
            [
                "git", "clone", "--depth", "1",
                self._git_url(), str(clone_dir),
            ],
            env=self._clone_env(skip_smudge=True),
            check=False,
        )
        self.assertEqual(result.returncode, 0, f"clone failed:\n{result.stderr}")

        pointers_before = _find_lfs_pointer_files(clone_dir)
        self.assertTrue(
            len(pointers_before) > 0,
            "expected at least one LFS pointer file before lfs pull",
        )

        # Point lfs.url at the real upstream LFS server
        _run(
            [
                "git", "config", "lfs.url",
                f"https://github.com/{self.owner_repo}.git/info/lfs",
            ],
            cwd=clone_dir,
        )

        pull_result = _run(
            ["git", "lfs", "pull"],
            cwd=clone_dir,
            check=False,
        )
        self.assertEqual(
            pull_result.returncode, 0,
            f"git lfs pull failed:\n{pull_result.stderr}",
        )

        # Verify at least one former pointer is now real content
        resolved = 0
        for ptr_path in pointers_before:
            if not ptr_path.exists():
                continue
            try:
                text = ptr_path.read_text(errors="replace")
            except OSError:
                continue
            if not _is_lfs_pointer(text):
                resolved += 1

        self.assertGreater(
            resolved, 0,
            "expected at least one LFS pointer to be resolved to real content after lfs pull",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
