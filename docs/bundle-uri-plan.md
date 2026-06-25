# bundle-uri: Fused Implementation Plan

**Status:** plan (not implemented). Pick-up-ready for an implementing agent.

This fuses three inputs:
1. `docs/clean-slate-design.md` item (c) — the original bundle-uri offload bet ("prototype (c) first").
2. **PR #76** (`devin/1781082659-bundle-uri`) — the first attempt: it built protocol-v2 + bundle-uri advertisement, but on the pre-#79 generation-**bundle-chain** layout. #79 then replaced that layer with content-addressed packs, so #76 is now `CONFLICTING`/semantically dead (`bundle_key`/`bundle_create` no longer exist). Do not merge it as-is.
3. **This session's operational findings** — why bundle-uri is worth doing and what it must account for (below).

## Why (grounded in measured findings)

bundle-uri tells a capable client: *download the repo's base bytes directly from S3/CDN, then fetch only a small top-up pack from the cache node.* That directly attacks problems we measured:

- **Cold-clone byte cost:** linux read-through ≈ **233 s** vs ~157 s direct GitHub (`docs/cold-read-through-performance.md`). bundle-uri moves the heavy base bytes off the node entirely.
- **Cloudflare ~100 s origin cap:** heavy node→client transfers (deepen=40, materialize) time out at the front door (HTTP 524). With bundle-uri the node response is a *small* top-up pack → no 100 s risk for the base.
- **No-bitmap `pack-objects` + lock contention:** the node's served repo has bitmaps disabled, so cold full-clone `pack-objects` is an expensive O(history) walk, and it contends with serving maintenance on the per-repo mutation lock (see `docs/serving-maintenance-cost.md`). A pre-built base bundle means the node doesn't regenerate the base for every cold clone.
- **Proxy-on-miss simplification:** bundle-uri collapses the cold-miss-proxy-vs-warm gap that proxy-on-miss exists to paper over.

## Foundation reality (post-#79) — what changed under #76

- Durable layer is now **content-addressed packs** + snapshot/generation manifests + a CAS'd head (`repos/{repo}/packs/pack-{sha256}.pack`, `put_if_absent`, `write_repo_generation_head_if_version_matches`). Generation **bundles are gone**.
- **bundle-uri serves bundle-*framed* files** (header + ref list + packfile). A content-addressed `.pack` is **not** a bundle. So we must publish a real bundle artifact — exactly the doc's note: *"the bundle-uri base bundle is the base pack + its ref list wrapped in bundle framing; produce one artifact, serve it both ways."*
- The direct-git GET still synthesizes a **v0-only** advertisement; bundle-uri requires **protocol v2**. (My matrix clones worked via v2→v0 fallback, confirming v2 isn't in main.)

## Plan (phased)

### Phase 1 — Protocol v2 negotiation (salvage from #76, low risk)
The reusable, foundation-independent half of #76. Conflicts here are **textual only**.
- `GET info/refs` with `Git-Protocol: version=2` → same upstream `ls-remote` access proof, then a synthesized v2 capability advertisement (`materializer/protocol_v2.rs`).
- `POST upload-pack` v2 bodies: `ls-refs` (synthesized from `UpstreamRefComparison`, honoring `ref-prefix`/`symrefs`), `fetch` (existing read-through, spawned with `GIT_PROTOCOL=version=2`), unknown command → 400.
- Land this as its own PR with the `protocol_v2_integration.rs` suite. **No bundle-uri yet.** Independently valuable and a prerequisite.

### Phase 2 — Publish a base bundle as a byproduct of generation publish/compaction
- When a generation/snapshot is published (and on the compaction repack), also emit a **base bundle** over the snapshot's refs: `git bundle create` (or wrap the repack's base pack + ref list in bundle framing — "one artifact, serve it both ways").
- Store it **content-addressed**: `repos/{repo}/bundles/bundle-{sha256}.bundle`, `put_if_absent` (free dedupe / idempotent). Record its key + sha256 in the snapshot manifest.
- **Do this in the async/background publish path, never on a client request** (see Cross-cutting → Freshness). The repack output is already produced by serving maintenance/compaction — reuse it so the bundle isn't a separate full `pack-objects`.
- Files: `crates/git-cache-domain/src/materializer/generations.rs` (publish/compaction), `crates/git-cache-objectstore/src/manifests.rs` (new `bundle_key`, snapshot field), `crates/git-cache-git/src/lib.rs` (`bundle_create`).

