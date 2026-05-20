use crate::{
    acquire_lease, commit_manifest_key, generation_manifest_key, lease_key, read_commit_manifest,
    read_generation_manifest, read_lease, read_ref_manifest, read_session_manifest,
    ref_manifest_key, session_manifest_key, validate_key, write_generation_manifest,
    write_generation_manifest_if_absent_or_matches, write_ref_manifest_if_absent_or_matches,
    GenerationPublish, LeaseManifest, LocalObjectStore, ObjectStore, PublishManifests,
};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use git_cache_core::{
    CommitManifest, CommitSha, GenerationId, GenerationManifest, RefManifest, RepoKey, SessionId,
    SessionManifest,
};
use tokio::fs;

fn temp_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "git-cache-objectstore-test-{}",
        uuid::Uuid::now_v7()
    ))
}

fn repo() -> RepoKey {
    RepoKey::parse("github.com/org/repo").unwrap()
}

fn commit(byte: char) -> CommitSha {
    CommitSha::parse(byte.to_string().repeat(40)).unwrap()
}

fn ts(seconds: u32) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("2026-05-18T00:00:{seconds:02}Z"))
        .unwrap()
        .with_timezone(&Utc)
}

fn test_generation_id() -> GenerationId {
    GenerationId::new()
}

fn generation_manifest(repo: &RepoKey) -> GenerationManifest {
    let gen = test_generation_id();
    GenerationManifest {
        repo: repo.clone(),
        generation: gen,
        bundle_key: format!("repos/{repo}/generations/{gen}/base.bundle"),
        parent_generation: None,
        created_at: ts(1),
        commits: vec![commit('a')],
    }
}

fn commit_manifest_with_gen(repo: &RepoKey, gen: GenerationId) -> CommitManifest {
    CommitManifest {
        repo: repo.clone(),
        commit: commit('a'),
        generation: gen,
        complete: true,
        verified_at: ts(2),
    }
}

fn ref_manifest_with_gen(repo: &RepoKey, gen: GenerationId) -> RefManifest {
    RefManifest {
        repo: repo.clone(),
        ref_name: "refs/heads/feature/cache".into(),
        commit: commit('a'),
        generation: gen,
        verified_at: ts(3),
    }
}

fn session_manifest(repo: &RepoKey) -> SessionManifest {
    SessionManifest {
        id: SessionId::new(),
        repo: repo.clone(),
        commit: commit('a'),
        synthetic_ref: "refs/cache/sessions/test".into(),
        created_at: ts(4),
        expires_at: ts(5),
    }
}

#[tokio::test]
async fn put_if_absent_is_conditional() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    assert!(store
        .put_if_absent("leases/update.json", Bytes::from("one"))
        .await
        .unwrap());
    assert!(!store
        .put_if_absent("leases/update.json", Bytes::from("two"))
        .await
        .unwrap());
    assert_eq!(
        store.get("leases/update.json").await.unwrap().unwrap(),
        Bytes::from("one")
    );

    let repo = repo();
    let generation = generation_manifest(&repo);
    assert!(
        write_generation_manifest_if_absent_or_matches(&store, &generation)
            .await
            .unwrap()
    );
    assert!(
        !write_generation_manifest_if_absent_or_matches(&store, &generation)
            .await
            .unwrap()
    );

    let mut conflicting = generation.clone();
    conflicting.bundle_key = format!("repos/{repo}/generations/{}/other.bundle", generation.generation);
    assert!(
        write_generation_manifest_if_absent_or_matches(&store, &conflicting)
            .await
            .is_err()
    );

    let _ = fs::remove_dir_all(root).await;
}

#[tokio::test]
async fn manifests_round_trip_as_json() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let generation = generation_manifest(&repo);
    let commit = commit_manifest_with_gen(&repo, generation.generation);
    let reference = ref_manifest_with_gen(&repo, generation.generation);
    let session = session_manifest(&repo);

    write_generation_manifest(&store, &generation)
        .await
        .unwrap();
    crate::write_commit_manifest(&store, &commit).await.unwrap();
    crate::write_ref_manifest(&store, &reference).await.unwrap();
    crate::write_session_manifest(&store, &session)
        .await
        .unwrap();

    assert_eq!(
        read_generation_manifest(&store, &repo, generation.generation)
            .await
            .unwrap(),
        Some(generation)
    );
    assert_eq!(
        read_commit_manifest(&store, &repo, &commit.commit)
            .await
            .unwrap(),
        Some(commit)
    );
    assert_eq!(
        read_ref_manifest(&store, &repo, &reference.ref_name)
            .await
            .unwrap(),
        Some(reference)
    );
    assert_eq!(
        read_session_manifest(&store, &repo, session.id)
            .await
            .unwrap(),
        Some(session)
    );

    let _ = fs::remove_dir_all(root).await;
}

