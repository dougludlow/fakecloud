//! Request headers carried in the query string of a SigV4 presigned S3 URL
//! (#2643).
//!
//! A presigned URL can express `x-amz-*` request headers as query parameters;
//! the AWS SDK for JavaScript v3 presigner hoists every `x-amz-*` header that
//! way by default. S3 applies them to the request as if they had been sent as
//! headers, so an object uploaded through such a URL keeps its metadata, tags
//! and other header-driven settings.

mod helpers;

use aws_credential_types::Credentials;
use helpers::TestServer;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The auth parameters of a presigned URL whose signature fakecloud does not
/// check in its default mode.
const UNVERIFIED_PRESIGN: &str = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
    &X-Amz-Credential=test%2F20260101%2Fus-east-1%2Fs3%2Faws4_request\
    &X-Amz-Date=20260101T000000Z&X-Amz-Expires=604800\
    &X-Amz-SignedHeaders=host&X-Amz-Signature=00";

async fn create_bucket(s3: &aws_sdk_s3::Client, bucket: &str) {
    s3.create_bucket().bucket(bucket).send().await.unwrap();
}

fn tag_pairs(
    out: &aws_sdk_s3::operation::get_object_tagging::GetObjectTaggingOutput,
) -> Vec<(String, String)> {
    let mut tags: Vec<(String, String)> = out
        .tag_set()
        .iter()
        .map(|t| (t.key().to_string(), t.value().to_string()))
        .collect();
    tags.sort();
    tags
}

/// The exact reproduction from the issue: metadata and tagging carried as
/// query parameters on a presigned PUT are stored on the object.
#[tokio::test]
async fn presigned_put_applies_query_metadata_and_tagging() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    create_bucket(&s3, "presign-bucket").await;

    let url = format!(
        "{}/presign-bucket/f.txt?{UNVERIFIED_PRESIGN}\
         &x-amz-meta-color=blue&x-amz-tagging=env%3Dtest\
         &x-amz-storage-class=STANDARD_IA",
        server.endpoint()
    );
    let resp = reqwest::Client::new()
        .put(&url)
        .body("hello\n")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let head = s3
        .head_object()
        .bucket("presign-bucket")
        .key("f.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.metadata()
            .and_then(|m| m.get("color"))
            .map(String::as_str),
        Some("blue")
    );
    assert_eq!(
        head.storage_class().map(|c| c.as_str()),
        Some("STANDARD_IA")
    );

    let tagging = s3
        .get_object_tagging()
        .bucket("presign-bucket")
        .key("f.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        tag_pairs(&tagging),
        vec![("env".to_string(), "test".to_string())]
    );
}

/// A header sent directly wins over the same header carried in the query.
#[tokio::test]
async fn presigned_put_direct_header_wins_over_query() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    create_bucket(&s3, "presign-bucket").await;

    let url = format!(
        "{}/presign-bucket/f.txt?{UNVERIFIED_PRESIGN}&x-amz-meta-color=blue",
        server.endpoint()
    );
    let resp = reqwest::Client::new()
        .put(&url)
        .header("x-amz-meta-color", "red")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let head = s3
        .head_object()
        .bucket("presign-bucket")
        .key("f.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.metadata()
            .and_then(|m| m.get("color"))
            .map(String::as_str),
        Some("red")
    );
}

/// A presigned GET keeps honoring `response-*` overrides alongside hoisted
/// `x-amz-*` parameters, and the SigV4 auth parameters never leak into the
/// stored or returned headers.
#[tokio::test]
async fn presigned_get_keeps_response_overrides() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    create_bucket(&s3, "presign-bucket").await;
    s3.put_object()
        .bucket("presign-bucket")
        .key("f.txt")
        .body(b"hello".to_vec().into())
        .metadata("color", "blue")
        .send()
        .await
        .unwrap();

    let url = format!(
        "{}/presign-bucket/f.txt?{UNVERIFIED_PRESIGN}\
         &response-content-type=text%2Fplain&x-amz-checksum-mode=ENABLED",
        server.endpoint()
    );
    let resp = reqwest::Client::new().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/plain");
    assert_eq!(resp.headers()["x-amz-meta-color"], "blue");
    assert!(resp.headers().get("x-amz-credential").is_none());
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"hello");
}

