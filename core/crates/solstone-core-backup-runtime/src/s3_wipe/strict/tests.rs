// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

#[test]
fn decoded_keys_upload_pairs_and_verbose_acknowledgments_are_exact() {
    let page = objects(br#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Prefix>a/</Prefix><Contents><Key>a/one&amp;&lt;&#13;&#x1F30E;</Key></Contents><IsTruncated>true</IsTruncated><NextContinuationToken>next&amp;token</NextContinuationToken></ListBucketResult>"#, "a/").unwrap();
    assert_eq!(page.keys, ["a/one&<\r🌎"]);
    assert_eq!(page.next.as_deref(), Some("next&token"));
    assert_eq!(
        String::from_utf8(delete_body(&page.keys).unwrap()).unwrap(),
        "<Delete><Object><Key>a/one&amp;&lt;&#13;🌎</Key></Object></Delete>"
    );
    deleted(
        "<DeleteResult><Deleted><Key>a/one&amp;&lt;&#13;🌎</Key></Deleted></DeleteResult>"
            .as_bytes(),
        &page.keys,
    )
    .unwrap();
    deleted("<DeleteResult><Error><Key>a/one&amp;&lt;&#13;🌎</Key><Code>NoSuchKey</Code></Error></DeleteResult>".as_bytes(), &page.keys).unwrap();
    let page = uploads(b"<ListMultipartUploadsResult><Upload><Key>a/first</Key><UploadId>id&amp;1</UploadId></Upload><Upload><Key>a/second</Key><UploadId>id2</UploadId></Upload><IsTruncated>true</IsTruncated><NextKeyMarker>a/second</NextKeyMarker><NextUploadIdMarker>id2</NextUploadIdMarker></ListMultipartUploadsResult>", "a/").unwrap();
    assert_eq!(
        page.uploads,
        [
            ("a/first".into(), "id&1".into()),
            ("a/second".into(), "id2".into())
        ]
    );
    assert_eq!(page.next, Some(("a/second".into(), "id2".into())));
    assert!(
        objects(
            b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
            "a/"
        )
        .unwrap()
        .keys
        .is_empty()
    );
}

#[test]
fn malformed_missing_duplicate_foreign_and_unbounded_documents_never_mean_empty() {
    let invalid: &[&[u8]] = &[
        b"", b"<ListBucketResult>", b"<ListBucketResult/>",
        b"<ListBucketResult><IsTruncated>false</ListBucketResult>",
        b"<ListBucketResult><IsTruncated>FALSE</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><IsTruncated>false</IsTruncated><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>ab/outside</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>a/one</Key><Key>a/two</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>a/one</Key></Contents><Contents><Key>a/one</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>a/&unknown;</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<!DOCTYPE ListBucketResult [<!ENTITY custom 'a/'>]><ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><IsTruncated>false</IsTruncated><NextContinuationToken>later</NextContinuationToken></ListBucketResult>",
        b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult><ListBucketResult/>",
        b"<ListBucketResult xmlns='foreign'><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><CommonPrefixes><Prefix>a/hidden/</Prefix></CommonPrefixes><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>a/&#1;</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
        b"<ListBucketResult><Contents><Key>a/\xff</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>",
    ];
    for body in invalid {
        assert!(objects(body, "a/").is_err(), "accepted {body:?}");
    }
    assert!(
        objects(
            "<ListBucketResult><IsTruncated>\u{a0}false</IsTruncated></ListBucketResult>"
                .as_bytes(),
            "a/"
        )
        .is_err()
    );
    let deep = format!(
        "<ListBucketResult>{}{}{}</ListBucketResult>",
        "<n>".repeat(MAX_DEPTH),
        "x",
        "</n>".repeat(MAX_DEPTH)
    );
    assert_eq!(
        objects(deep.as_bytes(), "a/").err(),
        Some(ProtocolError::Limit)
    );
    assert_eq!(
        objects(&vec![b' '; MAX_BODY + 1], "a/").err(),
        Some(ProtocolError::Limit)
    );
    for body in [
        "<ListMultipartUploadsResult><Upload><Key>a/one</Key></Upload><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>",
        "<ListMultipartUploadsResult><Upload><Key>a/one</Key><UploadId>1</UploadId><UploadId>2</UploadId></Upload><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>",
        "<ListMultipartUploadsResult><Key>a/one</Key><UploadId>1</UploadId><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>",
        "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextKeyMarker>a/one</NextKeyMarker></ListMultipartUploadsResult>",
    ] {
        assert!(uploads(body.as_bytes(), "a/").is_err(), "accepted {body}");
    }
}

#[test]
fn deletion_requires_one_exact_success_for_every_submitted_key() {
    let keys = vec!["a/one".into(), "a/two".into()];
    for body in [
        "<DeleteResult/>",
        "<DeleteResult><Deleted><Key>a/one</Key></Deleted></DeleteResult>",
        "<DeleteResult><Deleted><Key>a/one</Key></Deleted><Deleted><Key>a/one</Key></Deleted></DeleteResult>",
        "<DeleteResult><Deleted><Key>a/one</Key></Deleted><Deleted><Key>a/other</Key></Deleted></DeleteResult>",
        "<DeleteResult><Deleted><Key>a/one</Key></Deleted><Error><Key>a/two</Key><Code>AccessDenied</Code></Error></DeleteResult>",
        "<DeleteResult><Deleted><Key>a/one</Key></Deleted><Error><Code>NoSuchKey</Code></Error></DeleteResult>",
    ] {
        assert!(deleted(body.as_bytes(), &keys).is_err(), "accepted {body}");
    }
    deleted(b"<DeleteResult><Deleted><Key>a/two</Key></Deleted><Error><Key>a/one</Key><Code>NoSuchKey</Code></Error></DeleteResult>", &keys).unwrap();
}

#[test]
fn accumulated_payload_and_record_bounds_refuse_before_advancing() {
    let mut limit = InventoryLimit {
        bytes: MAX_PAYLOAD - 1,
        records: MAX_RECORDS - 1,
    };
    limit.add(&["x"]).unwrap();
    assert_eq!((limit.bytes, limit.records), (MAX_PAYLOAD, MAX_RECORDS));
    assert_eq!(limit.add(&[""]), Err(ProtocolError::Limit));
    assert_eq!((limit.bytes, limit.records), (MAX_PAYLOAD, MAX_RECORDS));
    let mut limit = InventoryLimit {
        bytes: MAX_PAYLOAD,
        records: 0,
    };
    assert_eq!(limit.add(&["x"]), Err(ProtocolError::Limit));
}
