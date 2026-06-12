#!/usr/bin/env python3
"""Opt-in integration tests for the read-through Git remote against real
public repositories, including high-commit-count repos (200k+ commits).

These tests use only Python's standard library and shell out to ``cargo``
and ``git``.  They are skipped unless ``RUN_GITHUB_INTEGRATION=1`` is set.

The test suite exercises the ``/git/{*repo_path}`` direct-remote endpoint
rather than the ``/v1/materialize`` API.  Because the target repos can be
very large, the tests use ``--depth 1`` clones and ``git ls-remote`` to keep
wall-clock time manageable while still exercising the full Smart HTTP
protocol round-trip.
"""

from __future__ import annotations

import os
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
MINIO_BUCKET = os.environ.get("GIT_CACHE_S3_BUCKET", "gitmirrorcache-test")
MINIO_PREFIX = os.environ.get("GIT_CACHE_S3_PREFIX", "python-integration")
USE_MINIO_BACKEND = os.environ.get("GIT_CACHE_USE_MINIO_BACKEND") == "1"

# Repos with 200k+ commits on their default branch.
HIGH_COMMIT_REPOS: list[dict[str, str]] = [
    {"repo": "github.com/torvalds/linux", "branch": "master"},
    {"repo": "github.com/llvm/llvm-project", "branch": "main"},
    {"repo": "github.com/gcc-mirror/gcc", "branch": "master"},
]


def run(
    cmd: list[str], *, cwd: Path = REPO_ROOT, env: dict[str, str] | None = None
) -> str:
    print("+", " ".join(cmd))
    completed = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if completed.stdout.strip():
        for line in completed.stdout.strip().splitlines()[:20]:
            print(line)
        if completed.stdout.strip().count("\n") > 20:
            print(f"  ... ({completed.stdout.strip().count(chr(10)) - 20} more lines)")
    completed.check_returncode()
    return completed.stdout


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def minio_env() -> dict[str, str]:
    env = {
        "GIT_CACHE_S3_ACCESS_KEY": os.environ.get("GIT_CACHE_S3_ACCESS_KEY", "minioadmin"),
        "GIT_CACHE_S3_SECRET_KEY": os.environ.get("GIT_CACHE_S3_SECRET_KEY", "minioadmin"),
        "GIT_CACHE_S3_REGION": os.environ.get("GIT_CACHE_S3_REGION", "us-east-1"),
        "AWS_ACCESS_KEY_ID": os.environ.get("GIT_CACHE_S3_ACCESS_KEY", "minioadmin"),
        "AWS_SECRET_ACCESS_KEY": os.environ.get("GIT_CACHE_S3_SECRET_KEY", "minioadmin"),
    }
    if "AWS_REGION" in os.environ:
        env["AWS_REGION"] = os.environ["AWS_REGION"]
    return env


def object_store_config(prefix: str, local_root: Path) -> str:
    if not USE_MINIO_BACKEND:
        return f"""\
[object_store]
kind = "local"
root = "{local_root}"
"""

    endpoint = os.environ.get("GIT_CACHE_S3_ENDPOINT", "http://127.0.0.1:9000")
    bucket = os.environ.get("GIT_CACHE_S3_BUCKET", MINIO_BUCKET)
    return f"""\
[object_store]
kind = "s3"
bucket = "{bucket}"
prefix = "{prefix}"
endpoint = "{endpoint}"
"""


def list_minio_objects(prefix: str) -> list[str]:
    output = run(
        [
            "docker",
            "compose",
            "-f",
            "docker-compose.minio.yml",
            "run",
            "--rm",
            "--entrypoint",
            "/bin/sh",
            "createbuckets",
            "-c",
            "mc alias set local http://minio:9000 minioadmin minioadmin >/dev/null && "
            f"mc ls --recursive local/{MINIO_BUCKET}/{prefix}",
        ]
    )
    return [
        line.strip()
        for line in output.splitlines()
        if "STANDARD" in line and not line.strip().endswith("/")
    ]


