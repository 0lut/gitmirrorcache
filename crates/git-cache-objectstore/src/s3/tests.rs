use super::{
    is_not_found, is_precondition_failed, multipart_part_count, multipart_part_size,
    S3_DEFAULT_MULTIPART_PART_BYTES, S3_MAX_MULTIPART_PARTS, S3_MAX_OBJECT_BYTES,
    S3_SINGLE_PUT_LIMIT_BYTES,
};
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

#[test]
fn multipart_part_size_uses_default_for_linux_sized_bundle() {
    assert_eq!(
        multipart_part_size(S3_SINGLE_PUT_LIMIT_BYTES + 1).unwrap(),
        S3_DEFAULT_MULTIPART_PART_BYTES
    );
}

#[test]
fn multipart_part_count_stays_within_s3_limit() {
    assert_eq!(
        multipart_part_count(S3_MAX_OBJECT_BYTES).unwrap(),
        S3_MAX_MULTIPART_PARTS
    );
}

#[test]
fn multipart_part_size_rejects_oversized_objects() {
    assert!(multipart_part_size(S3_MAX_OBJECT_BYTES + 1).is_err());
}
