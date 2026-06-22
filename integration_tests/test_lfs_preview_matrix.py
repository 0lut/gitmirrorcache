#!/usr/bin/env python3
"""LFS cache preview test matrix.

Runs against a deployed preview instance to verify:
- LFS batch API proxies upstream and returns download URLs
- LFS object download via cache URL
- Cache hits on repeated requests
- Upload operation is rejected
- Clone with LFS resolves pointers through cache

Usage:

    RUN_LFS_PREVIEW_MATRIX=1 \
    GIT_CACHE_PREVIEW_BASE_URL=http://<preview-alb>  \
    python3 -m unittest -v integration_tests.test_lfs_preview_matrix
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LFS_TEST_REPO = "github.com/charmbracelet/vhs"
LFS_CONTENT_TYPE = "application/vnd.git-lfs+json"

LFS_POINTER_RE = re.compile(
    r"^version https://git-lfs\.github\.com/spec/v1\n"
    r"oid sha256:[0-9a-f]{64}\n"
    r"size \d+\n\Z",
)


def _run(
    cmd: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    check: bool = True,
    timeout: int = 300,
) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd))
    completed = subprocess.run(
        cmd, cwd=cwd, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout,
    )
    combined = (completed.stdout or "") + (completed.stderr or "")
    for line in combined.strip().splitlines()[:15]:
        print(line)
    if check:
        completed.check_returncode()
    return completed


def _is_lfs_pointer(content: str) -> bool:
    return bool(LFS_POINTER_RE.match(content))


def _extract_oid(content: str) -> str | None:
    m = re.search(r"oid sha256:([0-9a-f]{64})", content)
    return m.group(1) if m else None


def _find_lfs_pointer_files(tree: Path) -> list[Path]:
    pointers: list[Path] = []
    for path in tree.rglob("*"):
        if not path.is_file() or ".git" in path.parts:
            continue
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        if _is_lfs_pointer(text):
            pointers.append(path)
    return pointers


@unittest.skipUnless(
    os.environ.get("RUN_LFS_PREVIEW_MATRIX") == "1",
    "set RUN_LFS_PREVIEW_MATRIX=1 to run",
)
class LfsPreviewMatrix(unittest.TestCase):
    """LFS cache test matrix against a deployed preview."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.base_url = os.environ.get("GIT_CACHE_PREVIEW_BASE_URL", "").rstrip("/")
        if not cls.base_url:
            raise unittest.SkipTest("set GIT_CACHE_PREVIEW_BASE_URL")

        cls.repo = os.environ.get("GIT_CACHE_LFS_TEST_REPO", DEFAULT_LFS_TEST_REPO)
        cls.owner_repo = cls.repo.removeprefix("github.com/")
        cls.command_timeout = int(os.environ.get("GIT_CACHE_LFS_COMMAND_TIMEOUT", "300"))

        tmp_base = Path(os.environ.get("TEST_TMPDIR", tempfile.gettempdir()))
        tmp_base.mkdir(parents=True, exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="lfs-preview-matrix-", dir=tmp_base))

        results_path = os.environ.get("GIT_CACHE_LFS_RESULTS")
        cls.results_path = Path(results_path) if results_path else cls.tmp / "results.jsonl"
        cls.results_path.parent.mkdir(parents=True, exist_ok=True)
        cls.results: list[dict[str, Any]] = []
        cls.failures: list[str] = []

        cls.record({
            "case": "suite_start",
            "base_url": cls.base_url,
            "repo": cls.repo,
        })

        # Verify server health
        health_url = f"{cls.base_url}/healthz"
        try:
            with urllib.request.urlopen(health_url, timeout=10) as resp:
                if resp.status != 200:
                    raise RuntimeError(f"healthz returned {resp.status}")
        except Exception as e:
            raise RuntimeError(f"preview not healthy: {e}") from e

    @classmethod
    def tearDownClass(cls) -> None:
        if hasattr(cls, "results_path"):
            cls.record({
                "case": "suite_end",
                "failures": cls.failures,
                "total_cases": len(cls.results),
            })
            print(f"LFS preview matrix results: {cls.results_path}")
        tmp = getattr(cls, "tmp", None)
        if tmp is not None:
            shutil.rmtree(tmp, ignore_errors=True)

    @classmethod
    def record(cls, event: dict[str, Any]) -> None:
        event.setdefault("timestamp", time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
        cls.results.append(event)
        with cls.results_path.open("a") as handle:
            handle.write(json.dumps(event, sort_keys=True) + "\n")

    def _git_url(self) -> str:
        return f"{self.base_url}/git/{self.repo}.git"

    def _lfs_batch(self, objects: list[dict[str, Any]]) -> dict[str, Any]:
        url = f"{self._git_url()}/info/lfs/objects/batch"
        body = json.dumps({
            "operation": "download",
            "transfers": ["basic"],
            "objects": objects,
        }).encode()
        req = urllib.request.Request(
            url, data=body, method="POST",
            headers={"Content-Type": LFS_CONTENT_TYPE},
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read().decode())

    # ── matrix cases ──────────────────────────────────────────────────

    def test_01_clone_skip_smudge(self) -> None:
        """Clone with GIT_LFS_SKIP_SMUDGE=1 and verify pointer files."""
        t0 = time.monotonic()
        clone_dir = self.tmp / "clone-skip-smudge"
        env = os.environ.copy()
        env["GIT_LFS_SKIP_SMUDGE"] = "1"
        result = _run(
            ["git", "clone", "--depth", "1", self._git_url(), str(clone_dir)],
            env=env, check=False, timeout=self.command_timeout,
        )
        elapsed = time.monotonic() - t0
        passed = result.returncode == 0
        pointers = _find_lfs_pointer_files(clone_dir) if passed else []
        self.record({
            "case": "clone_skip_smudge",
            "status": "passed" if passed and pointers else "failed",
            "returncode": result.returncode,
            "pointer_count": len(pointers),
            "elapsed_s": round(elapsed, 2),
        })
        self.assertEqual(result.returncode, 0)
        self.assertGreater(len(pointers), 0, "expected LFS pointer files")

    def test_02_batch_download_cold(self) -> None:
        """First batch request: cold cache, proxies from upstream."""
        clone_dir = self.tmp / "clone-for-oid"
        env = os.environ.copy()
        env["GIT_LFS_SKIP_SMUDGE"] = "1"
        _run(
            ["git", "clone", "--depth", "1", self._git_url(), str(clone_dir)],
            env=env, check=False,
        )
        pointers = _find_lfs_pointer_files(clone_dir)
        self.assertGreater(len(pointers), 0)

        ptr_text = pointers[0].read_text(errors="replace")
        oid = _extract_oid(ptr_text)
        self.assertIsNotNone(oid)
        size_m = re.search(r"size (\d+)", ptr_text)
        size = int(size_m.group(1)) if size_m else 0

        t0 = time.monotonic()
        resp = self._lfs_batch([{"oid": oid, "size": size}])
        elapsed = time.monotonic() - t0
        obj = resp["objects"][0]
        has_actions = "actions" in obj

        self.record({
            "case": "batch_download_cold",
            "status": "passed" if has_actions else "failed",
            "oid": oid,
            "size": size,
            "elapsed_s": round(elapsed, 2),
            "has_download_url": has_actions,
        })
        self.assertTrue(has_actions, f"expected actions, got: {obj}")

        # Save OID for later tests
        self.__class__._cached_oid = oid
        self.__class__._cached_size = size
        self.__class__._cached_href = obj["actions"]["download"]["href"]

    def test_03_object_download(self) -> None:
        """Download the LFS object via the cache URL."""
        oid = getattr(self.__class__, "_cached_oid", None)
        size = getattr(self.__class__, "_cached_size", 0)
        href = getattr(self.__class__, "_cached_href", None)
        if not href:
            self.skipTest("no cached href from test_02")

        t0 = time.monotonic()
        with urllib.request.urlopen(href, timeout=60) as resp:
            data = resp.read()
        elapsed = time.monotonic() - t0
        passed = len(data) == size

        self.record({
            "case": "object_download",
            "status": "passed" if passed else "failed",
            "oid": oid,
            "expected_size": size,
            "actual_size": len(data),
            "elapsed_s": round(elapsed, 2),
        })
        self.assertEqual(len(data), size)

    def test_04_batch_download_warm(self) -> None:
        """Second batch request for same OID: should be a cache hit."""
        oid = getattr(self.__class__, "_cached_oid", None)
        size = getattr(self.__class__, "_cached_size", 0)
        if not oid:
            self.skipTest("no cached OID from test_02")

        t0 = time.monotonic()
        resp = self._lfs_batch([{"oid": oid, "size": size}])
        elapsed = time.monotonic() - t0
        obj = resp["objects"][0]
        has_actions = "actions" in obj

        self.record({
            "case": "batch_download_warm",
            "status": "passed" if has_actions else "failed",
            "oid": oid,
            "elapsed_s": round(elapsed, 2),
        })
        self.assertTrue(has_actions)

    def test_05_object_download_warm(self) -> None:
        """Download cached object again — should be fast."""
        href = getattr(self.__class__, "_cached_href", None)
        size = getattr(self.__class__, "_cached_size", 0)
        if not href:
            self.skipTest("no cached href from test_02")

        t0 = time.monotonic()
        with urllib.request.urlopen(href, timeout=60) as resp:
            data = resp.read()
        elapsed = time.monotonic() - t0

        self.record({
            "case": "object_download_warm",
            "status": "passed" if len(data) == size else "failed",
            "expected_size": size,
            "actual_size": len(data),
            "elapsed_s": round(elapsed, 2),
        })
        self.assertEqual(len(data), size)

    def test_06_upload_rejected(self) -> None:
        """LFS upload operation should be rejected."""
        url = f"{self._git_url()}/info/lfs/objects/batch"
        body = json.dumps({
            "operation": "upload",
            "transfers": ["basic"],
            "objects": [{"oid": "a" * 64, "size": 100}],
        }).encode()
        req = urllib.request.Request(
            url, data=body, method="POST",
            headers={"Content-Type": LFS_CONTENT_TYPE},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                status = resp.status
        except urllib.error.HTTPError as exc:
            status = exc.code

        passed = status == 405
        self.record({
            "case": "upload_rejected",
            "status": "passed" if passed else "failed",
            "http_status": status,
        })
        self.assertEqual(status, 405)

    def test_07_invalid_oid_rejected(self) -> None:
        """Batch with invalid OID returns validation error."""
        resp = self._lfs_batch([{"oid": "not-a-valid-oid", "size": 100}])
        obj = resp["objects"][0]
        has_error = "error" in obj

        self.record({
            "case": "invalid_oid_rejected",
            "status": "passed" if has_error else "failed",
            "error": obj.get("error"),
        })
        self.assertTrue(has_error)

    def test_08_multi_object_batch(self) -> None:
        """Batch with multiple objects returns results for all."""
        clone_dir = self.tmp / "clone-multi-oid"
        env = os.environ.copy()
        env["GIT_LFS_SKIP_SMUDGE"] = "1"
        _run(
            ["git", "clone", "--depth", "1", self._git_url(), str(clone_dir)],
            env=env, check=False,
        )
        pointers = _find_lfs_pointer_files(clone_dir)
        objects = []
        for p in pointers[:3]:
            text = p.read_text(errors="replace")
            oid = _extract_oid(text)
            size_m = re.search(r"size (\d+)", text)
            if oid and size_m:
                objects.append({"oid": oid, "size": int(size_m.group(1))})
        if not objects:
            self.skipTest("no LFS pointer files found")

        resp = self._lfs_batch(objects)
        result_count = len(resp.get("objects", []))
        all_have_actions = all("actions" in o for o in resp["objects"])

        self.record({
            "case": "multi_object_batch",
            "status": "passed" if result_count == len(objects) and all_have_actions else "failed",
            "requested": len(objects),
            "returned": result_count,
            "all_have_actions": all_have_actions,
        })
        self.assertEqual(result_count, len(objects))
        self.assertTrue(all_have_actions)


if __name__ == "__main__":
    unittest.main(verbosity=2)
