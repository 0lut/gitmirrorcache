#!/usr/bin/env python3
"""Opt-in AWS dev direct-Git correctness and speed matrix.

This runner targets an already-deployed gitmirrorcache dev instance. It measures
direct GitHub baselines, cache proxy/read-through cold paths, hot-cache repeats,
and request-scoped Basic auth behavior for public repositories.

The suite is intentionally opt-in:

    RUN_AWS_DEV_GIT_MATRIX=1 \
    GIT_CACHE_AWS_DEV_BASE_URL=http://example-alb \
    python3 integration_tests/test_aws_dev_git_matrix.py
"""

from __future__ import annotations

import base64
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
from dataclasses import dataclass
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPOS = [
    ("github.com/astral-sh/uv", "main"),
    ("github.com/astral-sh/ruff", "main"),
    ("github.com/torvalds/linux", "master"),
    ("github.com/llvm/llvm-project", "main"),
]
SMALL_REPOS = {"github.com/astral-sh/uv", "github.com/astral-sh/ruff"}
LFS_REPOS = {"github.com/charmbracelet/vhs", "github.com/SixLabors/ImageSharp"}
LFS_CONTENT_TYPE = "application/vnd.git-lfs+json"
LFS_POINTER_RE = re.compile(
    r"^version https://git-lfs\.github\.com/spec/v1\n"
    r"oid sha256:[0-9a-f]{64}\n"
    r"size \d+\n\Z",
)


@dataclass(frozen=True)
class RepoCase:
    repo: str
    branch: str


@dataclass
class CommandResult:
    returncode: int
    duration_s: float
    stdout: str


def env_bool(name: str, default: bool = False) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def parse_repos() -> list[RepoCase]:
    value = os.environ.get("GIT_CACHE_AWS_DEV_REPOS")
    if not value:
        return [RepoCase(repo, branch) for repo, branch in DEFAULT_REPOS]

    repos: list[RepoCase] = []
    for item in value.split(","):
        item = item.strip()
        if not item:
            continue
        if ":" in item:
            repo, branch = item.split(":", 1)
        else:
            repo, branch = item, "main"
        repos.append(RepoCase(repo.strip(), branch.strip()))
    return repos


def run_command(
    cmd: list[str],
    *,
    cwd: Path = REPO_ROOT,
    env: dict[str, str] | None = None,
    timeout: int = 900,
) -> CommandResult:
    started = time.monotonic()
    completed = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    return CommandResult(
        returncode=completed.returncode,
        duration_s=time.monotonic() - started,
        stdout=completed.stdout,
    )


def git_env(extra_headers: list[str] | None = None) -> dict[str, str]:
    env = os.environ.copy()
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_CONFIG_COUNT"] = str(len(extra_headers or []))
    for idx, header in enumerate(extra_headers or []):
        env[f"GIT_CONFIG_KEY_{idx}"] = "http.extraHeader"
        env[f"GIT_CONFIG_VALUE_{idx}"] = header
    return env


def redact_output(output: str) -> str:
    lines = output.strip().splitlines()
    if len(lines) > 30:
        lines = lines[:15] + ["..."] + lines[-15:]
    return "\n".join(lines)


def basic_auth_header_from_env() -> str | None:
    raw = os.environ.get("GIT_CACHE_AWS_DEV_BASIC_AUTH")
    if raw:
        return raw if raw.lower().startswith("basic ") else f"Basic {raw}"

    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        payload = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        return f"Basic {payload}"

    if env_bool("GIT_CACHE_AWS_DEV_USE_GH_TOKEN", True):
        gh = shutil.which("gh")
        if gh:
            try:
                result = run_command([gh, "auth", "token"], timeout=15)
            except (OSError, subprocess.TimeoutExpired):
                result = CommandResult(1, 0.0, "")
            token = result.stdout.strip() if result.returncode == 0 else ""
            if token:
                payload = base64.b64encode(f"x-access-token:{token}".encode()).decode()
                return f"Basic {payload}"

    return None


def urlopen_json(url: str, *, timeout: int = 30) -> Any:
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return json.loads(response.read().decode())


def urlopen_text(url: str, *, timeout: int = 30) -> str:
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return response.read().decode()


