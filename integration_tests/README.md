# Integration Tests

These tests are intentionally opt-in because they hit the real GitHub repository
`astral-sh/uv` and may take time to fetch and bundle repository objects.

They use only Python's standard library and shell out to `cargo` and `git`.

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
```

Optional overrides:

```sh
GIT_CACHE_TEST_REPO=github.com/astral-sh/uv \
GIT_CACHE_TEST_BRANCH=main \
RUN_GITHUB_INTEGRATION=1 \
python3 -m unittest -v integration_tests.test_astral_uv
```

What the tests do:

- build and start `git-cache-api` on a random localhost port
- materialize `github.com/astral-sh/uv` `main` in strict mode
- compare the returned commit to `git ls-remote`
- fetch the returned session ref with `git fetch`
- resolve an abbreviated `short_commit` selector to the canonical full commit
- delete local hot-cache repos and verify exact commit materialization rehydrates from object storage with `cache_verified`
- materialize the upstream default branch
