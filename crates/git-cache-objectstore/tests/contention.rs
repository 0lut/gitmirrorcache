//! Resource contention tests for the ObjectStore (LocalObjectStore).
//!
//! These tests stress concurrent writes, reads, deletes, and conditional
//! operations to verify atomicity and consistency guarantees.

use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::join_all;
use git_cache_core::{GenerationId, GenerationManifest, RepoKey};
use git_cache_objectstore::{
    acquire_lease, write_generation_manifest_if_absent_or_matches, LocalObjectStore, ObjectStore,
};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Barrier;

fn make_store(tmp: &TempDir) -> LocalObjectStore {
    LocalObjectStore::new(tmp.path().join("objects"))
}

fn repo() -> RepoKey {
    RepoKey::parse("github.com/org/repo").unwrap()
}

fn commit_sha(c: char) -> git_cache_core::CommitSha {
    git_cache_core::CommitSha::parse(c.to_string().repeat(40)).unwrap()
}

// ── 1. Concurrent put to same key ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_put_to_same_key_only_one_wins() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));
    let barrier = Arc::new(Barrier::new(20));

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let s = Arc::clone(&store);
            let bar = Arc::clone(&barrier);
            tokio::spawn(async move {
                bar.wait().await;
                s.put("contention/key.dat", Bytes::from(format!("writer-{i}")))
                    .await
            })
        })
        .collect();

    let results: Vec<_> = join_all(handles).await;
    for r in &results {
        r.as_ref().unwrap().as_ref().unwrap();
    }

    // Read back: should be consistent (one complete write).
    let data = store.get("contention/key.dat").await.unwrap().unwrap();
    let value = String::from_utf8(data.to_vec()).unwrap();
    assert!(
        value.starts_with("writer-"),
        "value should be from one writer, got: {value}"
    );
}

// ── 2. Concurrent put_if_absent ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_put_if_absent_exactly_one_wins() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));
    let barrier = Arc::new(Barrier::new(50));

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let s = Arc::clone(&store);
            let bar = Arc::clone(&barrier);
            tokio::spawn(async move {
                bar.wait().await;
                let won = s
                    .put_if_absent("race/winner.dat", Bytes::from(format!("task-{i}")))
                    .await
                    .unwrap();
                (i, won)
            })
        })
        .collect();

    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let winners: Vec<_> = results.iter().filter(|(_, won)| *won).collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one task should win put_if_absent"
    );

    let winner_idx = winners[0].0;
    let stored = store.get("race/winner.dat").await.unwrap().unwrap();
    assert_eq!(
        stored,
        Bytes::from(format!("task-{winner_idx}")),
        "stored value must match the winner"
    );
}

// ── 3. Read during write ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn read_during_write_never_sees_partial_data() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));

    let large_data = Bytes::from(vec![0xABu8; 1024 * 1024]);

    let writer_store = Arc::clone(&store);
    let writer_data = large_data.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .put("large/object.bin", writer_data)
            .await
            .unwrap();
    });

    let reader_store = Arc::clone(&store);
    let reader = tokio::spawn(async move {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match reader_store.get("large/object.bin").await.unwrap() {
                None => {
                    // Not yet written — valid.
                    if attempts > 1000 {
                        break None;
                    }
                    tokio::task::yield_now().await;
                }
                Some(data) => {
                    break Some(data);
                }
            }
        }
    });

    writer.await.unwrap();
    let read_result = reader.await.unwrap();

    // If the reader saw data, it must be the complete object.
    if let Some(data) = read_result {
        assert_eq!(data.len(), 1024 * 1024, "no partial reads allowed");
        assert_eq!(data, large_data);
    }
}

