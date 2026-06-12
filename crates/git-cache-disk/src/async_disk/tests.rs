use super::*;
use crate::DiskManager;

fn test_disk_manager() -> DiskManager {
    let root = std::env::temp_dir().join(format!(
        "git-cache-async-disk-test-{}",
        uuid::Uuid::now_v7()
    ));
    DiskManager::new(root, 1024 * 1024 * 1024, 0)
}

#[tokio::test]
async fn release_cleans_up_temp_dirs_and_markers() {
    let dm = test_disk_manager();
    let async_dm = AsyncDiskManager::new(dm);
    let reservation = async_dm.reserve(1024).await.unwrap();
    let temp_path = reservation.temp_path().unwrap();

    // temp dir should exist after reservation
    assert!(temp_path.exists());

    reservation.release().await.unwrap();

    // temp dir should be gone after release
    assert!(!temp_path.exists());
}

#[tokio::test]
async fn drop_without_release_still_cleans_up() {
    let dm = test_disk_manager();
    let async_dm = AsyncDiskManager::new(dm);
    let reservation = async_dm.reserve(1024).await.unwrap();
    let temp_path = reservation.temp_path().unwrap();

    assert!(temp_path.exists());

    // Drop without calling release
    drop(reservation);

    // Sync drop should still clean up
    assert!(!temp_path.exists());
}

#[tokio::test]
async fn invalidate_repo_delegates_to_inner_manager() {
    let dm = test_disk_manager();
    let repo_dir = dm.repos_dir().join("repo.git");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("pack"), vec![0u8; 8]).unwrap();
    dm.record_repo_access("repo.git").unwrap();
    let async_dm = AsyncDiskManager::new(dm);

    async_dm
        .invalidate_repo(PathBuf::from("repo.git"))
        .await
        .unwrap();

    let index = async_dm.repo_index().await.unwrap();
    assert!(!index.repos.contains_key(std::path::Path::new("repo.git")));
}
