// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Whether an owner's own bucket keeps what restic deletes. Read-only; any
//! request or parse failure reads as unknown, never as "removes".

use std::time::Duration;

use serde_json::{Value, json};
use solstone_core_backup::Destination;
use solstone_core_backup::archive_admission::ArchiveCancellation;

use crate::hosted_runtime::{HttpRequest, HttpTransport};
use crate::s3_wipe::authorization_header;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const B2_AUTHORIZE: &str = "https://api.backblazeb2.com/b2api/v3/b2_authorize_account";

/// What keeps deleted backup data in the owner's own storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageKeeps {
    /// S3 versioning is on (or was, and is suspended).
    Versions,
    /// S3 object lock is on.
    ObjectLock,
    /// B2 file lock is on.
    FileLock,
    /// B2 has no lifecycle rule that deletes hidden files, so a delete only hides.
    HiddenFiles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageRetention {
    /// Every check answered and none keeps deleted data.
    Removes,
    Keeps(StorageKeeps),
    /// A check couldn't be made or answered.
    Unknown,
}

struct Call<'a> {
    transport: &'a dyn HttpTransport,
    cancellation: &'a ArchiveCancellation,
    timeout: Duration,
}

impl Call<'_> {
    fn send(&self, request: HttpRequest) -> Option<(u16, Vec<u8>)> {
        let request = HttpRequest {
            timeout: self.timeout,
            ..request
        };
        let response = self
            .transport
            .execute_cancellable(&request, self.cancellation)
            .ok()?;
        Some((response.status, response.body))
    }
}

/// `s3:https://host/bucket/prefix`, `s3:host/bucket/prefix` → (base URL, bucket).
fn s3_target(repository: &str) -> Option<(String, String)> {
    let rest = repository.strip_prefix("s3:")?;
    let (scheme, rest) = match rest.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (scheme, rest),
        Some(_) => return None,
        None => ("https", rest),
    };
    let mut parts = rest.splitn(3, '/');
    let host = parts.next().filter(|host| !host.is_empty())?;
    let bucket = parts.next().filter(|bucket| !bucket.is_empty())?;
    Some((format!("{scheme}://{host}"), bucket.to_owned()))
}

/// `b2:bucket:path` → bucket.
fn b2_bucket(repository: &str) -> Option<String> {
    let rest = repository.strip_prefix("b2:")?;
    let bucket = rest.split(':').next().filter(|bucket| !bucket.is_empty())?;
    Some(bucket.to_owned())
}

fn xml_text(body: &[u8], field: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let open = format!("<{field}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{field}>"))? + start;
    Some(text[start..end].trim().to_owned())
}

fn credential<'a>(destination: &'a Destination, key: &str) -> Option<&'a str> {
    destination
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn s3_get(
    base: &str,
    bucket: &str,
    query: &str,
    destination: &Destination,
    region: &str,
    amz_date: &str,
) -> Option<HttpRequest> {
    let access = credential(destination, "access_key_id")?;
    let secret = credential(destination, "secret_access_key")?;
    let host = base.split_once("://")?.1;
    let uri = format!("/{bucket}");
    let mut headers = vec![
        ("Host".to_owned(), host.to_owned()),
        ("x-amz-content-sha256".to_owned(), EMPTY_SHA256.to_owned()),
        ("x-amz-date".to_owned(), amz_date.to_owned()),
    ];
    if let Some(token) = credential(destination, "session_token") {
        headers.push(("x-amz-security-token".to_owned(), token.to_owned()));
    }
    let authorization = authorization_header(
        "GET",
        &uri,
        &format!("{query}="),
        &headers,
        EMPTY_SHA256,
        access,
        secret,
        region,
        amz_date,
    );
    headers.push(("Authorization".to_owned(), authorization));
    headers.push(("Connection".to_owned(), "close".to_owned()));
    Some(HttpRequest {
        method: "GET".into(),
        url: format!("{base}{uri}?{query}="),
        headers,
        body: vec![],
        timeout: Duration::ZERO,
    })
}