/// Non-ASCII metadata: a percent-encoded UTF-8 query value and an RFC 2047
/// encoded header are stored as UTF-8 and returned RFC 2047-encoded, as S3
/// returns any value that is not pure US-ASCII. A raw non-ASCII header is
/// read as ISO-8859-1, matching the example in the S3 user guide.
#[tokio::test]
async fn non_ascii_metadata_is_kept_and_returned_rfc2047_encoded() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    create_bucket(&s3, "presign-bucket").await;
    let http = reqwest::Client::new();

    let url = format!(
        "{}/presign-bucket/query.txt?{UNVERIFIED_PRESIGN}&x-amz-meta-name=caf%C3%A9",
        server.endpoint()
    );
    assert_eq!(http.put(&url).body("x").send().await.unwrap().status(), 200);

    s3.put_object()
        .bucket("presign-bucket")
        .key("encoded.txt")
        .metadata("name", "=?UTF-8?B?Y2Fmw6k=?=")
        .body(b"x".to_vec().into())
        .send()
        .await
        .unwrap();

    let url = format!("{}/presign-bucket/raw.txt", server.endpoint());
    let resp = http
        .put(&url)
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/s3/aws4_request, \
             SignedHeaders=host, Signature=00",
        )
        .header(
            "x-amz-meta-nonascii",
            reqwest::header::HeaderValue::from_bytes("ÄMÄZÕÑ S3".as_bytes()).unwrap(),
        )
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    for key in ["query.txt", "encoded.txt"] {
        let head = s3
            .head_object()
            .bucket("presign-bucket")
            .key(key)
            .send()
            .await
            .unwrap();
        assert_eq!(
            head.metadata()
                .and_then(|m| m.get("name"))
                .map(String::as_str),
            Some("=?UTF-8?B?Y2Fmw6k=?="),
            "{key}"
        );
    }
    let head = s3
        .head_object()
        .bucket("presign-bucket")
        .key("raw.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.metadata()
            .and_then(|m| m.get("nonascii"))
            .map(String::as_str),
        Some("=?UTF-8?B?w4PChE3Dg8KEWsODwpXDg8KRIFMz?=")
    );
}

// --- Signature-verified presigned URLs ---

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 percent-encoding as SigV4 canonicalizes query keys and values.
fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a SigV4 presigned URL query string for `method path` that signs only
/// `host` and carries `extra` (`x-amz-*` request headers) as query
/// parameters, the way the JS v3 presigner does.
fn presign_query(
    method: &str,
    host: &str,
    path: &str,
    akid: &str,
    secret: &str,
    extra: &[(&str, &str)],
) -> String {
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = &amz_date[..8];
    let scope = format!("{date_stamp}/us-east-1/s3/aws4_request");
    let mut params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        ("X-Amz-Credential".into(), format!("{akid}/{scope}")),
        ("X-Amz-Date".into(), amz_date.clone()),
        ("X-Amz-Expires".into(), "900".into()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    encoded.sort();
    let canonical_query = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_request =
        format!("{method}\n{path}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, b"us-east-1");
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
    format!("{canonical_query}&X-Amz-Signature={signature}")
}

async fn bootstrap_access_key(server: &TestServer, user: &str) -> (String, String) {
    let boot = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "root-bypass"))
        .load()
        .await;
    let iam = aws_sdk_iam::Client::new(&boot);
    iam.create_user().user_name(user).send().await.unwrap();
    let ak = iam
        .create_access_key()
        .user_name(user)
        .send()
        .await
        .unwrap();
    let key = ak.access_key().unwrap();
    (
        key.access_key_id().to_string(),
        key.secret_access_key().to_string(),
    )
}

/// Under `--verify-sigv4` a correctly signed presigned PUT carrying metadata
/// and tagging in its query is accepted and applies them; altering one of
/// those query parameters after signing breaks the signature.
#[tokio::test]
async fn verified_presigned_put_applies_signed_query_headers() {
    let server = TestServer::start_with_env(&[("FAKECLOUD_VERIFY_SIGV4", "true")]).await;
    // Root-bypass `test` creds always pass verification, so they can seed and
    // read back state.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "root-bypass"))
        .load()
        .await;
    let s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(true)
            .build(),
    );
    create_bucket(&s3, "presign-bucket").await;
    let (akid, secret) = bootstrap_access_key(&server, "presigner").await;
    let host = server
        .endpoint()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();

    let query = presign_query(
        "PUT",
        &host,
        "/presign-bucket/f.txt",
        &akid,
        &secret,
        &[("x-amz-meta-color", "blue"), ("x-amz-tagging", "env=test")],
    );
    let http = reqwest::Client::new();
    let url = format!("{}/presign-bucket/f.txt?{query}", server.endpoint());
    let resp = http.put(&url).body("hello").send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let head = s3
        .head_object()
        .bucket("presign-bucket")
        .key("f.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.metadata()
            .and_then(|m| m.get("color"))
            .map(String::as_str),
        Some("blue")
    );
    let tagging = s3
        .get_object_tagging()
        .bucket("presign-bucket")
        .key("f.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        tag_pairs(&tagging),
        vec![("env".to_string(), "test".to_string())]
    );

    let tampered = url.replace("x-amz-meta-color=blue", "x-amz-meta-color=green");
    assert_ne!(tampered, url);
    let resp = http.put(&tampered).body("hello").send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert!(resp.text().await.unwrap().contains("SignatureDoesNotMatch"));
}
