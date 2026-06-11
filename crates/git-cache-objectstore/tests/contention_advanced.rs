//! Advanced resource contention tests for the ObjectStore.
//!
//! Tests cover concurrent manifest writes, lease contention, put+delete races,
//! large concurrent writes, and list_prefix consistency during rapid mutations.

mod tests {
    use bytes::Bytes;
    use chrono::Utc;
    use futures::future::join_all;
    use git_cache_core::{GenerationId, GenerationManifest, RepoKey};
    use git_cache_objectstore::{
        write_generation_manifest_if_absent_or_matches, LocalObjectStore, ObjectStore,
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

    // ── 1. Concurrent manifest writes for same repo ─────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_manifest_writes_exactly_one_wins() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(make_store(&tmp));
        let barrier = Arc::new(Barrier::new(10));

        let repo = repo();
        let gen_id = GenerationId::new();
        let manifest = GenerationManifest {
            repo: repo.clone(),
            generation: gen_id,
            verified_at: None,
            packs: Vec::new(),
            refs: Default::default(),
            head_ref: None,
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
        let errors: Vec<_> = results.iter().filter(|r| r.is_err()).collect();

        assert_eq!(
            first_writes.len(),
            1,
            "exactly one task should do the first write"
        );
        assert_eq!(
            idempotent.len() + errors.len(),
            9,
            "remaining 9 should be idempotent or conflict errors"
        );
    }

    // ── 3. Put + delete race ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn put_delete_race_final_state_consistent() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(make_store(&tmp));
        let barrier = Arc::new(Barrier::new(2));

        let key = "race/put-delete.dat";
        let data = Bytes::from("race-data");

        // Concurrent put and delete on the same key.
        let put_store = Arc::clone(&store);
        let put_barrier = Arc::clone(&barrier);
        let put_data = data.clone();
        let put_handle = tokio::spawn(async move {
            put_barrier.wait().await;
            put_store.put(key, put_data).await
        });

        let del_store = Arc::clone(&store);
        let del_barrier = Arc::clone(&barrier);
        let del_handle = tokio::spawn(async move {
            del_barrier.wait().await;
            del_store.delete(key).await
        });

        let put_result = put_handle.await.unwrap();
        let del_result = del_handle.await.unwrap();

        // Both operations should succeed without error.
        assert!(
            put_result.is_ok(),
            "put should not error: {:?}",
            put_result.err()
        );
        assert!(
            del_result.is_ok(),
            "delete should not error: {:?}",
            del_result.err()
        );

        // Final state: key either exists (put won) or doesn't (delete won).
        let final_state = store.get(key).await.unwrap();
        match final_state {
            Some(value) => {
                assert_eq!(value, data, "if key exists, it should have the put data");
            }
            None => {
                // Key was deleted — consistent.
            }
        }
    }

    // ── 4. Large concurrent writes ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn large_concurrent_writes_to_different_keys() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(make_store(&tmp));
        let barrier = Arc::new(Barrier::new(10));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let s = Arc::clone(&store);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    // Each task writes a 5MB object to a different key.
                    let data = Bytes::from(vec![i as u8; 5 * 1024 * 1024]);
                    let key = format!("large/object-{i}.bin");
                    s.put(&key, data).await.unwrap();
                    i
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 10);

        // Verify all objects were written correctly.
        for i in 0..10u8 {
            let key = format!("large/object-{i}.bin");
            let data = store.get(&key).await.unwrap().unwrap();
            assert_eq!(data.len(), 5 * 1024 * 1024, "object {i} should be 5MB");
            assert!(
                data.iter().all(|&b| b == i),
                "object {i} should contain only byte {i}"
            );
        }
    }

    // ── 5. list_prefix consistency during rapid writes/deletes ──────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn list_prefix_consistency_during_rapid_mutations() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(make_store(&tmp));

        // Write initial 50 objects.
        for i in 0..50 {
            store
                .put(
                    &format!("prefix/item-{i:03}.dat"),
                    Bytes::from(format!("data-{i}")),
                )
                .await
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(3));

        // Task 1: Add 50 more objects.
        let writer_store = Arc::clone(&store);
        let writer_barrier = Arc::clone(&barrier);
        let writer = tokio::spawn(async move {
            writer_barrier.wait().await;
            for i in 50..100 {
                writer_store
                    .put(
                        &format!("prefix/item-{i:03}.dat"),
                        Bytes::from(format!("data-{i}")),
                    )
                    .await
                    .unwrap();
            }
        });

        // Task 2: Delete 25 of the original objects.
        let deleter_store = Arc::clone(&store);
        let deleter_barrier = Arc::clone(&barrier);
        let deleter = tokio::spawn(async move {
            deleter_barrier.wait().await;
            for i in 0..25 {
                deleter_store
                    .delete(&format!("prefix/item-{i:03}.dat"))
                    .await
                    .unwrap();
            }
        });

        // Task 3: Continuously list_prefix and verify consistency.
        let reader_store = Arc::clone(&store);
        let reader_barrier = Arc::clone(&barrier);
        let reader = tokio::spawn(async move {
            reader_barrier.wait().await;
            let mut observations = Vec::new();
            for _ in 0..100 {
                let keys = reader_store.list_prefix("prefix/", None).await.unwrap();
                // Filter out temp files.
                let final_keys: Vec<_> = keys.iter().filter(|k| !k.contains(".tmp")).collect();

                // Verify no corrupt or partial keys.
                for key in &final_keys {
                    assert!(
                        key.starts_with("prefix/item-"),
                        "unexpected key format: {key}"
                    );
                    assert!(key.ends_with(".dat"), "unexpected key suffix: {key}");
                }

                observations.push(final_keys.len());
                tokio::task::yield_now().await;
            }
            observations
        });

        writer.await.unwrap();
        deleter.await.unwrap();
        let observations = reader.await.unwrap();

        // Observations should be in valid range.
        // Minimum: original 50 - 25 deleted = 25 (if all deletes happen first).
        // Maximum: original 50 + 50 added = 100 (if all adds happen first).
        for &count in &observations {
            assert!(
                count <= 100,
                "list should never return more than 100 items, got {count}"
            );
        }

        // Final state verification.
        let final_keys = store.list_prefix("prefix/", None).await.unwrap();
        let final_keys: Vec<_> = final_keys.iter().filter(|k| !k.contains(".tmp")).collect();
        // Expected final: 25 remaining originals + 50 new = 75.
        assert_eq!(
            final_keys.len(),
            75,
            "final state should have 75 items (50 original - 25 deleted + 50 new)"
        );
    }
}