#[tokio::test]
async fn lease_acquisition_uses_create_once_semantics() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let acquired = acquire_lease(
        &store,
        &repo,
        "update",
        "worker-a",
        ts(10),
        Duration::minutes(15),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        acquired,
        LeaseManifest {
            repo: repo.clone(),
            name: "update".into(),
            holder: "worker-a".into(),
            acquired_at: ts(10),
            expires_at: ts(10) + Duration::minutes(15),
        }
    );

    assert!(acquire_lease(
        &store,
        &repo,
        "update",
        "worker-b",
        ts(11),
        Duration::minutes(15),
    )
    .await
    .unwrap()
    .is_none());

    assert_eq!(
        read_lease(&store, &repo, "update").await.unwrap(),
        Some(acquired)
    );

    let _ = fs::remove_dir_all(root).await;
}

#[tokio::test]
async fn publish_writes_bundle_generation_then_manifests() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let generation = generation_manifest(&repo);
    let commit_manifest = commit_manifest_with_gen(&repo, generation.generation);
    let reference = ref_manifest_with_gen(&repo, generation.generation);
    let session = session_manifest(&repo);
    let publish = GenerationPublish::with_manifests(
        generation.clone(),
        PublishManifests {
            commits: vec![commit_manifest.clone()],
            refs: vec![reference.clone()],
            sessions: vec![session.clone()],
        },
    );

    assert!(publish
        .publish_bundle_file(&store, root.join("missing.bundle"))
        .await
        .is_err());
    assert!(!store
        .exists(&generation_manifest_key(&repo, generation.generation))
        .await
        .unwrap());

    publish
        .publish_bundle_bytes(&store, Bytes::from_static(b"bundle-bytes"))
        .await
        .unwrap();
    assert_eq!(
        store.get(&generation.bundle_key).await.unwrap().unwrap(),
        Bytes::from_static(b"bundle-bytes")
    );
    assert_eq!(
        read_generation_manifest(&store, &repo, generation.generation)
            .await
            .unwrap(),
        Some(generation.clone())
    );
    assert_eq!(
        read_commit_manifest(&store, &repo, &commit_manifest.commit)
            .await
            .unwrap(),
        Some(commit_manifest.clone())
    );
    assert_eq!(
        read_ref_manifest(&store, &repo, &reference.ref_name)
            .await
            .unwrap(),
        Some(reference.clone())
    );
    assert_eq!(
        read_session_manifest(&store, &repo, session.id)
            .await
            .unwrap(),
        Some(session.clone())
    );

    publish
        .publish_bundle_bytes(&store, Bytes::from_static(b"bundle-bytes"))
        .await
        .unwrap();

    let mut changed_ref = reference;
    changed_ref.commit = commit('b');
    assert!(
        write_ref_manifest_if_absent_or_matches(&store, &changed_ref)
            .await
            .is_err()
    );

    let _ = fs::remove_dir_all(root).await;
}

#[tokio::test]
async fn rejects_traversal_keys() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    for key in [
        "../secret",
        "/secret",
        "repo\\secret",
        "repo/./secret",
        "repo/../secret",
        "repo/",
    ] {
        assert!(validate_key(key).is_err(), "{key} should be rejected");
        assert!(store.put(key, Bytes::from_static(b"bad")).await.is_err());
    }

    assert!(ref_manifest_key(&repo, "refs/heads/../main").is_err());
    assert!(ref_manifest_key(&repo, "/refs/heads/main").is_err());
    assert!(lease_key(&repo, "../update").is_err());

    assert_eq!(
        ref_manifest_key(&repo, "refs/heads/feature/cache").unwrap(),
        "repos/github.com/org/repo/manifests/refs/heads/feature%2Fcache.json"
    );
    assert_eq!(
        commit_manifest_key(&repo, &commit('a')),
        format!(
            "repos/github.com/org/repo/manifests/commits/aa/{}.json",
            "a".repeat(40)
        )
    );
    let session = session_manifest(&repo);
    assert_eq!(
        session_manifest_key(&repo, session.id),
        format!(
            "repos/github.com/org/repo/manifests/sessions/{}.json",
            session.id
        )
    );

    let _ = fs::remove_dir_all(root).await;
}