### Phase 3 — Advertise bundle-uri from the snapshot layout
- Re-implement the advertisement on the **content-addressed snapshot** (not the deleted chain): when enabled + a base URL is set + a published snapshot with a bundle exists, emit `bundle.<id>.uri = {base_url}/{bundle_key}` with `bundle.mode=all`. Empty list when disabled / no snapshot / no bundle / ineligible request (see scoping).
- Config (from #76): `git_remote.bundle_uri_enabled` (default **false**), `git_remote.bundle_uri_base_url`. Env `GIT_CACHE_GIT_REMOTE_BUNDLE_URI_ENABLED`, `..._BASE_URL`.
- **No node bundle-serving endpoint** — point `bundle_uri_base_url` at whatever fronts the object store `bundles/` keys (CloudFront/S3; the existing Cloudflare front door in `deploy-cloudflare-frontdoor` can serve them). Client then fetches the small top-up from the node.

### Phase 4 — Measure & roll out (off by default → opt-in)
Validate on dev/origin (same method as this session): a bundle-uri-capable client (git ≥ 2.38, `transfer.bundleURI=true`) cold-cloning linux should pull the base from the CDN and only top-up from the node.

## Cross-cutting concerns (the "fusion" — these gate real value)

1. **Freshness coupling (hard dependency).** A stale base bundle ⇒ a *large* top-up ⇒ bundle-uri buys little. Today linux's durable generation is ~a day stale because the only refresh path (synchronous `/v1/materialize`) times out at Cloudflare's 100 s and gets cancelled before publishing. **bundle-uri is only worthwhile alongside the freshness fix:** async/non-cancellable materialize (the `?async` lane) + a scheduled warm of hot repos so the published base (and its bundle) stays current. Track these together.
2. **Private-repo URL safety.** A bundle at a public CDN URL leaks repo bytes to anyone with the URL. Fine for **public** upstreams (the main workload). For request-authenticated/private repos, either **don't advertise bundle-uri** or use **signed/expiring URLs**. Gate the advertisement on "unauthenticated/public repo" initially.
3. **Shallow / blobless scoping.** A base bundle is full-history; advertising it to a `--depth=N` or `--filter=blob:none` client is wrong/wasteful — and shallow/blobless is the bulk of the traffic we've been exercising. **Scope Phase 3 to full, unfiltered clones**; leave shallow/blobless on the existing read-through path. (A future blobless base bundle is possible but out of scope.)
4. **Old clients.** git < 2.38 or without `transfer.bundleURI` ignore the advertisement → normal clone. Off by default makes rollout safe.
5. **git version gate.** bundle-uri client support is git ≥ 2.38; `git bundle create` is ancient (fine). The deploy image installs git unpinned — assert the *node* git is recent enough for the v2 + bundle paths, consistent with the pinning caution in `cold-read-through-performance.md`.

## Acceptance criteria
- Phase 1: v2 `clone`/`ls-remote`/`fetch` work against the direct endpoint; old-client v0 path unchanged; `protocol_v2_integration.rs` green.
- Phase 2: publishing a generation also writes `bundles/bundle-{sha256}.bundle` (idempotent), referenced from the snapshot manifest; produced off the client path.
- Phase 3: with `bundle_uri_enabled=true` + base URL, a git ≥ 2.38 full clone fetches the base from the CDN and a **small** top-up from the node; node response stays well under the Cloudflare 100 s cap; disabled/shallow/filtered/private requests get **no** bundle-uri advertisement.
- End-to-end: cold full clone of linux via bundle-uri beats the ~233 s read-through, with node bytes/CPU ≪ today.

## Recommendation
Do **Phase 1 now** (rebase #76's v2 half into a clean PR — textual-only conflicts, independently useful). Then Phases 2–3 as a fresh implementation on the content-addressed layout, **landed together with the freshness fix** (async materialize + scheduled warm) since bundle-uri's payoff depends on a fresh base. Close #76 once Phase 1 is extracted.

## References
- `docs/clean-slate-design.md` — items (b) v2 cached advertisement, (c) bundle-uri, (d) content-addressed packs
- `docs/cold-read-through-performance.md` — cold-serve costs, tee-import, bitmap/git-version caveats
- `docs/serving-maintenance-cost.md` — per-repo mutation-lock contention, no-bitmap `pack-objects`
- PR #76 (v2 + bundle-uri attempt, pre-#79), #79 (content-addressed packs), #81 (CAS head)
