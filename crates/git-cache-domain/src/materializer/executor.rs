use super::*;

pub struct MaterializerExecutor {
    state: Arc<AppState>,
}

impl MaterializerExecutor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl UpdateExecutor for MaterializerExecutor {
    async fn update(&self, request: UpdateRequest) -> CoreResult<UpdateResult> {
        let materializer = match &request.lease_token {
            Some(token) => Materializer::with_lease_token(Arc::clone(&self.state), token.clone()),
            None => Materializer::new(Arc::clone(&self.state)),
        };
        let mut result = UpdateResult::default();
        match request.target {
            UpdateTarget::Branch(ref branch) => {
                materializer
                    .ensure_branch(&request.repo, branch, false)
                    .await?;
            }
            UpdateTarget::DefaultBranch => {
                materializer.ensure_default_branch(&request.repo).await?;
            }
            UpdateTarget::Commit(commit) => {
                materializer
                    .materialize_commit(request.repo, commit.clone())
                    .await?;
                result.resolved_commit = Some(commit);
            }
            UpdateTarget::ShortCommit(commit) => {
                let response = materializer
                    .materialize_short_commit(request.repo, commit)
                    .await?;
                result.resolved_commit = Some(response.commit);
            }
            UpdateTarget::Ref(ref ref_name) => {
                if let Some(branch_str) = ref_name.strip_prefix("refs/heads/") {
                    let branch = BranchName::parse(branch_str)?;
                    materializer
                        .ensure_branch(&request.repo, &branch, false)
                        .await?;
                } else {
                    return Err(GitCacheError::Unsupported(format!(
                        "unsupported update target ref: {ref_name}"
                    )));
                }
            }
        }
        Ok(result)
    }
}
