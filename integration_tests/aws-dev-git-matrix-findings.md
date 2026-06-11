# AWS Dev Git Matrix Findings

Run date: 2026-06-11 UTC

Dev target:
`http://<dev-alb-dns>.us-west-2.elb.amazonaws.com`

Image:
`<account-id>.dkr.ecr.us-west-2.amazonaws.com/gitmirrorcache-arm:6443d09`

## Coverage

The AWS dev matrix covered:

- upstream and cache `ls-remote` correctness
- direct GitHub depth-1 blobless baseline timing
- cache proxy-on-miss cold and hot depth-1 blobless clones
- cache read-through cold and hot depth-1 blobless clones with
  `git-cache-use-proxy-on-miss: false`
- request-scoped Basic auth proxy-on-miss depth-1 blobless clones
- blobless-to-full depth-1 transitions for `astral-sh/uv` and `astral-sh/ruff`
- receive-pack rejection
- heavy full-history blobless read-through with proxy forced off
- direct GitHub heavy comparisons for the same heavy clone shapes

## Standard Matrix

Command:

```sh
RUN_AWS_DEV_GIT_MATRIX=1 \
GIT_CACHE_AWS_DEV_BASE_URL=http://<dev-alb-dns>.us-west-2.elb.amazonaws.com \
GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE=1 \
GIT_CACHE_AWS_DEV_RESULTS=/tmp/gitcache-aws-dev-matrix-20260611T0109.jsonl \
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm AWS_PAGER= \
python3 -m unittest -v integration_tests.test_aws_dev_git_matrix
```

Result: `Ran 1 test in 151.464s OK`.

| Repo | Direct GitHub depth-1 blobless | Proxy cold | Proxy hot | Read-through cold | Read-through hot | Basic proxy cold | Basic proxy hot |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `astral-sh/uv` | `1.623s` | `0.827s` | `0.717s` | `1.490s` | `0.840s` | `1.215s` | `0.747s` |
| `astral-sh/ruff` | `1.893s` | `0.947s` | `0.742s` | `1.144s` | `0.792s` | `1.636s` | `0.879s` |
| `torvalds/linux` | `2.383s` | `1.142s` | `0.809s` | `1.602s` | `0.757s` | `1.242s` | `0.835s` |
| `llvm/llvm-project` | `2.706s` | `1.695s` | `1.118s` | `2.575s` | `1.125s` | `2.604s` | `1.595s` |

Blobless-to-full depth-1 transition timings:

| Repo | Transition timing |
| --- | ---: |
| `astral-sh/uv` | `2.288s` |
| `astral-sh/ruff` | `4.847s` |

All HEAD checks matched upstream. Depth-1 clones had one commit and were shallow.
`git-receive-pack` returned HTTP 405.

## Heavy Proxy-Off Matrix

Command:

```sh
RUN_AWS_DEV_GIT_MATRIX=1 \
GIT_CACHE_AWS_DEV_BASE_URL=http://<dev-alb-dns>.us-west-2.elb.amazonaws.com \
GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE=1 \
GIT_CACHE_AWS_DEV_SKIP_STANDARD=1 \
GIT_CACHE_AWS_DEV_TIER=heavy \
GIT_CACHE_AWS_DEV_USE_GH_TOKEN=0 \
GIT_CACHE_AWS_DEV_COMMAND_TIMEOUT=7200 \
GIT_CACHE_AWS_DEV_RESULTS=/tmp/gitcache-aws-dev-heavy-proxyoff-20260611T0118.jsonl \
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm AWS_PAGER= \
python3 -m unittest -v integration_tests.test_aws_dev_git_matrix
```

Result: `Ran 1 test in 533.181s OK`.

| Repo | Heavy proxy-off case | Time | Correctness |
| --- | --- | ---: | --- |
| `astral-sh/uv` | full-history blobless no-checkout | `2.580s` | HEAD matched, 1000-log walk passed |
| `astral-sh/uv` | blobless full checkout | `7.741s` | clean checkout, 1479 tracked files |
| `astral-sh/ruff` | full-history blobless no-checkout | `4.597s` | HEAD matched, 1000-log walk passed |
| `astral-sh/ruff` | blobless full checkout | `14.731s` | clean checkout, 10594 tracked files |
| `torvalds/linux` | full-history blobless no-checkout | `233.921s` | HEAD matched, 1000-log walk passed |
| `llvm/llvm-project` | full-history blobless no-checkout | `241.279s` | HEAD matched, 1000-log walk passed |

During the large proxy-off lanes the dev API was doing real hydration,
connectivity checks, and pack work. Linux reached about 2.1 GiB in
`/cache/repos`, with one pack containing 8,429,949 objects.

## Direct GitHub Heavy Comparison

Direct heavy comparison was measured from the local client to GitHub with the
same full-history blobless no-checkout shape for all repos, plus blobless full
checkout for `uv` and `ruff`.

| Repo | Direct GitHub heavy case | Direct GitHub | AWS proxy-off |
| --- | --- | ---: | ---: |
| `astral-sh/uv` | full-history blobless no-checkout | `6.630s` | `2.580s` |
| `astral-sh/uv` | blobless full checkout | `8.378s` | `7.741s` |
| `astral-sh/ruff` | full-history blobless no-checkout | `4.361s` | `4.597s` |
| `astral-sh/ruff` | blobless full checkout | `16.819s` | `14.731s` |
| `torvalds/linux` | full-history blobless no-checkout | `157.707s` | `233.921s` |
| `llvm/llvm-project` | full-history blobless no-checkout | `74.500s` | `241.279s` |

## Takeaways

- The cache path was correct for every measured scenario: HEADs matched
  upstream, shallow semantics held for depth-1 clones, full checkouts were clean,
  and receive-pack was rejected.
- Proxy-on-miss is the right default for ordinary cold HTTP(S) misses. Forcing
  proxy-off shifts large-repo hydration and pack generation into client-visible
  latency.
- The proxy-off heavy lanes are still useful as explicit cache-fill correctness
  and speed tests. They should remain opt-in and use a long command timeout.
- For giant full-history blobless clones, direct GitHub was faster than AWS
  proxy-off in this run: Linux was 1.48x faster direct, and LLVM was 3.24x
  faster direct.
