---
name: deploy-cloudflare-frontdoor
description: Deploy or update the gitcache.sh Cloudflare front door for gitmirrorcache. Use when changing the static landing page under site/, pulling a landing page from a PR, deploying the Cloudflare Worker/static assets, wiring or checking gitcache.sh custom domains, or verifying that https://gitcache.sh/git/... still proxies to the prod ALB.
---

# Deploy Cloudflare Front Door

Requirements: local repo, network access, AWS CLI credentials for the prod ALB
lookup, and Wrangler authenticated with Cloudflare OAuth or
`CLOUDFLARE_API_TOKEN`. Do not mutate ECS unless the user explicitly asks for a
prod backend deploy.

## Architecture

- `site/index.html` is the static landing page.
- `site/worker.js` serves static assets and proxies `/git/*`, `/v1/*`,
  `/healthz`, and `/metrics` to `API_ORIGIN`.
- `wrangler.jsonc` attaches the Worker to `gitcache.sh` and `www.gitcache.sh`.
- `scripts/cloudflare/deploy-static-site.sh` resolves
  `gitmirrorcache-prod-ec2-alb` and deploys with
  `API_ORIGIN=http://<alb-dns-name>`.

The public Git remote must remain:

```bash
git clone https://gitcache.sh/git/github.com/org/repo.git
```

## Update Landing Page From A PR

1. Inspect the working tree first; do not revert unrelated edits:

   ```bash
   git status --short
   ```

2. Fetch the PR and inspect the changed files:

   ```bash
   git fetch origin pull/<number>/head:refs/remotes/origin/pr-<number>
   git diff --name-status origin/main...origin/pr-<number>
   ```

3. If the PR only changes `site/index.html`, restore just that file:

   ```bash
   git restore --source=origin/pr-<number> -- site/index.html
   ```

4. Keep `site/worker.js`, `site/_headers`, `site/robots.txt`, and
   `wrangler.jsonc` from this branch unless the PR intentionally updates them.
   Remove stale unreferenced static assets from the old landing page.

## Validate Before Deploying

Run a Wrangler dry run through the checked-in helper:

```bash
WRANGLER_DRY_RUN=true AWS_REGION=${AWS_REGION:-us-west-2} \
  scripts/cloudflare/deploy-static-site.sh
```

If Wrangler says it is unauthenticated:

```bash
npx wrangler whoami
npx wrangler login
```

In a non-interactive shell, use `CLOUDFLARE_API_TOKEN` instead.

## Deploy

```bash
AWS_REGION=${AWS_REGION:-us-west-2} scripts/cloudflare/deploy-static-site.sh
```

Expected output includes:

- `Uploaded gitcache-site`
- `gitcache.sh (custom domain)`
- `www.gitcache.sh (custom domain)`
- a `Current Version ID`

## Verify

Check the landing page contains a marker from the expected revision:

```bash
curl -fsS https://gitcache.sh/ | rg 'Clone once|gitcache\\.sh/git|theme'
```

Check the Worker proxy and backend health:

```bash
curl -fsS https://gitcache.sh/healthz
git ls-remote https://gitcache.sh/git/github.com/octocat/Hello-World.git HEAD
```

If local DNS is stale, verify against a known Cloudflare A record:

```bash
dig @1.1.1.1 gitcache.sh A +short
curl -fsS --resolve gitcache.sh:443:<cloudflare-a-ip> https://gitcache.sh/healthz
git -c http.curloptResolve=gitcache.sh:443:<cloudflare-a-ip> \
  ls-remote https://gitcache.sh/git/github.com/octocat/Hello-World.git HEAD
```

## DNS Gotchas

Cloudflare custom domain deployment may create authoritative A/AAAA records
before the local macOS resolver drops a cached negative answer. If Firefox or
curl still reports `Server Not Found` while public DNS works, check:

```bash
dig @ullis.ns.cloudflare.com gitcache.sh A +short
dig @1.1.1.1 gitcache.sh A +short
dns-sd -G v4 gitcache.sh
```

If `dns-sd` reports `No Such Record` with a TTL, the local resolver is stale.
It will expire naturally, or the user can flush it:

```bash
sudo dscacheutil -flushcache
sudo killall -HUP mDNSResponder
```

## Prod Backend Checks

Only use these to verify the existing backend unless the user asks to deploy
prod:

```bash
AWS_REGION=${AWS_REGION:-us-west-2} aws ecs describe-services \
  --cluster gitmirrorcache-prod-ec2 \
  --services gitmirrorcache-prod-ec2-api \
  --query 'services[0].{Status:status,Desired:desiredCount,Running:runningCount,Pending:pendingCount}'

curl -fsS http://gitmirrorcache-prod-ec2-alb-1771127492.us-west-2.elb.amazonaws.com/healthz
```

If the backend ALB DNS changes, let `scripts/cloudflare/deploy-static-site.sh`
resolve it automatically. Override with `API_ORIGIN=` only for deliberate
testing.

## Cleanup

Wrangler may create `.wrangler/` cache files in the repo. Remove them before
finishing unless they are intentionally tracked:

```bash
rm -rf .wrangler
git status --short
```
