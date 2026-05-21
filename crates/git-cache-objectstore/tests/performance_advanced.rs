//! Advanced performance tests for the object store.

use bytes::Bytes;
use git_cache_objectstore::{LocalObjectStore, ObjectStore};
use std::sync::Arc;
use std::time::Instant;

fn temp_store(name: &str) -> LocalObjectStore {
    let dir = std::env::temp_dir().join(format!(
        "git-cache-objstore-perf-adv-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    LocalObjectStore::new(dir)
}

fn payload(size: usize) -> Bytes {
    Bytes::from(vec![0xABu8; size])
}

// ── 1. Small object throughput (1000 x 1KB) ─────────────────────────────

#[tokio::test]
async fn test_small_object_throughput() {
    let store = temp_store("small-obj");
    let count = 1000;
    let size = 1_024; // 1KB

    // Put
    let start = Instant::now();
    for i in 0..count {
        let key = format!("small/{i:04}.bin");
        store.put(&key, payload(size)).await.unwrap();
    }
    let put_elapsed = start.elapsed();

    // Get
    let start = Instant::now();
    for i in 0..count {
        let key = format!("small/{i:04}.bin");
        let data = store.get(&key).await.unwrap().expect("object should exist");
        assert_eq!(data.len(), size);
    }
    let get_elapsed = start.elapsed();

    let total = put_elapsed + get_elapsed;
    let put_ops = count as f64 / put_elapsed.as_secs_f64();
    let get_ops = count as f64 / get_elapsed.as_secs_f64();
    eprintln!(
        "small object throughput ({count} x 1KB):\n  put: {put_elapsed:?} ({put_ops:.0} ops/sec)\n  get: {get_elapsed:?} ({get_ops:.0} ops/sec)\n  total: {total:?}"
    );
    assert!(
        total.as_secs() < 120,
        "small object throughput too slow: {total:?}"
    );
}

// ── 2. Mixed size throughput ─────────────────────────────────────────────

#[tokio::test]
async fn test_mixed_size_throughput() {
    let store = temp_store("mixed-size");
    let sizes: &[(usize, &str)] = &[
        (1_024, "1KB"),
        (100 * 1_024, "100KB"),
        (1_024 * 1_024, "1MB"),
    ];
    let per_size = 50;

    // Interleaved put.
    let start = Instant::now();
    for round in 0..per_size {
        for (idx, &(size, _)) in sizes.iter().enumerate() {
            let key = format!("mixed/{idx}/{round:04}.bin");
            store.put(&key, payload(size)).await.unwrap();
        }
    }
    let put_elapsed = start.elapsed();

    // Interleaved get.
    let start = Instant::now();
    for round in 0..per_size {
        for (idx, &(size, _)) in sizes.iter().enumerate() {
            let key = format!("mixed/{idx}/{round:04}.bin");
            let data = store.get(&key).await.unwrap().expect("object should exist");
            assert_eq!(data.len(), size);
        }
    }
    let get_elapsed = start.elapsed();

    let total_objects = per_size * sizes.len();
    let total = put_elapsed + get_elapsed;
    eprintln!(
        "mixed size throughput ({total_objects} objects, 1KB/100KB/1MB interleaved):\n  put: {put_elapsed:?}\n  get: {get_elapsed:?}\n  total: {total:?}"
    );
    assert!(
        total.as_secs() < 120,
        "mixed size throughput too slow: {total:?}"
    );
}

// ── 3. Concurrent mixed read/write ──────────────────────────────────────

#[tokio::test]
async fn test_concurrent_mixed_read_write() {
    let store = Arc::new(temp_store("concurrent-rw"));
    let object_size = 4_096;

    // Pre-populate objects for readers.
    let reader_objects = 100;
    for i in 0..reader_objects {
        let key = format!("pre/{i:04}.bin");
        store.put(&key, payload(object_size)).await.unwrap();
    }

    let writers = 10;
    let readers = 10;
    let objects_per_task = 50;

    let start = Instant::now();
    let mut handles = Vec::new();

    // Writers.
    for t in 0..writers {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..objects_per_task {
                let key = format!("write/{t}/{i:04}.bin");
                store.put(&key, payload(object_size)).await.unwrap();
            }
            ("writer", t)
        }));
    }

    // Readers.
    for t in 0..readers {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..objects_per_task {
                let key = format!("pre/{:04}.bin", i % reader_objects);
                let data = store
                    .get(&key)
                    .await
                    .unwrap()
                    .expect("pre-populated object");
                assert_eq!(data.len(), object_size);
            }
            ("reader", t)
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
    let elapsed = start.elapsed();

    let total_ops = (writers + readers) * objects_per_task;
    let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();
    eprintln!(
        "concurrent mixed r/w ({writers} writers + {readers} readers, {objects_per_task} ops each):\n  {elapsed:?}, {total_ops} ops ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 120,
        "concurrent mixed r/w too slow: {elapsed:?}"
    );
}

// ── 4. list_prefix scaling (500 objects) ─────────────────────────────────

#[tokio::test]
async fn test_list_prefix_scaling() {
    let store = temp_store("list-prefix-scale");
    let count = 500;

    for i in 0..count {
        let key = format!("prefix-scale/obj-{i:04}.bin");
        store.put(&key, payload(64)).await.unwrap();
    }

    let iterations = 10;
    let start = Instant::now();
    for _ in 0..iterations {
        let keys = store.list_prefix("prefix-scale/").await.unwrap();
        assert_eq!(
            keys.len(),
            count,
            "expected {count} keys, got {}",
            keys.len()
        );
    }
    let elapsed = start.elapsed();

    let avg = elapsed / iterations as u32;
    eprintln!(
        "list_prefix scaling ({count} objects): {iterations} calls in {elapsed:?}, avg={avg:?}"
    );
    assert!(
        elapsed.as_secs() < 120,
        "list_prefix scaling too slow: {elapsed:?}"
    );
}

// ── 5. Delete throughput (200 objects) ───────────────────────────────────

#[tokio::test]
async fn test_delete_throughput() {
    let store = temp_store("delete-throughput");
    let count = 200;

    // Create objects.
    let create_start = Instant::now();
    for i in 0..count {
        let key = format!("delete/{i:04}.bin");
        store.put(&key, payload(1_024)).await.unwrap();
    }
    let create_elapsed = create_start.elapsed();

    // Verify all exist.
    for i in 0..count {
        let key = format!("delete/{i:04}.bin");
        assert!(
            store.exists(&key).await.unwrap(),
            "object {key} should exist before delete"
        );
    }

    // Delete all.
    let delete_start = Instant::now();
    for i in 0..count {
        let key = format!("delete/{i:04}.bin");
        store.delete(&key).await.unwrap();
    }
    let delete_elapsed = delete_start.elapsed();

    // Verify all gone.
    for i in 0..count {
        let key = format!("delete/{i:04}.bin");
        let result = store.get(&key).await.unwrap();
        assert!(result.is_none(), "object {key} should be deleted");
    }

    let create_ops = count as f64 / create_elapsed.as_secs_f64();
    let delete_ops = count as f64 / delete_elapsed.as_secs_f64();
    eprintln!(
        "delete throughput ({count} objects):\n  create: {create_elapsed:?} ({create_ops:.0} ops/sec)\n  delete: {delete_elapsed:?} ({delete_ops:.0} ops/sec)"
    );
    assert!(
        delete_elapsed.as_secs() < 120,
        "delete throughput too slow: {delete_elapsed:?}"
    );
}