class AwsDevGitMatrix(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if os.environ.get("RUN_AWS_DEV_GIT_MATRIX") != "1":
            raise unittest.SkipTest("set RUN_AWS_DEV_GIT_MATRIX=1 to test AWS dev")

        cls.base_url = os.environ.get("GIT_CACHE_AWS_DEV_BASE_URL", "").rstrip("/")
        if not cls.base_url:
            raise unittest.SkipTest("set GIT_CACHE_AWS_DEV_BASE_URL to the deployed dev URL")

        cls.tier = os.environ.get("GIT_CACHE_AWS_DEV_TIER", "standard").strip().lower()
        cls.command_timeout = int(os.environ.get("GIT_CACHE_AWS_DEV_COMMAND_TIMEOUT", "1200"))
        cls.proxy_warm_wait_s = float(os.environ.get("GIT_CACHE_AWS_DEV_PROXY_WARM_WAIT_SECONDS", "5"))
        cls.skip_standard = env_bool("GIT_CACHE_AWS_DEV_SKIP_STANDARD", False)
        cls.direct_heavy_baseline = env_bool("GIT_CACHE_AWS_DEV_DIRECT_HEAVY_BASELINE", False)
        cls.reset_local_cache = env_bool("GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE", False)
        cls.require_auth = env_bool("GIT_CACHE_AWS_DEV_REQUIRE_AUTH", False)
        cls.skip_lfs = env_bool("GIT_CACHE_AWS_DEV_SKIP_LFS", False)
        cls.repos = parse_repos()
        # Append LFS repos unless explicitly skipped or already present.
        if not cls.skip_lfs:
            existing = {rc.repo for rc in cls.repos}
            for lfs_repo in sorted(LFS_REPOS):
                if lfs_repo not in existing:
                    cls.repos.append(RepoCase(lfs_repo, "main"))
        cls.basic_auth_header = basic_auth_header_from_env()
        if cls.require_auth and not cls.basic_auth_header:
            raise RuntimeError("auth is required but no Basic auth token/header is configured")

        tmp_base = Path(os.environ.get("TEST_TMPDIR", tempfile.gettempdir()))
        tmp_base.mkdir(parents=True, exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="git-cache-aws-dev-matrix-", dir=tmp_base))
        results_path = os.environ.get("GIT_CACHE_AWS_DEV_RESULTS")
        if results_path:
            cls.results_path = Path(results_path)
            cls.results_path.parent.mkdir(parents=True, exist_ok=True)
        else:
            cls.results_path = cls.tmp / "results.jsonl"
        cls.results: list[dict[str, Any]] = []
        cls.failures: list[str] = []
        cls.warnings: list[str] = []
        cls.upstream_heads: dict[tuple[str, str], str] = {}
        cls.baseline_seconds: dict[str, float] = {}

        cls.record(
            {
                "case": "suite_start",
                "base_url": cls.base_url,
                "tier": cls.tier,
                "skip_standard": cls.skip_standard,
                "direct_heavy_baseline": cls.direct_heavy_baseline,
                "reset_local_cache": cls.reset_local_cache,
                "repos": [case.__dict__ for case in cls.repos],
                "auth_modes": ["none"] + (["basic"] if cls.basic_auth_header else []),
                "results_path": str(cls.results_path),
            }
        )

    @classmethod
    def tearDownClass(cls) -> None:
        if hasattr(cls, "results_path"):
            cls.record({"case": "suite_end", "failures": cls.failures, "warnings": cls.warnings})
            print(f"aws dev matrix results: {cls.results_path}")
        tmp = getattr(cls, "tmp", None)
        if tmp is not None and not env_bool("GIT_CACHE_AWS_DEV_KEEP_TMP", False):
            shutil.rmtree(tmp, ignore_errors=True)

    @classmethod
    def record(cls, event: dict[str, Any]) -> None:
        event.setdefault("timestamp", time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
        cls.results.append(event)
        with cls.results_path.open("a") as handle:
            handle.write(json.dumps(event, sort_keys=True) + "\n")

    def test_matrix(self) -> None:
        health = urlopen_json(f"{self.base_url}/healthz")
        self.record({"case": "healthz", "status": "passed", "payload": health})
        self.assertTrue(health.get("ok"), health)

        metrics_before = urlopen_text(f"{self.base_url}/metrics")
        self.record({"case": "metrics_before", "metrics": metrics_before.strip()})

        self.assert_receive_pack_rejected()

        for repo_case in self.repos:
            with self.subTest(repo=repo_case.repo, branch=repo_case.branch):
                self.run_repo_matrix(repo_case)

        metrics_after = urlopen_text(f"{self.base_url}/metrics")
        self.record({"case": "metrics_after", "metrics": metrics_after.strip()})

        if self.failures:
            self.fail("\n".join(self.failures))

    def run_repo_matrix(self, repo_case: RepoCase) -> None:
        upstream_head = self.upstream_head(repo_case)
        self.cache_ls_remote(repo_case, upstream_head, auth_mode="none")
        if self.basic_auth_header:
            self.cache_ls_remote(repo_case, upstream_head, auth_mode="basic")

        if not self.skip_standard:
            self.github_baseline(repo_case, upstream_head)

            self.reset_repo_if_enabled(repo_case.repo, "cold_proxy_noauth")
            cold_proxy = self.clone_depth1_blobless(
                repo_case,
                upstream_head,
                label="cold_proxy_noauth",
                auth_mode="none",
                proxy_opt_out=False,
            )
            self.wait_after_proxy_warm(repo_case.repo, "cold_proxy_noauth")
            hot_proxy = self.clone_depth1_blobless(
                repo_case,
                upstream_head,
                label="hot_proxy_noauth",
                auth_mode="none",
                proxy_opt_out=False,
            )
            self.compare_hot_to_cold(repo_case.repo, cold_proxy, hot_proxy, "noauth")

            self.reset_repo_if_enabled(repo_case.repo, "cold_readthrough_noauth")
            self.clone_depth1_blobless(
                repo_case,
                upstream_head,
                label="cold_readthrough_noauth",
                auth_mode="none",
                proxy_opt_out=True,
            )
            self.clone_depth1_blobless(
                repo_case,
                upstream_head,
                label="hot_readthrough_noauth",
                auth_mode="none",
                proxy_opt_out=True,
            )

            if self.basic_auth_header:
                self.reset_repo_if_enabled(repo_case.repo, "cold_proxy_basic")
                self.clone_depth1_blobless(
                    repo_case,
                    upstream_head,
                    label="cold_proxy_basic",
                    auth_mode="basic",
                    proxy_opt_out=False,
                )
                self.wait_after_proxy_warm(repo_case.repo, "cold_proxy_basic")
                self.clone_depth1_blobless(
                    repo_case,
                    upstream_head,
                    label="hot_proxy_basic",
                    auth_mode="basic",
                    proxy_opt_out=False,
                )

        if repo_case.repo in SMALL_REPOS and not self.skip_standard:
            self.blobless_then_full_depth1_transition(repo_case, upstream_head)
        if repo_case.repo in LFS_REPOS and not self.skip_lfs:
            self.run_lfs_matrix(repo_case)
        if self.tier == "heavy":
            if self.direct_heavy_baseline:
                self.heavy_github_direct_baseline(repo_case, upstream_head)
            self.heavy_proxy_off_readthrough(repo_case, upstream_head)

    def git_url(self, repo: str) -> str:
        return f"{self.base_url}/git/{repo}.git"

    def upstream_url(self, repo: str) -> str:
        return f"https://github.com/{repo.removeprefix('github.com/')}.git"

    def auth_headers(self, auth_mode: str, *, proxy_opt_out: bool = False) -> list[str]:
        headers: list[str] = []
        if auth_mode == "basic":
            if not self.basic_auth_header:
                raise RuntimeError("basic auth requested but no header is configured")
            headers.append(f"Authorization: {self.basic_auth_header}")
        elif auth_mode != "none":
            raise ValueError(f"unknown auth mode: {auth_mode}")
        if proxy_opt_out:
            headers.append("git-cache-use-proxy-on-miss: false")
        return headers

    def run_git_case(
        self,
        *,
        name: str,
        repo: str,
        branch: str,
        cmd: list[str],
        auth_mode: str = "none",
        proxy_opt_out: bool = False,
        cwd: Path | None = None,
    ) -> CommandResult:
        result = run_command(
            cmd,
            cwd=cwd or REPO_ROOT,
            env=git_env(self.auth_headers(auth_mode, proxy_opt_out=proxy_opt_out)),
            timeout=self.command_timeout,
        )
        event = {
            "case": name,
            "repo": repo,
            "branch": branch,
            "auth_mode": auth_mode,
            "proxy_opt_out": proxy_opt_out,
            "duration_s": round(result.duration_s, 3),
            "returncode": result.returncode,
            "status": "passed" if result.returncode == 0 else "failed",
            "stdout_tail": redact_output(result.stdout),
        }
        self.record(event)
        if result.returncode != 0:
            self.failures.append(
                f"{name} failed for {repo} ({auth_mode}, proxy_opt_out={proxy_opt_out})"
            )
        return result

    def upstream_head(self, repo_case: RepoCase) -> str:
        key = (repo_case.repo, repo_case.branch)
        if key in self.upstream_heads:
            return self.upstream_heads[key]

        result = run_command(
            [
                "git",
                "ls-remote",
                "--heads",
                self.upstream_url(repo_case.repo),
                repo_case.branch,
            ],
            env=git_env(),
            timeout=self.command_timeout,
        )
        self.record(
            {
                "case": "upstream_ls_remote",
                "repo": repo_case.repo,
                "branch": repo_case.branch,
                "duration_s": round(result.duration_s, 3),
                "returncode": result.returncode,
                "status": "passed" if result.returncode == 0 else "failed",
                "stdout_tail": redact_output(result.stdout),
            }
        )
        if result.returncode != 0:
            self.failures.append(f"upstream ls-remote failed for {repo_case.repo}")
            return ""

        target_ref = f"refs/heads/{repo_case.branch}"
        for line in result.stdout.splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1] == target_ref:
                self.upstream_heads[key] = parts[0]
                return parts[0]
        self.failures.append(f"upstream ls-remote did not return {target_ref} for {repo_case.repo}")
        return ""

    def cache_ls_remote(self, repo_case: RepoCase, upstream_head: str, *, auth_mode: str) -> None:
        result = self.run_git_case(
            name="cache_ls_remote",
            repo=repo_case.repo,
            branch=repo_case.branch,
            auth_mode=auth_mode,
            cmd=[
                "git",
                "ls-remote",
                "--heads",
                self.git_url(repo_case.repo),
                repo_case.branch,
            ],
        )
        if result.returncode != 0:
            return
        target_ref = f"refs/heads/{repo_case.branch}"
        cache_head = ""
        for line in result.stdout.splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1] == target_ref:
                cache_head = parts[0]
                break
        if cache_head != upstream_head:
            self.failures.append(
                f"cache ls-remote mismatch for {repo_case.repo}: {cache_head} != {upstream_head}"
            )

    def github_baseline(self, repo_case: RepoCase, upstream_head: str) -> None:
        checkout = self.tmp / safe_name(f"github-{repo_case.repo}")
        shutil.rmtree(checkout, ignore_errors=True)
        result = self.run_git_case(
            name="github_direct_depth1_blobless",
            repo=repo_case.repo,
            branch=repo_case.branch,
            cmd=depth1_blobless_clone_cmd(self.upstream_url(repo_case.repo), repo_case.branch, checkout),
        )
        self.baseline_seconds[repo_case.repo] = result.duration_s
        if result.returncode == 0:
            self.verify_clone(checkout, repo_case, upstream_head, "github_direct_depth1_blobless")

    def clone_depth1_blobless(
        self,
        repo_case: RepoCase,
        upstream_head: str,
        *,
        label: str,
        auth_mode: str,
        proxy_opt_out: bool,
    ) -> float:
        checkout = self.tmp / safe_name(f"{label}-{auth_mode}-{repo_case.repo}")
        shutil.rmtree(checkout, ignore_errors=True)
        result = self.run_git_case(
            name="cache_depth1_blobless",
            repo=repo_case.repo,
            branch=repo_case.branch,
            auth_mode=auth_mode,
            proxy_opt_out=proxy_opt_out,
            cmd=depth1_blobless_clone_cmd(self.git_url(repo_case.repo), repo_case.branch, checkout),
        )
        if result.returncode == 0:
            self.verify_clone(checkout, repo_case, upstream_head, label)
        return result.duration_s

    def blobless_then_full_depth1_transition(self, repo_case: RepoCase, upstream_head: str) -> None:
        self.reset_repo_if_enabled(repo_case.repo, "transition_blobless_then_full")
        blobless_dir = self.tmp / safe_name(f"transition-blobless-{repo_case.repo}")
        shutil.rmtree(blobless_dir, ignore_errors=True)
        result = self.run_git_case(
            name="transition_blobless_depth1",
            repo=repo_case.repo,
            branch=repo_case.branch,
            cmd=depth1_blobless_clone_cmd(self.git_url(repo_case.repo), repo_case.branch, blobless_dir),
        )
        if result.returncode == 0:
            self.verify_clone(blobless_dir, repo_case, upstream_head, "transition_blobless_depth1")

        full_dir = self.tmp / safe_name(f"transition-full-depth1-{repo_case.repo}")
        shutil.rmtree(full_dir, ignore_errors=True)
        full_result = self.run_git_case(
            name="transition_full_depth1_after_blobless",
            repo=repo_case.repo,
            branch=repo_case.branch,
            cmd=[
                "git",
                "clone",
                "--depth",
                "1",
                "--branch",
                repo_case.branch,
                "--single-branch",
                "--no-tags",
                self.git_url(repo_case.repo),
                str(full_dir),
            ],
        )
        if full_result.returncode == 0:
            self.verify_clone(full_dir, repo_case, upstream_head, "transition_full_depth1_after_blobless")
            status = run_command(["git", "status", "--porcelain"], cwd=full_dir, env=git_env())
            if status.stdout.strip():
                self.failures.append(f"full depth-1 checkout is dirty for {repo_case.repo}")
            missing = run_command(
                ["git", "rev-list", "--objects", "--missing=print", "--max-count=1", "HEAD"],
                cwd=full_dir,
                env=git_env(),
            )
            if "\n?" in missing.stdout or missing.stdout.startswith("?"):
                self.failures.append(f"full depth-1 clone has missing objects for {repo_case.repo}")

    def heavy_proxy_off_readthrough(self, repo_case: RepoCase, upstream_head: str) -> None:
        self.reset_repo_if_enabled(repo_case.repo, "heavy_full_history_readthrough")
        checkout = self.tmp / safe_name(f"full-history-{repo_case.repo}")
        shutil.rmtree(checkout, ignore_errors=True)
        result = self.run_git_case(
            name="heavy_full_history_blobless_readthrough_no_checkout",
            repo=repo_case.repo,
            branch=repo_case.branch,
            proxy_opt_out=True,
            cmd=[
                "git",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                "--branch",
                repo_case.branch,
                "--single-branch",
                "--no-tags",
                self.git_url(repo_case.repo),
                str(checkout),
            ],
        )
        if result.returncode == 0:
            self.verify_full_history_head_walk(
                checkout,
                repo_case,
                upstream_head,
                "heavy_full_history_blobless_readthrough_no_checkout",
                walk_case="heavy_full_history_walk_1000",
            )

        if repo_case.repo in SMALL_REPOS:
            self.reset_repo_if_enabled(repo_case.repo, "heavy_blobless_checkout_readthrough")
            checkout = self.tmp / safe_name(f"full-checkout-{repo_case.repo}")
            shutil.rmtree(checkout, ignore_errors=True)
            result = self.run_git_case(
                name="heavy_blobless_readthrough_checkout",
                repo=repo_case.repo,
                branch=repo_case.branch,
                proxy_opt_out=True,
                cmd=[
                    "git",
                    "clone",
                    "--filter=blob:none",
                    "--branch",
                    repo_case.branch,
                    "--single-branch",
                    "--no-tags",
                    self.git_url(repo_case.repo),
                    str(checkout),
                ],
            )
            if result.returncode == 0:
                self.verify_full_checkout(
                    checkout,
                    repo_case,
                    upstream_head,
                    source_case="heavy_blobless_readthrough_checkout",
                )

    def heavy_github_direct_baseline(self, repo_case: RepoCase, upstream_head: str) -> None:
        checkout = self.tmp / safe_name(f"github-full-history-{repo_case.repo}")
        shutil.rmtree(checkout, ignore_errors=True)
        result = self.run_git_case(
            name="github_direct_full_history_blobless_no_checkout",
            repo=repo_case.repo,
            branch=repo_case.branch,
            cmd=[
                "git",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                "--branch",
                repo_case.branch,
                "--single-branch",
                "--no-tags",
                self.upstream_url(repo_case.repo),
                str(checkout),
            ],
        )
        if result.returncode == 0:
            self.verify_full_history_head_walk(
                checkout,
                repo_case,
                upstream_head,
                "github_direct_full_history_blobless_no_checkout",
                walk_case="github_direct_full_history_walk_1000",
            )

        if repo_case.repo in SMALL_REPOS:
            checkout = self.tmp / safe_name(f"github-full-checkout-{repo_case.repo}")
            shutil.rmtree(checkout, ignore_errors=True)
            result = self.run_git_case(
                name="github_direct_blobless_checkout",
                repo=repo_case.repo,
                branch=repo_case.branch,
                cmd=[
                    "git",
                    "clone",
                    "--filter=blob:none",
                    "--branch",
                    repo_case.branch,
                    "--single-branch",
                    "--no-tags",
                    self.upstream_url(repo_case.repo),
                    str(checkout),
                ],
            )
            if result.returncode == 0:
                self.verify_full_checkout(
                    checkout,
                    repo_case,
                    upstream_head,
                    source_case="github_direct_blobless_checkout",
                )

    def verify_full_history_head_walk(
        self,
        checkout: Path,
        repo_case: RepoCase,
        upstream_head: str,
        source_case: str,
        *,
        walk_case: str,
    ) -> None:
        head = run_command(["git", "rev-parse", "HEAD"], cwd=checkout, env=git_env())
        self.record(
            {
                "case": "verify_full_history_head",
                "source_case": source_case,
                "repo": repo_case.repo,
                "branch": repo_case.branch,
                "head": head.stdout.strip(),
                "expected_head": upstream_head,
                "status": "passed" if head.stdout.strip() == upstream_head else "failed",
            }
        )
        if head.stdout.strip() != upstream_head:
            self.failures.append(
                f"HEAD mismatch for {source_case} {repo_case.repo}: "
                f"{head.stdout.strip()} != {upstream_head}"
            )
        walk = run_command(["git", "log", "--oneline", "--max-count=1000"], cwd=checkout, env=git_env())
        self.record(
            {
                "case": walk_case,
                "source_case": source_case,
                "repo": repo_case.repo,
                "branch": repo_case.branch,
                "duration_s": round(walk.duration_s, 3),
                "returncode": walk.returncode,
                "status": "passed" if walk.returncode == 0 else "failed",
                "stdout_tail": redact_output(walk.stdout),
            }
        )
        if walk.returncode != 0:
            self.failures.append(f"full-history walk failed for {source_case} {repo_case.repo}")

    def verify_full_checkout(
        self,
        checkout: Path,
        repo_case: RepoCase,
        upstream_head: str,
        *,
        source_case: str,
    ) -> None:
        head = run_command(["git", "rev-parse", "HEAD"], cwd=checkout, env=git_env())
        status = run_command(["git", "status", "--porcelain"], cwd=checkout, env=git_env())
        walk = run_command(["git", "log", "--oneline", "--max-count=1000"], cwd=checkout, env=git_env())
        files = run_command(["git", "ls-files"], cwd=checkout, env=git_env())
        event = {
            "case": "verify_full_checkout",
            "source_case": source_case,
            "repo": repo_case.repo,
            "branch": repo_case.branch,
            "head": head.stdout.strip(),
            "expected_head": upstream_head,
            "dirty": bool(status.stdout.strip()),
            "walk_returncode": walk.returncode,
            "tracked_files": len([line for line in files.stdout.splitlines() if line]),
            "status": "passed",
        }
        if head.stdout.strip() != upstream_head:
            event["status"] = "failed"
            self.failures.append(
                f"HEAD mismatch for heavy_blobless_readthrough_checkout {repo_case.repo}: "
                f"{head.stdout.strip()} != {upstream_head}"
            )
        elif status.stdout.strip():
            event["status"] = "failed"
            self.failures.append(f"checkout is dirty for {repo_case.repo}")
        elif walk.returncode != 0:
            event["status"] = "failed"
            self.failures.append(f"checkout history walk failed for {repo_case.repo}")
        elif event["tracked_files"] <= 0:
            event["status"] = "failed"
            self.failures.append(f"checkout has no tracked files for {repo_case.repo}")
        self.record(event)

    def verify_clone(
        self,
        checkout: Path,
        repo_case: RepoCase,
        upstream_head: str,
        source_case: str,
    ) -> None:
        head = run_command(["git", "rev-parse", "HEAD"], cwd=checkout, env=git_env())
        shallow = run_command(
            ["git", "rev-parse", "--is-shallow-repository"],
            cwd=checkout,
            env=git_env(),
        )
        count = run_command(["git", "rev-list", "--count", "HEAD"], cwd=checkout, env=git_env())
        event = {
            "case": "verify_clone",
            "source_case": source_case,
            "repo": repo_case.repo,
            "branch": repo_case.branch,
            "head": head.stdout.strip(),
            "expected_head": upstream_head,
            "is_shallow": shallow.stdout.strip(),
            "commit_count": count.stdout.strip(),
            "status": "passed",
        }
        if head.returncode != 0 or shallow.returncode != 0 or count.returncode != 0:
            event["status"] = "failed"
            self.failures.append(f"verification command failed for {source_case} {repo_case.repo}")
        elif head.stdout.strip() != upstream_head:
            event["status"] = "failed"
            self.failures.append(
                f"HEAD mismatch for {source_case} {repo_case.repo}: {head.stdout.strip()} != {upstream_head}"
            )
        elif count.stdout.strip() != "1":
            event["status"] = "failed"
            self.failures.append(f"depth-1 clone has {count.stdout.strip()} commits for {repo_case.repo}")
        self.record(event)

    def compare_hot_to_cold(self, repo: str, cold_s: float, hot_s: float, auth_mode: str) -> None:
        ratio = hot_s / cold_s if cold_s > 0 else None
        warning = None
        if ratio is not None and hot_s > max(cold_s * 1.25, cold_s + 5.0):
            warning = f"hot {auth_mode} clone for {repo} was slower than cold ({hot_s:.3f}s vs {cold_s:.3f}s)"
            self.warnings.append(warning)
        self.record(
            {
                "case": "compare_hot_to_cold",
                "repo": repo,
                "auth_mode": auth_mode,
                "cold_s": round(cold_s, 3),
                "hot_s": round(hot_s, 3),
                "ratio": round(ratio, 3) if ratio is not None else None,
                "warning": warning,
            }
        )

    def reset_repo_if_enabled(self, repo: str, label: str) -> None:
        if not self.reset_local_cache:
            self.record({"case": "reset_local_cache", "repo": repo, "label": label, "status": "skipped"})
            return
        env = os.environ.copy()
        env["AWS_PAGER"] = ""
        result = run_command(
            [str(REPO_ROOT / "scripts/aws/remove-cache-repo.sh"), repo],
            env=env,
            timeout=240,
        )
        self.record(
            {
                "case": "reset_local_cache",
                "repo": repo,
                "label": label,
                "duration_s": round(result.duration_s, 3),
                "returncode": result.returncode,
                "status": "passed" if result.returncode == 0 else "failed",
                "stdout_tail": redact_output(result.stdout),
            }
        )
        if result.returncode != 0:
            self.failures.append(f"local cache reset failed for {repo} before {label}")

    def wait_after_proxy_warm(self, repo: str, label: str) -> None:
        if self.proxy_warm_wait_s <= 0:
            self.record({"case": "proxy_warm_wait", "repo": repo, "label": label, "status": "skipped"})
            return
        started = time.monotonic()
        time.sleep(self.proxy_warm_wait_s)
        self.record(
            {
                "case": "proxy_warm_wait",
                "repo": repo,
                "label": label,
                "duration_s": round(time.monotonic() - started, 3),
                "status": "passed",
            }
        )

    # ── LFS matrix ─────────────────────────────────────────────────────

    def run_lfs_matrix(self, repo_case: RepoCase) -> None:
        """LFS cache correctness and performance sub-matrix."""
        # 1. Clone with skip-smudge to discover pointer files
        clone_dir = self.tmp / safe_name(f"lfs-clone-{repo_case.repo}")
        shutil.rmtree(clone_dir, ignore_errors=True)
        env = git_env()
        env["GIT_LFS_SKIP_SMUDGE"] = "1"
        t0 = time.monotonic()
        clone_result = run_command(
            ["git", "clone", "--depth", "1", self.git_url(repo_case.repo), str(clone_dir)],
            env=env, timeout=self.command_timeout,
        )
        clone_elapsed = time.monotonic() - t0
        self.record({
            "case": "lfs_clone_skip_smudge",
            "repo": repo_case.repo,
            "branch": repo_case.branch,
            "duration_s": round(clone_elapsed, 3),
            "returncode": clone_result.returncode,
            "status": "passed" if clone_result.returncode == 0 else "failed",
        })
        if clone_result.returncode != 0:
            self.failures.append(f"LFS skip-smudge clone failed for {repo_case.repo}")
            return

        # Find pointer files
        pointers = self._find_lfs_pointer_files(clone_dir)
        self.record({
            "case": "lfs_pointer_discovery",
            "repo": repo_case.repo,
            "pointer_count": len(pointers),
            "status": "passed" if pointers else "failed",
        })
        if not pointers:
            self.failures.append(f"no LFS pointer files found in {repo_case.repo}")
            return

        # Pick the largest pointer for the single-object test to exercise streaming.
        best_ptr, oid, size = None, None, 0
        for p in pointers:
            text = p.read_text(errors="replace")
            o = self._extract_oid(text)
            sm = re.search(r"size (\d+)", text)
            s = int(sm.group(1)) if sm else 0
            if o and s > size:
                best_ptr, oid, size = p, o, s
        if not oid:
            self.failures.append(f"failed to extract OID from pointer in {repo_case.repo}")
            return

        # 2. LFS batch API — cold (first request, cache miss)
        cold_batch = self._lfs_batch_timed(repo_case, [{"oid": oid, "size": size}], "lfs_batch_cold")
        if not cold_batch:
            return

        cold_href = cold_batch.get("href")

        # 3. LFS object download — cold
        if cold_href:
            self._lfs_download_timed(repo_case, cold_href, oid, size, "lfs_download_cold")

        # 4. LFS batch API — warm (second request, cache hit)
        warm_batch = self._lfs_batch_timed(repo_case, [{"oid": oid, "size": size}], "lfs_batch_warm")

        # 5. LFS object download — warm
        warm_href = warm_batch.get("href") if warm_batch else cold_href
        if warm_href:
            self._lfs_download_timed(repo_case, warm_href, oid, size, "lfs_download_warm")

        # 6. Multi-object batch
        multi_objects = []
        for p in pointers[:3]:
            text = p.read_text(errors="replace")
            o = self._extract_oid(text)
            sm = re.search(r"size (\d+)", text)
            if o and sm:
                multi_objects.append({"oid": o, "size": int(sm.group(1))})
        if len(multi_objects) > 1:
            self._lfs_batch_timed(repo_case, multi_objects, "lfs_batch_multi_cold", expect_count=len(multi_objects))
            self._lfs_batch_timed(repo_case, multi_objects, "lfs_batch_multi_warm", expect_count=len(multi_objects))

        # 7. Upload rejected
        self._lfs_upload_rejected(repo_case)

        # 8. Invalid OID rejected
        self._lfs_invalid_oid(repo_case)

    def _lfs_batch_url(self, repo: str) -> str:
        return f"{self.git_url(repo)}/info/lfs/objects/batch"

    def _lfs_batch_timed(
        self,
        repo_case: RepoCase,
        objects: list[dict[str, Any]],
        label: str,
        *,
        expect_count: int = 1,
    ) -> dict[str, Any] | None:
        body = json.dumps({
            "operation": "download",
            "transfers": ["basic"],
            "objects": objects,
        }).encode()
        req = urllib.request.Request(
            self._lfs_batch_url(repo_case.repo),
            data=body, method="POST",
            headers={"Content-Type": LFS_CONTENT_TYPE},
        )
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                data = json.loads(resp.read().decode())
        except Exception as exc:
            elapsed = time.monotonic() - t0
            self.record({
                "case": label, "repo": repo_case.repo,
                "duration_s": round(elapsed, 3),
                "status": "failed", "error": str(exc),
            })
            self.failures.append(f"{label} failed for {repo_case.repo}: {exc}")
            return None
        elapsed = time.monotonic() - t0

        objs = data.get("objects", [])
        all_have_actions = all("actions" in o for o in objs)
        passed = len(objs) == expect_count and all_have_actions
        self.record({
            "case": label,
            "repo": repo_case.repo,
            "branch": repo_case.branch,
            "duration_s": round(elapsed, 3),
            "object_count": len(objs),
            "all_have_actions": all_have_actions,
            "status": "passed" if passed else "failed",
        })
        if not passed:
            self.failures.append(f"{label} failed for {repo_case.repo}")
            return None

        first = objs[0]
        href = first.get("actions", {}).get("download", {}).get("href")
        return {"href": href, "oid": first.get("oid"), "size": first.get("size")}

    def _lfs_download_timed(
        self,
        repo_case: RepoCase,
        href: str,
        oid: str,
        expected_size: int,
        label: str,
    ) -> None:
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(href, timeout=60) as resp:
                data = resp.read()
        except Exception as exc:
            elapsed = time.monotonic() - t0
            self.record({
                "case": label, "repo": repo_case.repo,
                "oid": oid, "duration_s": round(elapsed, 3),
                "status": "failed", "error": str(exc),
            })
            self.failures.append(f"{label} failed for {repo_case.repo}: {exc}")
            return
        elapsed = time.monotonic() - t0
        passed = len(data) == expected_size
        self.record({
            "case": label,
            "repo": repo_case.repo,
            "oid": oid,
            "expected_size": expected_size,
            "actual_size": len(data),
            "duration_s": round(elapsed, 3),
            "throughput_mbps": round(len(data) / elapsed / 1_000_000, 2) if elapsed > 0 else None,
            "status": "passed" if passed else "failed",
        })
        if not passed:
            self.failures.append(f"{label} size mismatch for {repo_case.repo}: {len(data)} != {expected_size}")

    def _lfs_upload_rejected(self, repo_case: RepoCase) -> None:
        body = json.dumps({
            "operation": "upload",
            "transfers": ["basic"],
            "objects": [{"oid": "a" * 64, "size": 100}],
        }).encode()
        req = urllib.request.Request(
            self._lfs_batch_url(repo_case.repo),
            data=body, method="POST",
            headers={"Content-Type": LFS_CONTENT_TYPE},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                status = resp.status
        except urllib.error.HTTPError as exc:
            status = exc.code
        passed = status == 405
        self.record({
            "case": "lfs_upload_rejected",
            "repo": repo_case.repo,
            "http_status": status,
            "status": "passed" if passed else "failed",
        })
        if not passed:
            self.failures.append(f"LFS upload not rejected for {repo_case.repo}: HTTP {status}")

    def _lfs_invalid_oid(self, repo_case: RepoCase) -> None:
        body = json.dumps({
            "operation": "download",
            "transfers": ["basic"],
            "objects": [{"oid": "not-a-valid-oid", "size": 100}],
        }).encode()
        req = urllib.request.Request(
            self._lfs_batch_url(repo_case.repo),
            data=body, method="POST",
            headers={"Content-Type": LFS_CONTENT_TYPE},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                data = json.loads(resp.read().decode())
        except urllib.error.HTTPError:
            self.record({"case": "lfs_invalid_oid", "repo": repo_case.repo, "status": "passed"})
            return
        obj = data.get("objects", [{}])[0]
        has_error = "error" in obj
        self.record({
            "case": "lfs_invalid_oid",
            "repo": repo_case.repo,
            "has_error": has_error,
            "error": obj.get("error"),
            "status": "passed" if has_error else "failed",
        })
        if not has_error:
            self.failures.append(f"LFS invalid OID not rejected for {repo_case.repo}")

    @staticmethod
    def _find_lfs_pointer_files(tree: Path) -> list[Path]:
        pointers: list[Path] = []
        for path in tree.rglob("*"):
            if not path.is_file() or ".git" in path.parts:
                continue
            try:
                text = path.read_text(errors="replace")
            except OSError:
                continue
            if LFS_POINTER_RE.match(text):
                pointers.append(path)
        return pointers

    @staticmethod
    def _extract_oid(content: str) -> str | None:
        m = re.search(r"oid sha256:([0-9a-f]{64})", content)
        return m.group(1) if m else None

    # ── infrastructure helpers ─────────────────────────────────────────

    def assert_receive_pack_rejected(self) -> None:
        url = (
            f"{self.base_url}/git/github.com/astral-sh/uv.git/info/refs"
            "?service=git-receive-pack"
        )
        started = time.monotonic()
        try:
            urllib.request.urlopen(url, timeout=30)
            status = 200
        except urllib.error.HTTPError as error:
            status = error.code
        self.record(
            {
                "case": "receive_pack_rejected",
                "status_code": status,
                "duration_s": round(time.monotonic() - started, 3),
                "status": "passed" if status == 405 else "failed",
            }
        )
        if status != 405:
            self.failures.append(f"receive-pack returned HTTP {status}, expected 405")


def safe_name(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "-" for ch in value)


def depth1_blobless_clone_cmd(url: str, branch: str, checkout: Path) -> list[str]:
    return [
        "git",
        "clone",
        "--depth",
        "1",
        "--filter=blob:none",
        "--no-checkout",
        "--branch",
        branch,
        "--single-branch",
        "--no-tags",
        url,
        str(checkout),
    ]


if __name__ == "__main__":
    unittest.main(verbosity=2)
