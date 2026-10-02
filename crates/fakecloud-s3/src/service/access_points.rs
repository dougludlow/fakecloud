use bytes::Bytes;
use chrono::Utc;
use http::{HeaderMap, StatusCode};

use fakecloud_aws::arn::Arn;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::{s3_xml, xml_escape, S3Service};
use crate::state::S3AccessPoint;

/// Detect whether the request arrived via an S3 access point endpoint
/// (`s3-accesspoint.*` Host header). If so, resolve the alias to the
/// underlying bucket name and rewrite `req.path_segments` so the rest
/// of the dispatch logic sees a normal bucket-key request.
pub(crate) fn resolve_access_point(
    service: &S3Service,
    req: &mut AwsRequest,
) -> Result<(), AwsServiceError> {
    let host = req
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_access_point = host.to_ascii_lowercase().contains("s3-accesspoint");
    if !is_access_point {
        return Ok(());
    }

    // The alias is the first path segment (prepended by dispatch from the
    // Host header). It may be `{name}-{account_id}`.
    let alias = req.path_segments.first().cloned().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Access point alias missing from request path",
        )
    })?;

    let accts = service.state.read();
    let _empty = crate::state::S3State::new(&req.account_id, &req.region);
    let state = accts.get(&req.account_id).unwrap_or(&_empty);

    // Try exact alias match first, then strip account-id suffix.
    let ap = state
        .access_points
        .get(&alias)
        .or_else(|| {
            let suffix = format!("-{}", req.account_id);
            let name = alias.strip_suffix(&suffix)?;
            state.access_points.get(name)
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "NoSuchAccessPoint",
                format!("The specified accesspoint does not exist: {alias}"),
            )
        })?;

    let bucket = ap.bucket.clone();
    drop(accts);

    // Rewrite the first path segment from alias -> bucket.
    if !req.path_segments.is_empty() {
        req.path_segments[0] = bucket.clone();
    }
    // Rewrite raw_path prefix too so key extraction stays consistent.
    let old_prefix = format!("/{alias}/");
    let new_prefix = format!("/{bucket}/");
    if req.raw_path.starts_with(&old_prefix) {
        req.raw_path = format!("{}{}", new_prefix, &req.raw_path[old_prefix.len()..]);
    } else if req.raw_path == format!("/{alias}") || req.raw_path == format!("/{alias}/") {
        req.raw_path = format!("/{bucket}");
    }

    Ok(())
}