fn observe_s3(call: &Call<'_>, destination: &Destination, amz_date: &str) -> StorageRetention {
    let Some((base, bucket)) = s3_target(&destination.repository) else {
        return StorageRetention::Unknown;
    };
    let mut region = credential(destination, "region")
        .unwrap_or("us-east-1")
        .to_owned();
    let mut get = |query: &str| -> Option<(u16, Vec<u8>)> {
        let first = call.send(s3_get(
            &base,
            &bucket,
            query,
            destination,
            &region,
            amz_date,
        )?)?;
        // A wrong signing region names the right one; try that once.
        if first.0 == 400
            && let Some(named) = xml_text(&first.1, "Region").filter(|named| *named != region)
        {
            region = named;
            return call.send(s3_get(
                &base,
                &bucket,
                query,
                destination,
                &region,
                amz_date,
            )?);
        }
        Some(first)
    };
    // Object lock implies versioning on S3; the lock is the stronger answer.
    let lock = match get("object-lock") {
        Some((200, body)) => xml_text(&body, "ObjectLockEnabled").as_deref() == Some("Enabled"),
        Some((404, body))
            if xml_text(&body, "Code").as_deref()
                == Some("ObjectLockConfigurationNotFoundError") =>
        {
            false
        }
        _ => return StorageRetention::Unknown,
    };
    if lock {
        return StorageRetention::Keeps(StorageKeeps::ObjectLock);
    }
    match get("versioning") {
        Some((200, body)) => match xml_text(&body, "Status").as_deref() {
            Some("Enabled" | "Suspended") => StorageRetention::Keeps(StorageKeeps::Versions),
            Some(_) | None => StorageRetention::Removes,
        },
        _ => StorageRetention::Unknown,
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn observe_b2(call: &Call<'_>, destination: &Destination) -> StorageRetention {
    let (Some(bucket), Some(account), Some(key)) = (
        b2_bucket(&destination.repository),
        credential(destination, "account_id"),
        credential(destination, "account_key"),
    ) else {
        return StorageRetention::Unknown;
    };
    let basic = base64(format!("{account}:{key}").as_bytes());
    let Some((200, body)) = call.send(HttpRequest {
        method: "GET".into(),
        url: B2_AUTHORIZE.into(),
        headers: vec![
            ("Authorization".into(), format!("Basic {basic}")),
            ("Connection".into(), "close".into()),
        ],
        body: vec![],
        timeout: Duration::ZERO,
    }) else {
        return StorageRetention::Unknown;
    };
    let Ok(authorized) = serde_json::from_slice::<Value>(&body) else {
        return StorageRetention::Unknown;
    };
    let (Some(account_id), Some(token), Some(api)) = (
        authorized["accountId"].as_str(),
        authorized["authorizationToken"].as_str(),
        authorized["apiInfo"]["storageApi"]["apiUrl"].as_str(),
    ) else {
        return StorageRetention::Unknown;
    };
    let Some((200, body)) = call.send(HttpRequest {
        method: "POST".into(),
        url: format!("{}/b2api/v3/b2_list_buckets", api.trim_end_matches('/')),
        headers: vec![
            ("Authorization".into(), token.to_owned()),
            ("Content-Type".into(), "application/json".into()),
            ("Connection".into(), "close".into()),
        ],
        body: json!({"accountId": account_id, "bucketName": bucket})
            .to_string()
            .into_bytes(),
        timeout: Duration::ZERO,
    }) else {
        return StorageRetention::Unknown;
    };
    let Ok(listed) = serde_json::from_slice::<Value>(&body) else {
        return StorageRetention::Unknown;
    };
    let Some(found) = listed["buckets"]
        .as_array()
        .and_then(|buckets| buckets.iter().find(|row| row["bucketName"] == bucket))
    else {
        return StorageRetention::Unknown;
    };
    if found["fileLockConfiguration"]["value"]["isFileLockEnabled"] == true {
        return StorageRetention::Keeps(StorageKeeps::FileLock);
    }
    // Only a whole-bucket rule that deletes hidden files reaches every repository path.
    let deletes_hidden = found["lifecycleRules"].as_array().is_some_and(|rules| {
        rules.iter().any(|rule| {
            rule["fileNamePrefix"].as_str() == Some("")
                && rule["daysFromHidingToDeleting"].as_u64().is_some()
        })
    });
    if deletes_hidden {
        StorageRetention::Removes
    } else {
        StorageRetention::Keeps(StorageKeeps::HiddenFiles)
    }
}

/// Read-only checks against the owner's own bucket, bounded by `timeout` each.
pub(crate) fn observe(
    destination: &Destination,
    transport: &dyn HttpTransport,
    cancellation: &ArchiveCancellation,
    timeout: Duration,
    amz_date: &str,
) -> StorageRetention {
    let call = Call {
        transport,
        cancellation,
        timeout,
    };
    match destination.backend.as_str() {
        "s3" => observe_s3(&call, destination, amz_date),
        "b2" => observe_b2(&call, destination),
        _ => StorageRetention::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosted_runtime::{HttpError, HttpResponse};
    use serde_json::Map;
    use std::cell::RefCell;

    struct Script(RefCell<Vec<(u16, &'static str)>>, RefCell<Vec<String>>);
    impl HttpTransport for Script {
        fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
            Err(HttpError::Other)
        }
        fn execute_cancellable(
            &self,
            request: &HttpRequest,
            _: &ArchiveCancellation,
        ) -> Result<HttpResponse, HttpError> {
            self.1.borrow_mut().push(request.url.clone());
            let mut script = self.0.borrow_mut();
            if script.is_empty() {
                return Err(HttpError::Unreachable);
            }
            let (status, body) = script.remove(0);
            Ok(HttpResponse {
                status,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            })
        }
    }

    fn destination(backend: &str, repository: &str) -> Destination {
        let mut credentials = Map::new();
        for key in [
            "access_key_id",
            "secret_access_key",
            "account_id",
            "account_key",
        ] {
            credentials.insert(key.into(), Value::String("synthetic".into()));
        }
        Destination {
            repository: repository.into(),
            backend: backend.into(),
            credentials,
        }
    }

    fn observe_with(
        backend: &str,
        repository: &str,
        script: &[(u16, &'static str)],
    ) -> (StorageRetention, Vec<String>) {
        let transport = Script(RefCell::new(script.to_vec()), RefCell::new(vec![]));
        let result = observe(
            &destination(backend, repository),
            &transport,
            &ArchiveCancellation::new(),
            Duration::from_secs(5),
            "20261012T000000Z",
        );
        (result, transport.1.into_inner())
    }

    #[test]
    fn repository_strings_name_the_bucket() {
        assert_eq!(
            s3_target("s3:https://s3.example.com/bucket/journal"),
            Some(("https://s3.example.com".into(), "bucket".into()))
        );
        assert_eq!(
            s3_target("s3:s3.amazonaws.com/bucket"),
            Some(("https://s3.amazonaws.com".into(), "bucket".into()))
        );
        assert_eq!(s3_target("s3:ftp://host/bucket"), None);
        assert_eq!(s3_target("s3:host"), None);
        assert_eq!(b2_bucket("b2:bucket:journal/x"), Some("bucket".into()));
        assert_eq!(b2_bucket("b2::x"), None);
    }

    #[test]
    fn s3_versioning_object_lock_and_failures() {
        let off = "<VersioningConfiguration/>";
        let unlocked = "<Error><Code>ObjectLockConfigurationNotFoundError</Code></Error>";
        let locked = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>";
        let (result, urls) = observe_with("s3", "s3:https://h/b/p", &[(404, unlocked), (200, off)]);
        assert_eq!(result, StorageRetention::Removes);
        assert_eq!(
            urls,
            ["https://h/b?object-lock=", "https://h/b?versioning="]
        );
        for body in [
            "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            "<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>",
        ] {
            assert_eq!(
                observe_with("s3", "s3:https://h/b/p", &[(404, unlocked), (200, body)]).0,
                StorageRetention::Keeps(StorageKeeps::Versions)
            );
        }
        // A lock wins without asking about versions.
        let (result, urls) = observe_with("s3", "s3:https://h/b/p", &[(200, locked)]);
        assert_eq!(result, StorageRetention::Keeps(StorageKeeps::ObjectLock));
        assert_eq!(urls.len(), 1);
        // A refused or unreachable check is never read as "removes".
        assert_eq!(
            observe_with("s3", "s3:https://h/b/p", &[(403, "denied")]).0,
            StorageRetention::Unknown
        );
        assert_eq!(
            observe_with("s3", "s3:https://h/b/p", &[(404, unlocked), (403, "")]).0,
            StorageRetention::Unknown
        );
        assert_eq!(
            observe_with("s3", "s3:https://h/b/p", &[]).0,
            StorageRetention::Unknown
        );
        // A wrong region is retried once with the named one.
        let (result, urls) = observe_with(
            "s3",
            "s3:https://h/b/p",
            &[
                (
                    400,
                    "<Error><Code>AuthorizationHeaderMalformed</Code><Region>eu-west-1</Region></Error>",
                ),
                (404, unlocked),
                (200, off),
            ],
        );
        assert_eq!(result, StorageRetention::Removes);
        assert_eq!(urls.len(), 3);
    }

    #[test]
    fn b2_lifecycle_and_file_lock() {
        let authorized = r#"{"accountId":"a","authorizationToken":"t","apiInfo":{"storageApi":{"apiUrl":"https://api.example"}}}"#;
        let bucket = |rules: &'static str, lock: bool| -> &'static str {
            Box::leak(format!(
                r#"{{"buckets":[{{"bucketName":"b","lifecycleRules":{rules},"fileLockConfiguration":{{"value":{{"isFileLockEnabled":{lock}}}}}}}]}}"#
            ).into_boxed_str())
        };
        assert_eq!(
            observe_with(
                "b2",
                "b2:b:p",
                &[(200, authorized), (200, bucket("[]", false))]
            )
            .0,
            StorageRetention::Keeps(StorageKeeps::HiddenFiles)
        );
        assert_eq!(
            observe_with(
                "b2",
                "b2:b:p",
                &[
                    (200, authorized),
                    (
                        200,
                        bucket(
                            r#"[{"fileNamePrefix":"","daysFromHidingToDeleting":1}]"#,
                            false
                        )
                    )
                ]
            )
            .0,
            StorageRetention::Removes
        );
        assert_eq!(
            observe_with(
                "b2",
                "b2:b:p",
                &[
                    (200, authorized),
                    (
                        200,
                        bucket(
                            r#"[{"fileNamePrefix":"other/","daysFromHidingToDeleting":1}]"#,
                            false
                        )
                    )
                ]
            )
            .0,
            StorageRetention::Keeps(StorageKeeps::HiddenFiles)
        );
        assert_eq!(
            observe_with(
                "b2",
                "b2:b:p",
                &[(200, authorized), (200, bucket("[]", true))]
            )
            .0,
            StorageRetention::Keeps(StorageKeeps::FileLock)
        );
        assert_eq!(
            observe_with("b2", "b2:b:p", &[(401, "{}")]).0,
            StorageRetention::Unknown
        );
        let (_, urls) = observe_with(
            "b2",
            "b2:b:p",
            &[(200, authorized), (200, bucket("[]", false))],
        );
        assert_eq!(
            urls,
            [B2_AUTHORIZE, "https://api.example/b2api/v3/b2_list_buckets"]
        );
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"acct:key"), "YWNjdDprZXk=");
    }
}
