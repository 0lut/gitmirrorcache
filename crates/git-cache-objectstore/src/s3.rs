use crate::{validate_key, ObjectMeta, ObjectStore};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
#[cfg(feature = "s3")]
use aws_smithy_runtime_api::{client::orchestrator::HttpResponse, client::result::SdkError};
#[cfg(feature = "s3")]
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::Path;

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
            .body(ByteStream::new(value.into()))
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
            .body(ByteStream::new(value.into()))
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

    async fn delete(&self, key: &str) -> Result<()> {
        let s3_key = self.s3_key(key)?;
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
            .map_err(|err| s3_error("delete", &s3_key, err))?;
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str, max_keys: Option<usize>) -> Result<Vec<String>> {
        let s3_prefix = if self.prefix.is_empty() {
            prefix.to_string()
        } else {
            format!("{}/{}", self.prefix, prefix)
        };

        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&s3_prefix);

            if let Some(token) = continuation_token.take() {
                request = request.continuation_token(token);
            }

            let output = request
                .send()
                .await
                .map_err(|err| s3_error("list", &s3_prefix, err))?;

            if let Some(contents) = output.contents {
                for object in contents {
                    if let Some(full_key) = object.key {
                        let relative = if self.prefix.is_empty() {
                            full_key
                        } else {
                            full_key
                                .strip_prefix(&format!("{}/", self.prefix))
                                .unwrap_or(&full_key)
                                .to_string()
                        };
                        keys.push(relative);
                        if let Some(limit) = max_keys {
                            if keys.len() >= limit {
                                return Ok(keys);
                            }
                        }
                    }
                }
            }

            if output.is_truncated == Some(true) {
                continuation_token = output.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(keys)
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let s3_key = self.s3_key(key)?;
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(output) => {
                let len = output.content_length().unwrap_or(0) as u64;
                let updated_at = output
                    .last_modified()
                    .and_then(|t| DateTime::<Utc>::from_timestamp(t.secs(), t.subsec_nanos()));
                Ok(Some(ObjectMeta {
                    key: key.to_string(),
                    len,
                    updated_at,
                }))
            }
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(s3_error("head", &s3_key, err)),
        }
    }

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let s3_key = self.s3_key(key)?;
        let body = ByteStream::from_path(path)
            .await
            .map_err(|err| s3_error("put_file read", &s3_key, err))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(body)
            .send()
            .await
            .map_err(|err| s3_error("put_file", &s3_key, err))?;
        Ok(())
    }
}

fn normalize_prefix(prefix: String) -> Result<String> {
    let prefix = prefix.trim_matches('/').to_string();
    if !prefix.is_empty() {
        validate_key(&prefix)?;
    }
    Ok(prefix)
}

fn is_not_found<E>(error: &SdkError<E, HttpResponse>) -> bool
where
    E: ProvideErrorMetadata,
{
    matches!(
        error
            .raw_response()
            .map(|response| response.status().as_u16()),
        Some(404)
    ) || matches!(
        error.as_service_error().and_then(|err| err.code()),
        Some("NoSuchKey" | "NotFound")
    )
}

fn is_precondition_failed<E>(error: &SdkError<E, HttpResponse>) -> bool
where
    E: ProvideErrorMetadata,
{
    matches!(
        error
            .raw_response()
            .map(|response| response.status().as_u16()),
        Some(412)
    ) || matches!(
        error.as_service_error().and_then(|err| err.code()),
        Some("PreconditionFailed")
    )
}

fn s3_error(op: &'static str, key: &str, error: impl std::fmt::Debug) -> GitCacheError {
    GitCacheError::UpstreamUnavailable(format!("s3 {op} `{key}` failed: {error:?}"))
}

#[cfg(test)]
#[cfg(feature = "s3")]
mod tests {
    use super::{is_not_found, is_precondition_failed};
    use aws_sdk_s3::operation::{head_object::HeadObjectError, put_object::PutObjectError};
    use aws_smithy_runtime_api::{
        client::{orchestrator::HttpResponse, result::SdkError},
        http::StatusCode,
    };
    use aws_smithy_types::{body::SdkBody, error::metadata::ErrorMetadata};

    fn response(status: u16, body: &'static str) -> HttpResponse {
        HttpResponse::new(StatusCode::try_from(status).unwrap(), SdkBody::from(body))
    }

    #[test]
    fn non_404_error_with_404_in_key_is_not_not_found() {
        let error = SdkError::service_error(
            HeadObjectError::generic(
                ErrorMetadata::builder()
                    .code("AccessDenied")
                    .message("access denied for repos/repo404/base.bundle")
                    .build(),
            ),
            response(403, "<Key>repos/repo404/base.bundle</Key>"),
        );

        assert!(!is_not_found(&error));
    }

    #[test]
    fn non_412_error_with_412_in_message_is_not_precondition_failed() {
        let error = SdkError::service_error(
            PutObjectError::generic(
                ErrorMetadata::builder()
                    .code("AccessDenied")
                    .message("access denied for repos/repo412/base.bundle")
                    .build(),
            ),
            response(403, "<Key>repos/repo412/base.bundle</Key>"),
        );

        assert!(!is_precondition_failed(&error));
    }
}
