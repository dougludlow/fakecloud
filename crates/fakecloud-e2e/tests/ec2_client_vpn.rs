//! Client VPN endpoint E2E: connection logging options set on Create/Modify
//! read back through DescribeClientVpnEndpoints, and Modify on an endpoint
//! that does not exist is refused. Metadata-only, no container runtime needed.

mod helpers;

use aws_sdk_ec2::types::{
    ClientVpnAuthenticationRequest, ClientVpnAuthenticationType, ConnectionLogOptions,
};
use helpers::TestServer;

async fn connection_log(
    c: &aws_sdk_ec2::Client,
    id: &str,
) -> aws_sdk_ec2::types::ConnectionLogResponseOptions {
    c.describe_client_vpn_endpoints()
        .client_vpn_endpoint_ids(id)
        .send()
        .await
        .unwrap()
        .client_vpn_endpoints()[0]
        .connection_log_options()
        .cloned()
        .expect("connectionLogOptions is always described")
}

#[tokio::test]
async fn connection_log_options_round_trip() {
    let s = TestServer::start().await;
    let c = s.ec2_client().await;
    let id = c
        .create_client_vpn_endpoint()
        .client_cidr_block("10.0.0.0/22")
        .server_certificate_arn("arn:aws:acm:us-east-1:123456789012:certificate/abc")
        .authentication_options(
            ClientVpnAuthenticationRequest::builder()
                .r#type(ClientVpnAuthenticationType::CertificateAuthentication)
                .build(),
        )
        .connection_log_options(
            ConnectionLogOptions::builder()
                .enabled(true)
                .cloudwatch_log_group("vpn-logs")
                .cloudwatch_log_stream("connections")
                .build(),
        )
        .send()
        .await
        .unwrap()
        .client_vpn_endpoint_id()
        .unwrap()
        .to_string();

    let log = connection_log(&c, &id).await;
    assert_eq!(log.enabled(), Some(true));
    assert_eq!(log.cloudwatch_log_group(), Some("vpn-logs"));
    assert_eq!(log.cloudwatch_log_stream(), Some("connections"));

    c.modify_client_vpn_endpoint()
        .client_vpn_endpoint_id(&id)
        .connection_log_options(ConnectionLogOptions::builder().enabled(false).build())
        .send()
        .await
        .unwrap();
    let log = connection_log(&c, &id).await;
    assert_eq!(log.enabled(), Some(false));
    assert_eq!(log.cloudwatch_log_group(), None);

    // Logging cannot be enabled without somewhere to publish to.
    let err = c
        .modify_client_vpn_endpoint()
        .client_vpn_endpoint_id(&id)
        .connection_log_options(ConnectionLogOptions::builder().enabled(true).build())
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidParameterValue")
    );
}

#[tokio::test]
async fn modify_missing_endpoint_is_not_found() {
    let s = TestServer::start().await;
    let c = s.ec2_client().await;
    let err = c
        .modify_client_vpn_endpoint()
        .client_vpn_endpoint_id("cvpn-endpoint-0123456789abcdef0")
        .description("x")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidClientVpnEndpointId.NotFound")
    );
}
