#!/usr/bin/env python3
"""Opt-in GitHub integration tests for the real astral-sh/uv repository.

These tests intentionally use only Python's standard library and shell out to
`cargo` and `git`. They are skipped unless RUN_GITHUB_INTEGRATION=1 is set.
"""

from __future__ import annotations

import json
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
DEFAULT_REPO = "github.com/astral-sh/uv"
DEFAULT_BRANCH = "main"


def run(cmd: list[str], *, cwd: Path = REPO_ROOT, env: dict[str, str] | None = None) -> str:
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
        print(completed.stdout.strip())
    completed.check_returncode()
    return completed.stdout


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@unittest.skipUnless(
    os.environ.get("RUN_GITHUB_INTEGRATION") == "1",
    "set RUN_GITHUB_INTEGRATION=1 to hit github.com/astral-sh/uv",
)
class AstralUvIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = os.environ.get("GIT_CACHE_TEST_REPO", DEFAULT_REPO)
        cls.branch = os.environ.get("GIT_CACHE_TEST_BRANCH", DEFAULT_BRANCH)
        cls.tmp = Path(tempfile.mkdtemp(prefix="git-cache-astral-uv-"))
        cls.port = free_port()
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        cls.cache_root = cls.tmp / "cache"
        cls.object_root = cls.tmp / "object-store"

        run(["cargo", "build", "-p", "git-cache-api"])

        env = os.environ.copy()
        env.update(
            {
                "GIT_CACHE_BIND_ADDR": f"127.0.0.1:{cls.port}",
                "GIT_CACHE_PUBLIC_BASE_URL": cls.base_url,
                "GIT_CACHE_ROOT": str(cls.cache_root),
                "GIT_CACHE_OBJECT_STORE_ROOT": str(cls.object_root),
                "GIT_CACHE_GIT_TIMEOUT_SECONDS": "600",
                "GIT_CACHE_MAX_GIT_OUTPUT_BYTES": str(512 * 1024 * 1024),
                "RUST_LOG": "info",
            }
        )
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
                    print("server output tail:")
                    print(tail.strip())

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

    def materialize(self, selector: dict[str, object], mode: str = "strict") -> dict[str, object]:
        body = json.dumps(
            {
                "repo": self.repo,
                "selector": selector,
                "mode": mode,
            }
        ).encode()
        request = urllib.request.Request(
            f"{self.base_url}/v1/materialize",
            data=body,
            headers={"content-type": "application/json"},
            method="POST",
        )

        with urllib.request.urlopen(request, timeout=600) as response:
            self.assertEqual(response.status, 200)
            payload = json.loads(response.read().decode())

        print(json.dumps(payload, indent=2, sort_keys=True))
        return payload

    def expected_branch_commit(self) -> str:
        owner_repo = self.repo.removeprefix("github.com/")
        output = run(
            [
                "git",
                "ls-remote",
                "--heads",
                f"https://github.com/{owner_repo}.git",
                self.branch,
            ]
        )
        return output.split()[0]

    def fetch_session_ref(self, materialized: dict[str, object], label: str) -> str:
        checkout = self.tmp / f"fetch-{label}"
        checkout.mkdir()
        run(["git", "init"], cwd=checkout)
        run(
            [
                "git",
                "fetch",
                str(materialized["git_url"]),
                str(materialized["ref"]),
            ],
            cwd=checkout,
        )
        fetched = run(["git", "rev-parse", "FETCH_HEAD"], cwd=checkout).strip()
        self.assertEqual(fetched, materialized["commit"])
        return fetched

    def test_strict_main_materializes_astral_uv_and_fetches_session_ref(self) -> None:
        materialized = self.materialize({"branch": self.branch})

        self.assertEqual(materialized["repo"], self.repo)
        self.assertEqual(materialized["source"], "github_verified")
        self.assertEqual(materialized["commit"], self.expected_branch_commit())

        fetched = self.fetch_session_ref(materialized, "strict-main")
        self.assertEqual(fetched, materialized["commit"])

    def test_exact_commit_rehydrates_after_hot_cache_deletion(self) -> None:
        first = self.materialize({"branch": self.branch})

        shutil.rmtree(self.cache_root / "repos", ignore_errors=True)
        shutil.rmtree(self.cache_root / "sessions", ignore_errors=True)

        cached = self.materialize({"commit": first["commit"]})
        self.assertEqual(cached["source"], "cache_verified")
        self.assertEqual(cached["commit"], first["commit"])

        fetched = self.fetch_session_ref(cached, "cached-commit")
        self.assertEqual(fetched, first["commit"])

    def test_short_commit_resolves_to_full_commit(self) -> None:
        branch = self.materialize({"branch": self.branch})
        short = str(branch["commit"])[:8]

        resolved = self.materialize({"short_commit": short})
        self.assertEqual(resolved["source"], "cache_verified")
        self.assertEqual(resolved["commit"], branch["commit"])

        fetched = self.fetch_session_ref(resolved, "short-commit")
        self.assertEqual(fetched, branch["commit"])

    def test_default_branch_materializes(self) -> None:
        materialized = self.materialize({"default_branch": True})

        self.assertEqual(materialized["repo"], self.repo)
        self.assertEqual(materialized["source"], "github_verified")
        self.assertEqual(materialized["commit"], self.expected_branch_commit())


if __name__ == "__main__":
    unittest.main(verbosity=2)
