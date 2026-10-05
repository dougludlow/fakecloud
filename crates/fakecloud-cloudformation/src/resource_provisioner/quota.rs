//! Service Quotas checks for the resources CloudFormation provisions by
//! writing service state directly. On AWS a stack creates them through the
//! service APIs, so a quota the user enforces fails the resource the same way
//! the API call would; these helpers resolve the same enforced limits the
//! services do (before any service lock is taken) and render the refusal as
//! the resource's failure reason.

use fakecloud_core::service::AwsServiceError;
use fakecloud_iam::quota::IamQuota;

use super::ResourceProvisioner;

/// The status reason of a resource a quota refused: the service's error
/// message, code and status, as a handler reports a failed API call.
pub(super) fn refusal(service: &str, err: AwsServiceError) -> String {
    format!(
        "{} (Service: {service}, Status Code: {}, Error Code: {})",
        err.message(),
        err.status().as_u16(),
        err.code()
    )
}

/// `check(quota, limit, count)` for IAM, as a resource failure.
pub(super) fn check_iam(quota: IamQuota, limit: Option<usize>, count: usize) -> Result<(), String> {
    fakecloud_iam::quota::check(quota, limit, count).map_err(|e| refusal("Iam", e))
}

/// The managed policies a principal would hold after attaching `adding` to
/// `current`, counting a policy already attached once.
pub(super) fn attached_after(current: &[String], adding: &[String]) -> usize {
    let mut all: Vec<&String> = current.iter().collect();
    for a in adding {
        if !all.contains(&a) {
            all.push(a);
        }
    }
    all.len()
}

impl ResourceProvisioner {
    /// The limit of an IAM quota when it is enforced for this stack's account.
    pub(super) fn iam_limit(&self, quota: IamQuota) -> Option<usize> {
        fakecloud_iam::quota::enforced_limit(
            self.quota_provider.as_ref(),
            &self.account_id,
            &self.region,
            quota,
        )
    }

    /// The enforced DynamoDB table limit of `region`.
    pub(super) fn dynamodb_table_limit(&self, region: &str) -> Option<usize> {
        fakecloud_dynamodb::quota::enforced_table_limit(
            self.quota_provider.as_ref(),
            &self.account_id,
            region,
        )
    }

    /// The enforced KMS customer managed key limit of `region`.
    pub(super) fn kms_key_limit(&self, region: &str) -> Option<usize> {
        fakecloud_kms::quota::enforced_key_limit(
            self.quota_provider.as_ref(),
            &self.account_id,
            region,
        )
    }

    /// The enforced S3 general purpose bucket limit of this account.
    pub(super) fn s3_bucket_limit(&self) -> Option<usize> {
        fakecloud_s3::quota::enforced_bucket_limit(
            self.quota_provider.as_ref(),
            &self.account_id,
            &self.region,
        )
    }

    /// The enforced Lambda code storage limit of this stack's region, in
    /// bytes.
    pub(super) fn lambda_storage_limit(&self) -> Option<i64> {
        fakecloud_lambda::quota::enforced_storage_limit(
            self.quota_provider.as_ref(),
            &self.account_id,
            &self.region,
        )
    }

