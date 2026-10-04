//! ACM certificates are regional resources: a certificate lives in the region
//! it was requested or imported in, `ListCertificates` only returns the
//! request region's certificates, and a certificate ARN from another region
//! is not found (ResourceNotFoundException), for reads and writes alike. The
//! account configuration is per region too.

mod helpers;

use aws_sdk_acm::primitives::Blob;
use aws_sdk_acm::types::ValidationMethod;
use helpers::TestServer;

const KEY_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
fake-key-bytes-for-fakecloud-tests-only\n\
-----END RSA PRIVATE KEY-----\n";

async fn acm_in(server: &TestServer, region: &str) -> aws_sdk_acm::Client {
    aws_sdk_acm::Client::new(&server.aws_config_in(region).await)
}

async fn list_arns(acm: &aws_sdk_acm::Client) -> Vec<String> {
    let mut arns: Vec<String> = acm
        .list_certificates()
        .send()
        .await
        .expect("ListCertificates")
        .certificate_summary_list()
        .iter()
        .map(|c| c.certificate_arn().unwrap().to_string())
        .collect();
    arns.sort();
    arns
}

fn not_found<E: aws_sdk_acm::error::ProvideErrorMetadata, R>(
    err: aws_sdk_acm::error::SdkError<E, R>,
) -> String {
    use aws_sdk_acm::error::ProvideErrorMetadata;
    err.code().unwrap_or_default().to_string()
}

#[tokio::test]
async fn certificates_are_scoped_to_their_region() {
    let server = TestServer::start().await;
    let east = acm_in(&server, "us-east-1").await;
    let west = acm_in(&server, "eu-west-1").await;

    let mut east_arns = Vec::new();
    let mut west_arns = Vec::new();
    for (acm, out) in [(&east, &mut east_arns), (&west, &mut west_arns)] {
        let requested = acm
            .request_certificate()
            .domain_name("same.example.com")
            .validation_method(ValidationMethod::Dns)
            .send()
            .await
            .expect("RequestCertificate")
            .certificate_arn()
            .unwrap()
            .to_string();
        let pem = "-----BEGIN CERTIFICATE-----\nCN=same.example.com\n-----END CERTIFICATE-----\n";
        let imported = acm
            .import_certificate()
            .certificate(Blob::new(pem.as_bytes().to_vec()))
            .private_key(Blob::new(KEY_PEM.as_bytes().to_vec()))
            .send()
            .await
            .expect("ImportCertificate")
            .certificate_arn()
            .unwrap()
            .to_string();
        out.push(requested);
        out.push(imported);
        out.sort();
    }
    assert!(east_arns
        .iter()
        .all(|a| a.starts_with("arn:aws:acm:us-east-1:")));
    assert!(west_arns
        .iter()
        .all(|a| a.starts_with("arn:aws:acm:eu-west-1:")));

    assert_eq!(list_arns(&east).await, east_arns);
    assert_eq!(list_arns(&west).await, west_arns);
    // An untouched region lists nothing.
    let south = acm_in(&server, "ap-south-1").await;
    assert!(list_arns(&south).await.is_empty());

    let east_arn = &east_arns[0];
    // Described in its own region...
    let d = east
        .describe_certificate()
        .certificate_arn(east_arn)
        .send()
        .await
        .expect("DescribeCertificate");
    assert_eq!(
        d.certificate().unwrap().certificate_arn(),
        Some(east_arn.as_str())
    );
    // ...but not found from another region, whether reading or writing.
    let err = west
        .describe_certificate()
        .certificate_arn(east_arn)
        .send()
        .await
        .expect_err("DescribeCertificate from another region");
    assert_eq!(not_found(err), "ResourceNotFoundException");
    let err = west
        .get_certificate()
        .certificate_arn(&east_arns[1])
        .send()
        .await
        .expect_err("GetCertificate from another region");
    assert_eq!(not_found(err), "ResourceNotFoundException");
    let err = west
        .delete_certificate()
        .certificate_arn(east_arn)
        .send()
        .await
        .expect_err("DeleteCertificate from another region");
    assert_eq!(not_found(err), "ResourceNotFoundException");
    // The delete from the wrong region left the certificate in place.
    assert_eq!(list_arns(&east).await, east_arns);

    // Deleting in its own region removes it from that region only.
    east.delete_certificate()
        .certificate_arn(east_arn)
        .send()
        .await
        .expect("DeleteCertificate");
    assert_eq!(list_arns(&east).await, east_arns[1..].to_vec());
    assert_eq!(list_arns(&west).await, west_arns);
}

#[tokio::test]
async fn account_configuration_is_per_region() {
    let server = TestServer::start().await;
    let east = acm_in(&server, "us-east-1").await;
    let west = acm_in(&server, "eu-west-1").await;

    west.put_account_configuration()
        .idempotency_token("tok")
        .expiry_events(
            aws_sdk_acm::types::ExpiryEventsConfiguration::builder()
                .days_before_expiry(12)
                .build(),
        )
        .send()
        .await
        .expect("PutAccountConfiguration");
    let got = west
        .get_account_configuration()
        .send()
        .await
        .expect("GetAccountConfiguration");
    assert_eq!(
        got.expiry_events().and_then(|e| e.days_before_expiry()),
        Some(12)
    );
    let got = east
        .get_account_configuration()
        .send()
        .await
        .expect("GetAccountConfiguration");
    assert_eq!(
        got.expiry_events().and_then(|e| e.days_before_expiry()),
        None
    );
}
