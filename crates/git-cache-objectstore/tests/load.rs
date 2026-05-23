//! Load tests for the object store.

use bytes::Bytes;
use git_cache_core::{GenerationId, GenerationManifest, RepoKey};
use git_cache_objectstore::{
    read_generation_manifest, write_generation_manifest, LocalObjectStore, ObjectStore,
};
use std::sync::Arc;
use tempfile::TempDir;

fn make_store(tmp: &TempDir) -> LocalObjectStore {
    LocalObjectStore::new(tmp.path().join("objects"))
}

fn repo_key(n: usize) -> RepoKey {
    RepoKey::parse(format!("github.com/org/repo-{n}")).unwrap()
}

fn make_generation_manifest(repo: &RepoKey, gen_id: GenerationId) -> GenerationManifest {
    GenerationManifest {
        repo: repo.clone(),
        generation: gen_id,
        bundle_key: format!("bundles/{}/{}.bundle", repo.as_str(), gen_id),
        parent_generation: None,
        created_at: chrono::Utc::now(),
        commits: vec![],
    }
}

// ── 1. Bulk manifest writes ─────────────────────────────────────────────

#[tokio::test]
async fn bulk_manifest_writes() {
    let tmp = TempDir::new().unwrap();
    let store = make_store(&tmp);

    let mut manifests = Vec::new();
    for i in 0..100 {
        let repo = repo_key(i);
        let gen_id = GenerationId::new();
        let manifest = make_generation_manifest(&repo, gen_id);
        write_generation_manifest(&store, &manifest)
            .await
            .unwrap_or_else(|e| panic!("write manifest {i}: {e}"));
        manifests.push((repo, gen_id, manifest));
    }

    for (i, (repo, gen_id, expected)) in manifests.iter().enumerate() {
        let read_back = read_generation_manifest(&store, repo, *gen_id)
            .await
            .unwrap_or_else(|e| panic!("read manifest {i}: {e}"))
            .unwrap_or_else(|| panic!("manifest {i} not found"));

        assert_eq!(read_back.repo, expected.repo);
        assert_eq!(read_back.generation, expected.generation);
        assert_eq!(read_back.bundle_key, expected.bundle_key);
    }
}

// ── 2. Large bundle simulation (50MB) ───────────────────────────────────

#[tokio::test]
async fn large_bundle_simulation() {
    let tmp = TempDir::new().unwrap();
    let store = make_store(&tmp);

    let size = 50 * 1024 * 1024; // 50MB
    let data = Bytes::from(vec![0xCDu8; size]);

    store
        .put(
            "bundles/github.com/org/large-repo/gen1.bundle",
            data.clone(),
        )
        .await
        .expect("put 50MB object");

    let read_back = store
        .get("bundles/github.com/org/large-repo/gen1.bundle")
        .await
        .expect("get 50MB object")
        .expect("50MB object should exist");

    assert_eq!(read_back.len(), size);
    assert_eq!(read_back, data);
}

// ── 3. Sustained throughput ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn sustained_throughput() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));

    let tasks = 50;
    let objects_per_task = 20;

    let mut handles = Vec::new();
    for t in 0..tasks {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..objects_per_task {
                let key = format!("throughput/task-{t}/obj-{i}.bin");
                let payload = Bytes::from(format!("task-{t}-obj-{i}-payload-data"));
                s.put(&key, payload).await.expect("put should succeed");
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Read all 1000 objects back
    for t in 0..tasks {
        for i in 0..objects_per_task {
            let key = format!("throughput/task-{t}/obj-{i}.bin");
            let data = store
                .get(&key)
                .await
                .expect("get should succeed")
                .unwrap_or_else(|| panic!("object {key} should exist"));

            let expected = format!("task-{t}-obj-{i}-payload-data");
            assert_eq!(
                String::from_utf8(data.to_vec()).unwrap(),
                expected,
                "data mismatch for {key}"
            );
        }
    }
}

// ── 4. Prefix listing at scale ──────────────────────────────────────────

#[tokio::test]
async fn prefix_listing_at_scale() {
    let tmp = TempDir::new().unwrap();
    let store = make_store(&tmp);

    // Write 500 objects with structured key prefixes
    for host in 0..5 {
        for org in 0..10 {
            for repo in 0..10 {
                let key = format!("repos/host-{host}/org-{org}/repo-{repo}/manifest.json");
                let payload = Bytes::from(format!("manifest-{host}-{org}-{repo}"));
                store.put(&key, payload).await.expect("put should succeed");
            }
        }
    }

    // List all
    let all = store.list_prefix("repos", None).await.expect("list all");
    assert_eq!(all.len(), 500, "should have 500 objects total");

    // List by host
    let host0 = store
        .list_prefix("repos/host-0", None)
        .await
        .expect("list host-0");
    assert_eq!(host0.len(), 100, "host-0 should have 100 objects");

    // List by host/org
    let org0 = store
        .list_prefix("repos/host-0/org-0", None)
        .await
        .expect("list host-0/org-0");
    assert_eq!(org0.len(), 10, "host-0/org-0 should have 10 objects");

    // List by host/org/repo (single object)
    let single = store
        .list_prefix("repos/host-0/org-0/repo-0", None)
        .await
        .expect("list single repo");
    assert_eq!(single.len(), 1, "single repo should have 1 object");

    // List nonexistent prefix
    let empty = store
        .list_prefix("repos/nonexistent", None)
        .await
        .expect("list nonexistent");
    assert!(empty.is_empty(), "nonexistent prefix should be empty");
}