// ── 4. Concurrent list_prefix during writes ──────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_list_prefix_during_writes_is_consistent() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));

    let writer_store = Arc::clone(&store);
    let writer = tokio::spawn(async move {
        for i in 0..20 {
            writer_store
                .put(
                    &format!("prefix/item-{i:03}.dat"),
                    Bytes::from(format!("data-{i}")),
                )
                .await
                .unwrap();
        }
    });

    let reader_store = Arc::clone(&store);
    let reader = tokio::spawn(async move {
        let mut observed_sizes = Vec::new();
        for _ in 0..50 {
            let keys = reader_store.list_prefix("prefix/").await.unwrap();
            // Filter to final keys (ignore temp files from atomic writes).
            let final_keys: Vec<_> = keys.iter().filter(|k| !k.contains(".tmp")).collect();
            observed_sizes.push(final_keys.len());
            for key in &final_keys {
                assert!(key.starts_with("prefix/item-"), "unexpected key: {key}");
            }
            tokio::task::yield_now().await;
        }
        observed_sizes
    });

    writer.await.unwrap();
    let sizes = reader.await.unwrap();

    // Sizes should be in [0, 20].
    for &size in &sizes {
        assert!(
            size <= 20,
            "list should never return more items than written"
        );
    }
}

// ── 5. Concurrent delete + read ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_delete_and_read_no_error() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));

    store
        .put("ephemeral/data.bin", Bytes::from("hello world"))
        .await
        .unwrap();

    let deleter_store = Arc::clone(&store);
    let deleter = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        deleter_store.delete("ephemeral/data.bin").await.unwrap();
    });

    let reader_store = Arc::clone(&store);
    let reader = tokio::spawn(async move {
        let mut results = Vec::new();
        for _ in 0..100 {
            let result = reader_store.get("ephemeral/data.bin").await.unwrap();
            results.push(result);
            tokio::task::yield_now().await;
        }
        results
    });

    deleter.await.unwrap();
    let results = reader.await.unwrap();

    for data in results.into_iter().flatten() {
        assert_eq!(data, Bytes::from("hello world"));
    }
}

// ── 6. Manifest write contention ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn manifest_write_contention_idempotent_or_error() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));
    let barrier = Arc::new(Barrier::new(10));

    let repo = repo();
    let gen_id = GenerationId::new();
    let manifest = GenerationManifest {
        repo: repo.clone(),
        generation: gen_id,
        bundle_key: format!("repos/{repo}/generations/{gen_id}/base.bundle"),
        parent_generation: None,
        created_at: Utc::now(),
        commits: vec![commit_sha('a')],
    };

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let s = Arc::clone(&store);
            let bar = Arc::clone(&barrier);
            let m = manifest.clone();
            tokio::spawn(async move {
                bar.wait().await;
                write_generation_manifest_if_absent_or_matches(&*s, &m).await
            })
        })
        .collect();

    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let first_writes: Vec<_> = results.iter().filter(|r| matches!(r, Ok(true))).collect();
    let idempotent: Vec<_> = results.iter().filter(|r| matches!(r, Ok(false))).collect();

    assert_eq!(first_writes.len(), 1, "exactly one first-write");
    assert_eq!(idempotent.len(), 9, "remaining should be idempotent");
}

// ── 7. Lease acquisition contention ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn lease_acquisition_contention_exactly_one_acquires() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(make_store(&tmp));
    let barrier = Arc::new(Barrier::new(20));

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let s = Arc::clone(&store);
            let bar = Arc::clone(&barrier);
            tokio::spawn(async move {
                bar.wait().await;
                acquire_lease(
                    &*s,
                    &repo(),
                    "update",
                    format!("holder-{i}"),
                    Utc::now(),
                    ChronoDuration::minutes(5),
                )
                .await
            })
        })
        .collect();

    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();

    let acquired: Vec<_> = results.iter().filter(|r| r.is_some()).collect();
    assert_eq!(
        acquired.len(),
        1,
        "exactly one task should acquire the lease"
    );

    let not_acquired: Vec<_> = results.iter().filter(|r| r.is_none()).collect();
    assert_eq!(not_acquired.len(), 19);
}
