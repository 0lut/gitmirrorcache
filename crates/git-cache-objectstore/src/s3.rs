use crate::{validate_key, ObjectStore};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use git_cache_core::{GitCacheError, Result};

#[derive(Debug, Clone)]
pub struct S3ObjectStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3ObjectStore {
    pub fn new(
        client: Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Result<Self> {
        let bucket = bucket.into();
        if bucket.trim().is_empty() {
            return Err(GitCacheError::Validation(
                "s3 bucket name must not be empty".into(),
            ));
        }

        let prefix = normalize_prefix(prefix.into())?;
        Ok(Self {
            client,
            bucket,
            prefix,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn s3_key(&self, key: &str) -> Result<String> {
        validate_key(key)?;
        if self.prefix.is_empty() {
            Ok(key.to_string())
        } else {
            Ok(format!("{}/{}", self.prefix, key))
        }
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let s3_key = self.s3_key(key)?;
        let output = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(s3_error("get", &s3_key, err)),
        };

        let body = output
            .body
            .collect()
            .await
            .map_err(|err| s3_error("read body", &s3_key, err))?;
        Ok(Some(body.into_bytes()))
    }

    async fn put(&self, key: &str, value: Bytes) -> Result<()> {
        let s3_key = self.s3_key(key)?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(ByteStream::from(value.to_vec()))
            .send()
            .await
            .map_err(|err| s3_error("put", &s3_key, err))?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, value: Bytes) -> Result<bool> {
        let s3_key = self.s3_key(key)?;
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .if_none_match("*")
            .body(ByteStream::from(value.to_vec()))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_precondition_failed(&err) => Ok(false),
            Err(err) => Err(s3_error("put_if_absent", &s3_key, err)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let s3_key = self.s3_key(key)?;
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_not_found(&err) => Ok(false),
            Err(err) => Err(s3_error("head", &s3_key, err)),
        }
    }
}

fn normalize_prefix(prefix: String) -> Result<String> {
    let prefix = prefix.trim_matches('/').to_string();
    if !prefix.is_empty() {
        validate_key(&prefix)?;
    }
    Ok(prefix)
}

fn is_not_found(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string();
    text.contains("NoSuchKey")
        || text.contains("NotFound")
        || text.contains("404")
        || text.contains("not found")
}

fn is_precondition_failed(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string();
    text.contains("PreconditionFailed")
        || text.contains("Precondition Failed")
        || text.contains("412")
}

fn s3_error(op: &'static str, key: &str, error: impl std::fmt::Display) -> GitCacheError {
    GitCacheError::UpstreamUnavailable(format!("s3 {op} `{key}` failed: {error}"))
}
