# Docker / MinIO Integration Work Log

## Detailed plan

1. Inspect the object store, domain materializer, and API wiring before changing code.
2. Keep MinIO tests opt-in or CI-scoped so normal local/unit test workflows do not require Docker.
3. Add a local Docker Compose MinIO service with stable test credentials and bucket bootstrap.
4. Add S3-feature integration tests that use MinIO through the existing `ObjectStore` trait boundary.
5. Add materializer tests that combine MinIO durable storage with local disk hot-cache/block-store paths, then delete hot-cache repos to prove cold hydration works.
6. Run default Rust checks and explicit MinIO checks.

## Assumptions

- Docker is available in CI and in developer environments that want to run these tests.
- MinIO is close enough to S3 for object-store contract coverage: object PUT/GET/HEAD/DELETE, conditional `If-None-Match: *`, prefix listing, and bucket creation.
- Production S3 wiring can remain separate from API startup; these tests can instantiate `S3ObjectStore` directly and inject it into domain `AppState`.
- Local disk cache/block-store semantics are represented by `cache_root`; object storage remains the durable source of truth.

## Intended symbol-level changes

1. `S3ObjectStore::new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Result<Self>`
   - No production signature change planned. New tests will construct AWS SDK clients configured for MinIO path-style access and pass them through this constructor.
2. `impl ObjectStore for S3ObjectStore`
   - Validate existing `get`, `put`, `put_if_absent`, `head`, `delete`, `list_prefix`, and `put_file` behavior against a real S3-compatible server.
3. `pub struct AppState { pub config: AppConfig, pub store: Arc<dyn ObjectStore>, pub git: Git, pub disk: AsyncDiskManager }`
   - Use direct test construction with `Arc<S3ObjectStore>` so domain tests exercise MinIO durable storage and local block storage together.
4. `pub async fn Materializer::materialize(&self, request: MaterializeRequest) -> CoreResult<MaterializeResponse>`
   - Exercise normal branch publish and exact-commit cached materialization through MinIO-backed manifests and bundles.
5. `async fn Materializer::hydrate_generation(&self, repo: &RepoKey, repo_dir: &FsPath, generation: GenerationId) -> CoreResult<()>`
   - Verify hot-cache deletion triggers bundle hydration from MinIO.
6. `pub async fn Materializer::compact_generation_chain(&self, repo: &RepoKey) -> CoreResult<Option<CompactionReport>>`
   - Verify compaction writes a new generation, repoints manifests, deletes old MinIO generation objects, and still hydrates commits after hot-cache deletion.

## Changes made

- Added `docker-compose.minio.yml` with MinIO plus a one-shot bucket bootstrap service for `gitmirrorcache-test`.
- Added `config/minio.example.toml` to show local S3-compatible object-store configuration.
- Added AWS SDK credential/type dev dependencies and `git-cache-domain` feature `s3-tests` to compile MinIO-specific domain tests only when requested.
- Added MinIO object-store tests for `put/get/head/exists/delete`, `put_if_absent`, manifest publish helpers, prefix isolation, and bounded listing.
- Added MinIO materializer tests that publish generations into MinIO, delete local hot-cache repos, hydrate exact commits from bundles, and compact generation chains.
- Updated S3 error classification to inspect debug metadata so AWS SDK `NoSuchKey` and `PreconditionFailed` service errors map correctly to `None` / `false`.
- Added CI job that starts MinIO with Docker Compose and runs the S3-feature object-store and materializer tests.

## Tradeoffs

- Using Docker Compose keeps local MinIO setup transparent and reproducible, but tests depend on Docker when explicitly enabled.
- Direct domain `AppState` construction avoids adding incomplete production S3 API startup wiring in this PR.
- Tests use small local Git fixtures instead of public GitHub repos to keep them deterministic and fast.
