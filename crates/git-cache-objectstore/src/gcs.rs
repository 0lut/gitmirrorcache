use crate::{validate_key, ObjectMeta, ObjectStore, ObjectVersion};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use gcloud_storage::client::Client;
use gcloud_storage::http::objects::delete::DeleteObjectRequest;
use gcloud_storage::http::objects::download::Range;
use gcloud_storage::http::objects::get::GetObjectRequest;
use gcloud_storage::http::objects::list::ListObjectsRequest;
use gcloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use gcloud_storage::http::objects::Object;
use gcloud_storage::http::Error as GcsError;
use git_cache_core::{GitCacheError, Result};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

const GCS_LIST_PAGE_SIZE: i32 = 1000;

#[derive(Clone)]
pub struct GcsObjectStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl std::fmt::Debug for GcsObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsObjectStore")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl GcsObjectStore {
    pub fn new(
        client: Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Result<Self> {
        let bucket = bucket.into();
        if bucket.trim().is_empty() {
            return Err(GitCacheError::Validation(
                "gcs bucket name must not be empty".into(),
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

    fn gcs_key(&self, key: &str) -> Result<String> {
        validate_key(key)?;
        if self.prefix.is_empty() {
            Ok(key.to_string())
        } else {
            Ok(format!("{}/{}", self.prefix, key))
        }
    }

    fn get_request(&self, gcs_key: &str) -> GetObjectRequest {
        GetObjectRequest {
            bucket: self.bucket.clone(),
            object: gcs_key.to_string(),
            ..Default::default()
        }
    }
}

#[async_trait]
impl ObjectStore for GcsObjectStore {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let gcs_key = self.gcs_key(key)?;
        match self
            .client
            .download_object(&self.get_request(&gcs_key), &Range::default())
            .await
        {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(gcs_error("get", &gcs_key, err)),
        }
    }

    async fn put(&self, key: &str, value: Bytes) -> Result<()> {
        let gcs_key = self.gcs_key(key)?;
        let upload_type = UploadType::Simple(Media::new(gcs_key.clone()));
        self.client
            .upload_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    ..Default::default()
                },
                value,
                &upload_type,
            )
            .await
            .map_err(|err| gcs_error("put", &gcs_key, err))?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, value: Bytes) -> Result<bool> {
        let gcs_key = self.gcs_key(key)?;
        let upload_type = UploadType::Multipart(Box::new(Object {
            name: gcs_key.clone(),
            ..Default::default()
        }));
        match self
            .client
            .upload_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    if_generation_match: Some(0),
                    ..Default::default()
                },
                value,
                &upload_type,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_precondition_failed(&err) => Ok(false),
            Err(err) => Err(gcs_error("put_if_absent", &gcs_key, err)),
        }
    }

    async fn get_versioned(&self, key: &str) -> Result<Option<(Bytes, ObjectVersion)>> {
        let gcs_key = self.gcs_key(key)?;
        let object = match self.client.get_object(&self.get_request(&gcs_key)).await {
            Ok(object) => object,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(gcs_error("get_versioned", &gcs_key, err)),
        };

        let mut request = self.get_request(&gcs_key);
        request.generation = Some(object.generation);
        match self
            .client
            .download_object(&request, &Range::default())
            .await
        {
            Ok(data) => Ok(Some((
                Bytes::from(data),
                ObjectVersion::new(object.generation.to_string()),
            ))),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(gcs_error("get_versioned", &gcs_key, err)),
        }
    }

    async fn put_if_version_matches(
        &self,
        key: &str,
        value: Bytes,
        version: &ObjectVersion,
    ) -> Result<bool> {
        let gcs_key = self.gcs_key(key)?;
        let generation: i64 = version.token().parse().map_err(|_| {
            GitCacheError::Validation(format!(
                "gcs version token for `{gcs_key}` is not a generation number"
            ))
        })?;
        let upload_type = UploadType::Multipart(Box::new(Object {
            name: gcs_key.clone(),
            ..Default::default()
        }));
        match self
            .client
            .upload_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    if_generation_match: Some(generation),
                    ..Default::default()
                },
                value,
                &upload_type,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_precondition_failed(&err) || is_not_found(&err) => Ok(false),
            Err(err) => Err(gcs_error("put_if_version_matches", &gcs_key, err)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.head(key).await?.is_some())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let gcs_key = self.gcs_key(key)?;
        match self
            .client
            .delete_object(&DeleteObjectRequest {
                bucket: self.bucket.clone(),
                object: gcs_key.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(()) => Ok(()),
            Err(err) if is_not_found(&err) => Ok(()),
            Err(err) => Err(gcs_error("delete", &gcs_key, err)),
        }
    }

    async fn list_prefix(&self, prefix: &str, max_keys: Option<usize>) -> Result<Vec<String>> {
        let gcs_prefix = if self.prefix.is_empty() {
            prefix.to_string()
        } else {
            format!("{}/{}", self.prefix, prefix)
        };

        let mut keys = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let page_size = match max_keys {
                Some(limit) => limit
                    .saturating_sub(keys.len())
                    .min(GCS_LIST_PAGE_SIZE as usize) as i32,
                None => GCS_LIST_PAGE_SIZE,
            };
            let output = self
                .client
                .list_objects(&ListObjectsRequest {
                    bucket: self.bucket.clone(),
                    prefix: Some(gcs_prefix.clone()),
                    max_results: Some(page_size),
                    page_token: page_token.take(),
                    ..Default::default()
                })
                .await
                .map_err(|err| gcs_error("list", &gcs_prefix, err))?;

            if let Some(items) = output.items {
                for object in items {
                    let relative = if self.prefix.is_empty() {
                        object.name
                    } else {
                        object
                            .name
                            .strip_prefix(&format!("{}/", self.prefix))
                            .unwrap_or(&object.name)
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

            match output.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }

        Ok(keys)
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let gcs_key = self.gcs_key(key)?;
        match self.client.get_object(&self.get_request(&gcs_key)).await {
            Ok(object) => Ok(Some(object_meta(key, &object))),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(gcs_error("head", &gcs_key, err)),
        }
    }

    async fn get_file(&self, key: &str, path: &Path) -> Result<bool> {
        let gcs_key = self.gcs_key(key)?;
        let stream = match self
            .client
            .download_streamed_object(&self.get_request(&gcs_key), &Range::default())
            .await
        {
            Ok(stream) => stream,
            Err(err) if is_not_found(&err) => return Ok(false),
            Err(err) => return Err(gcs_error("get_file", &gcs_key, err)),
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut body = tokio_util::io::StreamReader::new(
            stream.map_err(|err| std::io::Error::other(format!("gcs stream error: {err}"))),
        );
        let mut file = tokio::fs::File::create(path).await?;
        tokio::io::copy(&mut body, &mut file)
            .await
            .map_err(|err| gcs_error("get_file body", &gcs_key, err))?;
        file.flush().await?;
        Ok(true)
    }

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let gcs_key = self.gcs_key(key)?;
        let file_len = tokio::fs::metadata(path).await?.len();
        let mut media = Media::new(gcs_key.clone());
        media.content_length = Some(file_len);
        let upload_type = UploadType::Simple(media);

        let file = tokio::fs::File::open(path).await?;
        let stream = ReaderStream::new(file);
        self.client
            .upload_streamed_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    ..Default::default()
                },
                stream,
                &upload_type,
            )
            .await
            .map_err(|err| gcs_error("put_file", &gcs_key, err))?;
        Ok(())
    }
}