@unittest.skipUnless(
    os.environ.get("RUN_GITHUB_INTEGRATION") == "1",
    "set RUN_GITHUB_INTEGRATION=1 to test against real public repositories",
)
class GitRemotePublicRepoTest(unittest.TestCase):
    """Test the read-through /git/ endpoint against real GitHub repos."""

    @classmethod
    def setUpClass(cls) -> None:
        base = Path(os.environ.get("TEST_TMPDIR", tempfile.gettempdir()))
        base.mkdir(parents=True, exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="git-cache-remote-public-", dir=base))
        cls.port = free_port()
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        cls.cache_root = cls.tmp / "cache"
        cls.object_root = cls.tmp / "object-store"
        cls.object_prefix = f"{MINIO_PREFIX}/git-remote-public/{cls.tmp.name}"
        config_path = cls.tmp / "config.toml"
        config_path.write_text(
            f"""\
bind_addr = "127.0.0.1:{cls.port}"
cache_root = "{cls.cache_root}"
git_timeout_seconds = 1200
max_git_output_bytes = 1073741824
rate_limit_per_minute = 0
allowed_upstream_hosts = ["github.com"]

{object_store_config(cls.object_prefix, cls.object_root)}
[disk]
quota_bytes = 10737418240
min_free_bytes = 0

[git_remote]
commit_read_through = true
"""
        )

        build = ["cargo", "build", "-p", "git-cache-api"]
        if USE_MINIO_BACKEND:
            build.extend(["--features", "s3"])
        run(build)

        git_tmp = cls.tmp / "git-tmp"
        git_tmp.mkdir(parents=True, exist_ok=True)

        env = os.environ.copy()
        env["RUST_LOG"] = "info"
        env["GIT_CACHE_CONFIG"] = str(config_path)
        env["TMPDIR"] = str(git_tmp)
        if USE_MINIO_BACKEND:
            env.update(minio_env())
        cls.server = subprocess.Popen(
            [str(REPO_ROOT / "target/debug/git-cache-api")],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        cls.wait_for_healthz()

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
    def wait_for_healthz(cls) -> None:
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

    def git_url(self, repo: str) -> str:
        return f"{self.base_url}/git/{repo}.git"

    def assert_minio_backend_used(self) -> None:
        if not USE_MINIO_BACKEND:
            return

        self.assertFalse(
            self.object_root.exists(),
            "local object store path should not be used in MinIO backend mode",
        )
        objects = list_minio_objects(self.object_prefix)
        self.assertTrue(objects, "expected MinIO bucket prefix to contain cached objects")
        self.assertTrue(
            any("/packs/pack-" in path for path in objects),
            f"expected MinIO cached objects to include a pack, got: {objects}",
        )

    # ── Per-repo tests ────────────────────────────────────────────────

    def test_ls_remote_torvalds_linux(self) -> None:
        """ls-remote via the cache should list refs for torvalds/linux."""
        self._assert_ls_remote("github.com/torvalds/linux", "master")

    def test_shallow_clone_torvalds_linux(self) -> None:
        """Shallow clone torvalds/linux via the cache server."""
        self._assert_shallow_clone("github.com/torvalds/linux", "master")

    def test_ls_remote_llvm_project(self) -> None:
        """ls-remote via the cache should list refs for llvm/llvm-project."""
        self._assert_ls_remote("github.com/llvm/llvm-project", "main")

    def test_shallow_clone_llvm_project(self) -> None:
        """Shallow clone llvm/llvm-project via the cache server."""
        self._assert_shallow_clone("github.com/llvm/llvm-project", "main")

    def test_ls_remote_gcc_mirror(self) -> None:
        """ls-remote via the cache should list refs for gcc-mirror/gcc."""
        self._assert_ls_remote("github.com/gcc-mirror/gcc", "master")

    def test_shallow_clone_gcc_mirror(self) -> None:
        """Shallow clone gcc-mirror/gcc via the cache server."""
        self._assert_shallow_clone("github.com/gcc-mirror/gcc", "master")

    def test_ls_remote_astral_uv(self) -> None:
        """ls-remote via the cache for the existing test repo astral-sh/uv."""
        self._assert_ls_remote("github.com/astral-sh/uv", "main")

    def test_shallow_clone_astral_uv(self) -> None:
        """Shallow clone astral-sh/uv via the cache server."""
        self._assert_shallow_clone("github.com/astral-sh/uv", "main")

    # ── Helpers ───────────────────────────────────────────────────────

    def _upstream_head(self, repo: str, branch: str) -> str:
        owner_repo = repo.removeprefix("github.com/")
        output = run(
            ["git", "ls-remote", "--heads", f"https://github.com/{owner_repo}.git", branch]
        )
        # ls-remote may return multiple lines when the pattern matches
        # more than one ref (e.g. "master" matches both refs/heads/master
        # and refs/heads/devel/rust/master).  Pick the exact match.
        target_ref = f"refs/heads/{branch}"
        for line in output.strip().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1] == target_ref:
                sha = parts[0]
                self.assertEqual(len(sha), 40, f"bad sha from upstream ls-remote: {sha!r}")
                return sha
        # Fallback: first line
        sha = output.strip().split()[0]
        self.assertEqual(len(sha), 40, f"bad sha from upstream ls-remote: {sha!r}")
        return sha

    def _assert_ls_remote(self, repo: str, branch: str) -> None:
        url = self.git_url(repo)
        upstream_sha = self._upstream_head(repo, branch)

        output = run(["git", "ls-remote", "--heads", url, branch])
        target_ref = f"refs/heads/{branch}"
        cache_sha = None
        for line in output.strip().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1] == target_ref:
                cache_sha = parts[0]
                break
        if cache_sha is None:
            cache_sha = output.strip().split()[0]
        self.assertEqual(len(cache_sha), 40, f"bad sha from cache ls-remote: {cache_sha!r}")

        self.assertEqual(
            cache_sha,
            upstream_sha,
            f"cache and upstream disagree for {repo} {branch}",
        )

    def _assert_shallow_clone(self, repo: str, branch: str) -> None:
        url = self.git_url(repo)
        upstream_sha = self._upstream_head(repo, branch)

        checkout = self.tmp / f"clone-{repo.replace('/', '-')}"
        if checkout.exists():
            shutil.rmtree(checkout)

        run(
            ["git", "clone", "--depth", "1", "--branch", branch, "--no-tags", url, str(checkout)]
        )
        head = run(["git", "rev-parse", "HEAD"], cwd=checkout).strip()
        self.assertEqual(
            head,
            upstream_sha,
            f"shallow clone HEAD doesn't match upstream for {repo}",
        )
        self.assert_minio_backend_used()


if __name__ == "__main__":
    unittest.main(verbosity=2)