    /// The applied Lambda concurrency limit, when Service Quotas is attached.
    pub(super) fn lambda_concurrency_quota(&self) -> Option<f64> {
        self.quota_provider.as_ref().and_then(|p| {
            p.applied_value(
                &self.account_id,
                &self.region,
                fakecloud_lambda::quota::SERVICE_CODE,
                fakecloud_lambda::quota::CONCURRENT_EXECUTIONS,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_provisioner, make_resource};
    use super::*;
    use std::sync::Arc;

    fn enforcing(quotas: fakecloud_core::quota::FixedQuotas) -> ResourceProvisioner {
        let mut prov = make_provisioner();
        prov.quota_provider = Some(Arc::new(quotas));
        prov
    }

    const TRUST: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;

    #[test]
    fn stack_resources_are_not_limited_without_a_provider() {
        let prov = make_provisioner();
        for i in 0..3 {
            prov.create_resource(&make_resource(
                "AWS::S3::Bucket",
                &format!("B{i}"),
                serde_json::json!({ "BucketName": format!("free-bucket-{i}") }),
            ))
            .expect("bucket");
        }
    }

    #[test]
    fn an_enforced_iam_quota_fails_the_stack_resource() {
        // Two seeded service-linked roles plus one created: a limit of 3.
        let prov = enforcing(
            fakecloud_core::quota::FixedQuotas::default()
                .with("iam", "L-FE177D64", 3.0)
                .with("iam", "L-0DA4ABF3", 1.0),
        );
        prov.create_resource(&make_resource(
            "AWS::IAM::Role",
            "R1",
            serde_json::json!({ "RoleName": "r1", "AssumeRolePolicyDocument": TRUST }),
        ))
        .expect("first role fits");
        let err = prov
            .create_resource(&make_resource(
                "AWS::IAM::Role",
                "R2",
                serde_json::json!({ "RoleName": "r2", "AssumeRolePolicyDocument": TRUST }),
            ))
            .expect_err("second role is over the quota");
        assert!(
            err.contains("Cannot exceed quota for RolesPerAccount: 3"),
            "{err}"
        );
        assert!(err.contains("Error Code: LimitExceeded"), "{err}");

        // A managed policy attaching to a role already at its per-role limit.
        let prov =
            enforcing(fakecloud_core::quota::FixedQuotas::default().with("iam", "L-0DA4ABF3", 1.0));
        prov.create_resource(&make_resource(
            "AWS::IAM::Role",
            "R",
            serde_json::json!({
                "RoleName": "full",
                "AssumeRolePolicyDocument": TRUST,
                "ManagedPolicyArns": ["arn:aws:iam::aws:policy/ReadOnlyAccess"],
            }),
        ))
        .expect("one managed policy fits");
        let err = prov
            .create_resource(&make_resource(
                "AWS::IAM::ManagedPolicy",
                "P",
                serde_json::json!({
                    "ManagedPolicyName": "extra",
                    "PolicyDocument": {"Version": "2012-10-17", "Statement": []},
                    "Roles": ["full"],
                }),
            ))
            .expect_err("the role has no room for another managed policy");
        assert!(err.contains("PoliciesPerRole: 1"), "{err}");
    }

    #[test]
    fn enforced_table_bucket_key_and_storage_quotas_fail_the_stack_resource() {
        let prov = enforcing(
            fakecloud_core::quota::FixedQuotas::default()
                .with("dynamodb", "L-F98FE922", 0.0)
                .with("s3", "L-DC2B2D3D", 0.0)
                .with("kms", "L-C2F1777E", 0.0)
                .with("lambda", "L-2ACBD22F", 0.0),
        );
        let table = prov
            .create_resource(&make_resource(
                "AWS::DynamoDB::Table",
                "T",
                serde_json::json!({
                    "TableName": "quota-table",
                    "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}],
                    "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}],
                    "BillingMode": "PAY_PER_REQUEST",
                }),
            ))
            .expect_err("table refused");
        assert!(table.contains("LimitExceededException"), "{table}");

        let bucket = prov
            .create_resource(&make_resource(
                "AWS::S3::Bucket",
                "B",
                serde_json::json!({ "BucketName": "quota-bucket" }),
            ))
            .expect_err("bucket refused");
        assert!(bucket.contains("TooManyBuckets"), "{bucket}");

        let key = prov
            .create_resource(&make_resource("AWS::KMS::Key", "K", serde_json::json!({})))
            .expect_err("key refused");
        assert!(key.contains("LimitExceededException"), "{key}");

        let layer = prov
            .create_resource(&make_resource(
                "AWS::Lambda::LayerVersion",
                "L",
                serde_json::json!({
                    "LayerName": "quota-layer",
                    "Content": {"ZipFile": "YWJj"},
                }),
            ))
            .expect_err("layer refused");
        assert!(layer.contains("CodeStorageExceededException"), "{layer}");
    }

    #[test]
    fn attached_after_counts_a_repeated_policy_once() {
        let current = vec!["a".to_string(), "b".to_string()];
        assert_eq!(attached_after(&current, &["b".into(), "c".into()]), 3);
        assert_eq!(attached_after(&[], &["a".into(), "a".into()]), 1);
    }

    #[test]
    fn refusal_carries_the_error_code_and_status() {
        let err = fakecloud_iam::quota::limit_exceeded(IamQuota::Roles, 3);
        assert_eq!(
            refusal("Iam", err),
            "Cannot exceed quota for RolesPerAccount: 3 (Service: Iam, Status Code: 409, \
             Error Code: LimitExceeded)"
        );
    }
}