fn object_meta(key: &str, object: &Object) -> ObjectMeta {
    let updated_at = object
        .updated
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.unix_timestamp(), t.nanosecond()));
    ObjectMeta {
        key: key.to_string(),
        len: object.size.max(0) as u64,
        updated_at,
    }
}

fn normalize_prefix(prefix: String) -> Result<String> {
    let prefix = prefix.trim_matches('/').to_string();
    if !prefix.is_empty() {
        validate_key(&prefix)?;
    }
    Ok(prefix)
}

fn status_code(error: &GcsError) -> Option<u16> {
    match error {
        GcsError::Response(response) => Some(response.code),
        GcsError::HttpClient(err) => err.status().map(|status| status.as_u16()),
        GcsError::RawResponse(err, _) => err.status().map(|status| status.as_u16()),
        _ => None,
    }
}

fn is_not_found(error: &GcsError) -> bool {
    status_code(error) == Some(404)
}

fn is_precondition_failed(error: &GcsError) -> bool {
    status_code(error) == Some(412)
}

fn gcs_error(op: &'static str, key: &str, error: impl std::fmt::Debug) -> GitCacheError {
    GitCacheError::UpstreamUnavailable(format!("gcs {op} `{key}` failed: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::{is_not_found, is_precondition_failed};
    use gcloud_storage::http::error::ErrorResponse;
    use gcloud_storage::http::Error as GcsError;

    fn response_error(code: u16, message: &str) -> GcsError {
        GcsError::Response(ErrorResponse {
            code,
            errors: vec![],
            message: message.to_string(),
        })
    }

    #[test]
    fn response_404_is_not_found() {
        assert!(is_not_found(&response_error(404, "object not found")));
    }

    #[test]
    fn response_412_is_precondition_failed() {
        assert!(is_precondition_failed(&response_error(
            412,
            "conditionNotMet"
        )));
    }

    #[test]
    fn non_404_error_with_404_in_message_is_not_not_found() {
        let error = response_error(403, "access denied for repos/repo404/base.bundle");
        assert!(!is_not_found(&error));
    }

    #[test]
    fn non_412_error_with_412_in_message_is_not_precondition_failed() {
        let error = response_error(403, "access denied for repos/repo412/base.bundle");
        assert!(!is_precondition_failed(&error));
    }
}