impl S3Service {
    pub(super) fn create_access_point(
        &self,
        account_id: &str,
        req: &AwsRequest,
        name: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body_str = std::str::from_utf8(&req.body).unwrap_or("");
        let bucket = crate::xml_util::extract_tag(body_str, "Bucket").ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "Bucket is required in CreateAccessPointConfiguration",
            )
        })?;

        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);

        // Bucket must exist in this account.
        if !state.buckets.contains_key(&bucket) {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidBucket",
                format!("The specified bucket does not exist: {bucket}"),
            ));
        }

        if state.access_points.contains_key(name) {
            return Err(AwsServiceError::aws_error(
                StatusCode::CONFLICT,
                "AccessPointAlreadyExists",
                format!("Access point already exists: {name}"),
            ));
        }

        let vpc_config = crate::xml_util::extract_tag(body_str, "VPCConfiguration");
        let public_access_block =
            crate::xml_util::extract_tag(body_str, "PublicAccessBlockConfiguration");

        let ap = S3AccessPoint {
            name: name.to_string(),
            bucket,
            account_id: account_id.to_string(),
            network_origin: if vpc_config.is_some() {
                "VPC".to_string()
            } else {
                "Internet".to_string()
            },
            vpc_configuration: vpc_config,
            creation_date: Utc::now(),
            public_access_block,
            bucket_account_id: Some(account_id.to_string()),
            tags: Default::default(),
        };

        let arn = Arn::s3_access_point(&req.region, account_id, name);
        let alias = format!("{name}-{account_id}");

        state.access_points.insert(name.to_string(), ap);

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <CreateAccessPointResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <AccessPointArn>{arn}</AccessPointArn>\
             <Alias>{alias}</Alias>\
             </CreateAccessPointResult>"
        );
        Ok(s3_xml(StatusCode::OK, body))
    }

    pub(super) fn get_access_point(
        &self,
        account_id: &str,
        req: &AwsRequest,
        name: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accts = self.state.read();
        let _empty = crate::state::S3State::new(account_id, &req.region);
        let state = accts.get(account_id).unwrap_or(&_empty);

        let ap = state.access_points.get(name).ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "NoSuchAccessPoint",
                format!("The specified accesspoint does not exist: {name}"),
            )
        })?;

        let arn = Arn::s3_access_point(&req.region, account_id, name);
        let alias = format!("{}-{}", ap.name, ap.account_id);

        let mut vpc_xml = String::new();
        if let Some(ref vpc) = ap.vpc_configuration {
            vpc_xml.push_str(&format!("<VpcConfiguration>{vpc}</VpcConfiguration>"));
        }

        let mut pab_xml = String::new();
        if let Some(ref pab) = ap.public_access_block {
            pab_xml.push_str(&format!(
                "<PublicAccessBlockConfiguration>{pab}</PublicAccessBlockConfiguration>"
            ));
        }

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <GetAccessPointResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>{name}</Name>\
             <Bucket>{bucket}</Bucket>\
             <NetworkOrigin>{origin}</NetworkOrigin>\
             <AccessPointArn>{arn}</AccessPointArn>\
             <Alias>{alias}</Alias>\
             {vpc_xml}\
             {pab_xml}\
             </GetAccessPointResult>",
            bucket = xml_escape(&ap.bucket),
            origin = xml_escape(&ap.network_origin),
        );
        Ok(s3_xml(StatusCode::OK, body))
    }

    pub(super) fn delete_access_point(
        &self,
        account_id: &str,
        _req: &AwsRequest,
        name: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let removed = state.access_points.remove(name).is_some();
        if !removed {
            return Err(AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "NoSuchAccessPoint",
                format!("The specified accesspoint does not exist: {name}"),
            ));
        }
        Ok(AwsResponse {
            status: StatusCode::NO_CONTENT,
            content_type: "application/xml".to_string(),
            body: Bytes::new().into(),
            headers: HeaderMap::new(),
        })
    }

    pub(super) fn list_access_points(
        &self,
        account_id: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accts = self.state.read();
        let _empty = crate::state::S3State::new(account_id, &req.region);
        let state = accts.get(account_id).unwrap_or(&_empty);

        let bucket_filter = req.query_params.get("bucket").cloned();
        let mut entries: Vec<&S3AccessPoint> = state
            .access_points
            .values()
            .filter(|ap| {
                if let Some(ref b) = bucket_filter {
                    &ap.bucket == b
                } else {
                    true
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        let mut ap_xml = String::new();
        for ap in entries {
            let arn = Arn::s3_access_point(&req.region, account_id, &ap.name);
            let alias = format!("{}-{}", ap.name, ap.account_id);
            ap_xml.push_str(&format!(
                "<AccessPoint>\
                 <Name>{name}</Name>\
                 <Bucket>{bucket}</Bucket>\
                 <NetworkOrigin>{origin}</NetworkOrigin>\
                 <AccessPointArn>{arn}</AccessPointArn>\
                 <Alias>{alias}</Alias>\
                 </AccessPoint>",
                name = xml_escape(&ap.name),
                bucket = xml_escape(&ap.bucket),
                origin = xml_escape(&ap.network_origin),
            ));
        }

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListAccessPointsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             {ap_xml}\
             </ListAccessPointsResult>"
        );
        Ok(s3_xml(StatusCode::OK, body))
    }
}
