use crate::{validate_key, ObjectMeta, ObjectStore, ObjectVersion};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
#[cfg(feature = "s3")]
use aws_smithy_runtime_api::{client::orchestrator::HttpResponse, client::result::SdkError};
use aws_smithy_types::byte_stream::Length;
#[cfg(feature = "s3")]
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::Path;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const S3_SINGLE_PUT_LIMIT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const S3_MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;
const S3_MAX_MULTIPART_PARTS: u64 = 10_000;
const S3_MIN_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024;
const S3_DEFAULT_MULTIPART_PART_BYTES: u64 = 64 * 1024 * 1024;
const S3_FILE_UPLOAD_BUFFER_BYTES: usize = 1024 * 1024;

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

    /// Startup probe: confirms the configured bucket exists and is reachable
    /// with the current credentials, so misconfiguration fails fast with a
    /// clear error instead of surfacing as `NoSuchBucket` on the first cache
    /// operation.
    pub async fn verify_bucket_access(&self) -> Result<()> {
        match self.client.head_bucket().bucket(&self.bucket).send().await {
            Ok(_) => Ok(()),
            Err(err) if is_not_found(&err) => Err(GitCacheError::Validation(format!(
                "s3 bucket `{}` does not exist; create it (the server never creates buckets) or fix GIT_CACHE_S3_BUCKET",
                self.bucket
            ))),
            Err(err) => Err(GitCacheError::UpstreamUnavailable(format!(
                "s3 bucket `{}` is not accessible with the current credentials/endpoint: {err:?}",
                self.bucket
            ))),
        }
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

    async fn put_file_multipart(&self, s3_key: &str, path: &Path, file_len: u64) -> Result<()> {
        if file_len > S3_MAX_OBJECT_BYTES {
            return Err(GitCacheError::Validation(format!(
                "s3 object `{s3_key}` is too large for multipart upload: {file_len} bytes"
            )));
        }

        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(s3_key)
            .send()
            .await
            .map_err(|err| s3_error("create_multipart_upload", s3_key, err))?;
        let upload_id = create
            .upload_id()
            .ok_or_else(|| {
                GitCacheError::UpstreamUnavailable(format!(
                    "s3 create_multipart_upload `{s3_key}` returned no upload id"
                ))
            })?
            .to_string();

        let upload_result = self
            .put_file_multipart_inner(s3_key, path, file_len, &upload_id)
            .await;

        if let Err(err) = upload_result {
            return match self.abort_multipart_upload(s3_key, &upload_id).await {
                Ok(()) => Err(err),
                Err(abort_err) => Err(GitCacheError::UpstreamUnavailable(format!(
                    "{err}; additionally failed to abort s3 multipart upload `{s3_key}`: {abort_err}"
                ))),
            };
        }

        Ok(())
    }

    async fn put_file_multipart_inner(
        &self,
        s3_key: &str,
        path: &Path,
        file_len: u64,
        upload_id: &str,
    ) -> Result<()> {
        let part_size = multipart_part_size(file_len)?;
        let mut parts = Vec::new();
        let mut offset = 0;
        let mut part_number = 1;

        while offset < file_len {
            let part_len = part_size.min(file_len - offset);
            let body = ByteStream::read_from()
                .path(path)
                .offset(offset)
                .length(Length::Exact(part_len))
                .buffer_size(S3_FILE_UPLOAD_BUFFER_BYTES)
                .build()
                .await
                .map_err(|err| s3_error("multipart put_file read", s3_key, err))?;
            let output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(s3_key)
                .upload_id(upload_id)
                .part_number(part_number)
                .content_length(part_len as i64)
                .body(body)
                .send()
                .await
                .map_err(|err| s3_error("upload_part", s3_key, err))?;
            let e_tag = output
                .e_tag()
                .ok_or_else(|| {
                    GitCacheError::UpstreamUnavailable(format!(
                        "s3 upload_part `{s3_key}` part {part_number} returned no etag"
                    ))
                })?
                .to_string();
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );
            offset += part_len;
            part_number += 1;
        }

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(s3_key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|err| s3_error("complete_multipart_upload", s3_key, err))?;
        Ok(())
    }

    async fn abort_multipart_upload(&self, s3_key: &str, upload_id: &str) -> Result<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(s3_key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|err| s3_error("abort_multipart_upload", s3_key, err))?;
        Ok(())
    }

    async fn put_stream_multipart_inner(
        &self,
        s3_key: &str,
        reader: &mut (dyn AsyncRead + Send + Unpin),
        part_size: usize,
        upload_id: &str,
    ) -> Result<()> {
        let mut parts = Vec::new();
        let mut part_number: i32 = 1;

        loop {
            let mut buf = vec![0u8; part_size];
            let mut filled = 0;
            // Fill the buffer up to part_size before uploading.
            while filled < part_size {
                match reader.read(&mut buf[filled..]).await? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            buf.truncate(filled);

            let body = ByteStream::new(Bytes::from(buf).into());
            let output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(s3_key)
                .upload_id(upload_id)
                .part_number(part_number)
                .content_length(filled as i64)
                .body(body)
                .send()
                .await
                .map_err(|err| s3_error("upload_part", s3_key, err))?;
            let e_tag = output
                .e_tag()
                .ok_or_else(|| {
                    GitCacheError::UpstreamUnavailable(format!(
                        "s3 upload_part `{s3_key}` part {part_number} returned no etag"
                    ))
                })?
                .to_string();
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );
            part_number += 1;

            if filled < part_size {
                // Last chunk was smaller than part_size → EOF.
                break;
            }
        }

        if parts.is_empty() {
            // Empty stream — upload a zero-length object instead.
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(s3_key)
                .body(ByteStream::new(Bytes::new().into()))
                .send()
                .await
                .map_err(|err| s3_error("put_stream empty", s3_key, err))?;
            // Abort the unused multipart upload.
            let _ = self.abort_multipart_upload(s3_key, upload_id).await;
            return Ok(());
        }

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(s3_key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|err| s3_error("complete_multipart_upload", s3_key, err))?;
        Ok(())
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

    async fn get_versioned(&self, key: &str) -> Result<Option<(Bytes, ObjectVersion)>> {
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
            Err(err) => return Err(s3_error("get_versioned", &s3_key, err)),
        };

        let e_tag = output
            .e_tag()
            .ok_or_else(|| {
                GitCacheError::UpstreamUnavailable(format!("s3 get `{s3_key}` returned no etag"))
            })?
            .to_string();
        let body = output
            .body
            .collect()
            .await
            .map_err(|err| s3_error("read body", &s3_key, err))?;
        Ok(Some((body.into_bytes(), ObjectVersion::new(e_tag))))
    }

    async fn put_if_version_matches(
        &self,
        key: &str,
        value: Bytes,
        version: &ObjectVersion,
    ) -> Result<bool> {
        let s3_key = self.s3_key(key)?;
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .if_match(version.token())
            .body(ByteStream::new(value.into()))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_precondition_failed(&err) || is_not_found(&err) => Ok(false),
            Err(err) => Err(s3_error("put_if_version_matches", &s3_key, err)),
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

    async fn get_file(&self, key: &str, path: &Path) -> Result<bool> {
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
            Err(err) if is_not_found(&err) => return Ok(false),
            Err(err) => return Err(s3_error("get_file", &s3_key, err)),
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut body = output.body.into_async_read();
        let mut file = tokio::fs::File::create(path).await?;
        tokio::io::copy(&mut body, &mut file)
            .await
            .map_err(|err| s3_error("get_file body", &s3_key, err))?;
        file.flush().await?;
        Ok(true)
    }

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let s3_key = self.s3_key(key)?;
        let file_len = tokio::fs::metadata(path).await?.len();
        if file_len > S3_SINGLE_PUT_LIMIT_BYTES {
            return self.put_file_multipart(&s3_key, path, file_len).await;
        }

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

    async fn get_stream(&self, key: &str) -> Result<Option<(Pin<Box<dyn AsyncRead + Send>>, u64)>> {
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
            Err(err) => return Err(s3_error("get_stream", &s3_key, err)),
        };

        let len = output.content_length().unwrap_or(0) as u64;
        let reader = output.body.into_async_read();
        Ok(Some((Box::pin(reader), len)))
    }

    async fn put_stream(
        &self,
        key: &str,
        reader: Pin<Box<dyn AsyncRead + Send>>,
        content_length: u64,
    ) -> Result<()> {
        let s3_key = self.s3_key(key)?;
        let mut reader = reader;

        // Small objects: single PutObject with in-memory buffer.
        if content_length <= S3_MIN_MULTIPART_PART_BYTES {
            let mut buf = Vec::with_capacity(content_length as usize);
            reader.read_to_end(&mut buf).await?;
            let body = ByteStream::new(buf.into());
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&s3_key)
                .body(body)
                .send()
                .await
                .map_err(|err| s3_error("put_stream", &s3_key, err))?;
            return Ok(());
        }

        // Large objects: multipart upload streaming from reader.
        let part_size = multipart_part_size(content_length)? as usize;
        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
            .map_err(|err| s3_error("create_multipart_upload", &s3_key, err))?;
        let upload_id = create
            .upload_id()
            .ok_or_else(|| {
                GitCacheError::UpstreamUnavailable(format!(
                    "s3 create_multipart_upload `{s3_key}` returned no upload id"
                ))
            })?
            .to_string();

        let result = self
            .put_stream_multipart_inner(&s3_key, &mut reader, part_size, &upload_id)
            .await;

        if let Err(err) = result {
            return match self.abort_multipart_upload(&s3_key, &upload_id).await {
                Ok(()) => Err(err),
                Err(abort_err) => Err(GitCacheError::UpstreamUnavailable(format!(
                    "{err}; additionally failed to abort multipart upload `{s3_key}`: {abort_err}"
                ))),
            };
        }

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

fn multipart_part_size(file_len: u64) -> Result<u64> {
    if file_len > S3_MAX_OBJECT_BYTES {
        return Err(GitCacheError::Validation(format!(
            "s3 object is too large for multipart upload: {file_len} bytes"
        )));
    }

    Ok(S3_DEFAULT_MULTIPART_PART_BYTES
        .max(file_len.div_ceil(S3_MAX_MULTIPART_PARTS))
        .max(S3_MIN_MULTIPART_PART_BYTES))
}

#[cfg(test)]
fn multipart_part_count(file_len: u64) -> Result<u64> {
    let part_size = multipart_part_size(file_len)?;
    Ok(file_len.div_ceil(part_size))
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
mod tests;
