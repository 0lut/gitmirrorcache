//! Performance / throughput tests for the object store.

mod tests {
    use bytes::Bytes;
    use git_cache_objectstore::{LocalObjectStore, ObjectStore};
    use std::sync::Arc;
    use std::time::Instant;

    fn temp_store(name: &str) -> LocalObjectStore {
        let dir = std::env::temp_dir().join(format!(
            "git-cache-objstore-perf-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        LocalObjectStore::new(dir)
    }

    fn payload(size: usize) -> Bytes {
        Bytes::from(vec![0xABu8; size])
    }

    // ── 1. Sequential put/get throughput ─────────────────────────────────────

    #[tokio::test]
    async fn test_sequential_put_get_throughput() {
        let store = temp_store("seq-put-get");
        let sizes: &[usize] = &[1_024, 10_240, 102_400, 1_048_576]; // 1KB, 10KB, 100KB, 1MB
        let per_size = 25; // 25 * 4 sizes = 100 objects

        let start = Instant::now();
        for (si, &size) in sizes.iter().enumerate() {
            for i in 0..per_size {
                let key = format!("seq/{si}/{i}.bin");
                store.put(&key, payload(size)).await.unwrap();
            }
        }
        let put_elapsed = start.elapsed();

        let start = Instant::now();
        for (si, &size) in sizes.iter().enumerate() {
            for i in 0..per_size {
                let key = format!("seq/{si}/{i}.bin");
                let data = store.get(&key).await.unwrap().expect("object should exist");
                assert_eq!(data.len(), size);
            }
        }
        let get_elapsed = start.elapsed();

        let total = put_elapsed + get_elapsed;
        eprintln!(
            "sequential put/get: put={:?}, get={:?}, total={:?} for 100 objects",
            put_elapsed, get_elapsed, total
        );
        assert!(
            total.as_secs() < 30,
            "sequential put/get took too long: {total:?}"
        );
    }

    // ── 2. Concurrent put throughput ─────────────────────────────────────────

    #[tokio::test]
    async fn test_concurrent_put_throughput() {
        let store = Arc::new(temp_store("concurrent-put"));
        let tasks_count = 20;
        let objects_per_task = 50;
        let object_size = 4_096;

        let start = Instant::now();
        let mut handles = Vec::new();
        for t in 0..tasks_count {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                for i in 0..objects_per_task {
                    let key = format!("conc/{t}/{i}.bin");
                    store.put(&key, payload(object_size)).await.unwrap();
                }
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }
        let elapsed = start.elapsed();

        // Verify all objects and check integrity.
        for t in 0..tasks_count {
            for i in 0..objects_per_task {
                let key = format!("conc/{t}/{i}.bin");
                let data = store.get(&key).await.unwrap().expect("object should exist");
                assert_eq!(data.len(), object_size);
                assert!(data.iter().all(|&b| b == 0xAB));
            }
        }

        let total_objects = tasks_count * objects_per_task;
        let ops_per_sec = total_objects as f64 / elapsed.as_secs_f64();
        eprintln!(
            "concurrent put: {total_objects} objects in {:?} ({ops_per_sec:.0} ops/sec)",
            elapsed
        );
        assert!(
            elapsed.as_secs() < 30,
            "concurrent put took too long: {elapsed:?}"
        );
    }

    // ── 3. Large object handling ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_large_object_10mb() {
        let store = temp_store("large-10mb");
        let size = 10 * 1024 * 1024;
        let data = payload(size);

        let start = Instant::now();
        store.put("large/10mb.bin", data.clone()).await.unwrap();
        let put_elapsed = start.elapsed();

        let start = Instant::now();
        let read_back = store
            .get("large/10mb.bin")
            .await
            .unwrap()
            .expect("large object should exist");
        let get_elapsed = start.elapsed();

        assert_eq!(read_back.len(), size);
        assert_eq!(read_back, data);

        eprintln!("large object 10MB: put={put_elapsed:?}, get={get_elapsed:?}");
        assert!(
            (put_elapsed + get_elapsed).as_secs() < 30,
            "10MB object handling too slow"
        );
    }

    #[tokio::test]
    async fn test_large_object_50mb() {
        let store = temp_store("large-50mb");
        let size = 50 * 1024 * 1024;
        let data = payload(size);

        let start = Instant::now();
        store.put("large/50mb.bin", data.clone()).await.unwrap();
        let put_elapsed = start.elapsed();

        let start = Instant::now();
        let read_back = store
            .get("large/50mb.bin")
            .await
            .unwrap()
            .expect("large object should exist");
        let get_elapsed = start.elapsed();

        assert_eq!(read_back.len(), size);
        assert_eq!(read_back, data);

        eprintln!("large object 50MB: put={put_elapsed:?}, get={get_elapsed:?}");
        assert!(
            (put_elapsed + get_elapsed).as_secs() < 60,
            "50MB object handling too slow"
        );
    }

    // ── 4. list_prefix at scale ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_prefix_at_scale() {
        let store = temp_store("list-prefix");
        let count = 500;

        for i in 0..count {
            let key = format!("prefix-test/obj-{i:04}.bin");
            store.put(&key, payload(64)).await.unwrap();
        }

        let start = Instant::now();
        let keys = store.list_prefix("prefix-test/", None).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(
            keys.len(),
            count,
            "expected {count} keys, got {}",
            keys.len()
        );

        eprintln!("list_prefix 500 objects: {elapsed:?}");
        assert!(elapsed.as_secs() < 10, "list_prefix too slow: {elapsed:?}");
    }

    // ── 5. put_if_absent under contention ────────────────────────────────────

    #[tokio::test]
    async fn test_put_if_absent_under_contention() {
        let store = Arc::new(temp_store("put-if-absent"));
        let contenders = 10;
        let key = "contested/single-key.bin";
        let value = payload(256);

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..contenders {
            let store = Arc::clone(&store);
            let value = value.clone();
            handles.push(tokio::spawn(async move {
                store.put_if_absent(key, value).await.unwrap()
            }));
        }

        let mut won_count = 0;
        for handle in handles {
            if handle.await.unwrap() {
                won_count += 1;
            }
        }
        let elapsed = start.elapsed();

        assert_eq!(
            won_count, 1,
            "exactly one writer should win, got {won_count}"
        );

        // Verify the object is readable and has the right content.
        let stored = store.get(key).await.unwrap().expect("object should exist");
        assert_eq!(stored.len(), 256);
        assert!(stored.iter().all(|&b| b == 0xAB));

        eprintln!("put_if_absent contention ({contenders} tasks): {elapsed:?}");
    }
}
