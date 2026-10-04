//! Lambda functions, layers and account settings are regional resources: the
//! same function name exists independently in every region (different code
//! and configuration), `ListFunctions` lists the request region's functions
//! only, and a function ARN addresses the region it names, so another
//! region's ARN is unreachable the way AWS refuses it.

mod helpers;

use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{Environment, FunctionCode, Runtime};
use helpers::TestServer;

async fn lambda_in(server: &TestServer, region: &str) -> aws_sdk_lambda::Client {
    aws_sdk_lambda::Client::new(&server.aws_config_in(region).await)
}

fn zip_with(content: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("index.py", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(content).unwrap();
    writer.finish().unwrap().into_inner()
}

async fn create(client: &aws_sdk_lambda::Client, name: &str, description: &str, zip: Vec<u8>) {
    client
        .create_function()
        .function_name(name)
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/test-role")
        .handler("index.handler")
        .description(description)
        .environment(
            Environment::builder()
                .variables("WHERE", description)
                .build(),
        )
        .code(FunctionCode::builder().zip_file(Blob::new(zip)).build())
        .send()
        .await
        .unwrap_or_else(|e| panic!("CreateFunction {name} ({description}): {e:?}"));
}

#[tokio::test]
async fn same_function_name_coexists_in_two_regions() {
    let server = TestServer::start().await;
    let east = lambda_in(&server, "us-east-1").await;
    let west = lambda_in(&server, "eu-west-1").await;
    let east_zip = zip_with(b"def handler(e, c): return 'east'\n");
    let west_zip = zip_with(b"def handler(e, c): return 'west'\n");

    create(&east, "regional-fn", "east", east_zip.clone()).await;
    // Same name in another region: a separate function, not a conflict.
    create(&west, "regional-fn", "west", west_zip.clone()).await;

    for (client, region, description, zip) in [
        (&east, "us-east-1", "east", &east_zip),
        (&west, "eu-west-1", "west", &west_zip),
    ] {
        let got = client
            .get_function()
            .function_name("regional-fn")
            .send()
            .await
            .expect("GetFunction");
        let config = got.configuration().unwrap();
        assert_eq!(config.description(), Some(description));
        assert_eq!(
            config.function_arn(),
            Some(format!("arn:aws:lambda:{region}:123456789012:function:regional-fn").as_str())
        );
        assert_eq!(
            config
                .environment()
                .and_then(|e| e.variables())
                .and_then(|v| v.get("WHERE"))
                .map(String::as_str),
            Some(description)
        );
        // The code download serves this region's deployment package.
        let location = got.code().unwrap().location().unwrap();
        let body = reqwest::get(location)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), zip.as_slice(), "{region} code");

        // ListFunctions is scoped to the request region.
        let listed = client.list_functions().send().await.expect("ListFunctions");
        let names: Vec<_> = listed
            .functions()
            .iter()
            .map(|f| (f.function_name().unwrap(), f.description().unwrap()))
            .collect();
        assert_eq!(names, vec![("regional-fn", description)], "{region}");
    }

    // A region with no functions lists none.
    let south = lambda_in(&server, "ap-south-1").await;
    assert!(south
        .list_functions()
        .send()
        .await
        .unwrap()
        .functions()
        .is_empty());

    // Deleting one region's function leaves the other region's untouched.
    west.delete_function()
        .function_name("regional-fn")
        .send()
        .await
        .expect("DeleteFunction west");
    assert!(west
        .get_function()
        .function_name("regional-fn")
        .send()
        .await
        .is_err());
    east.get_function()
        .function_name("regional-fn")
        .send()
        .await
        .expect("east function survives the west delete");
}

#[tokio::test]
async fn function_arn_of_another_region_is_unreachable() {
    let server = TestServer::start().await;
    let east = lambda_in(&server, "us-east-1").await;
    let west = lambda_in(&server, "eu-west-1").await;
    create(&west, "west-only", "west", zip_with(b"x")).await;
    let west_arn = "arn:aws:lambda:eu-west-1:123456789012:function:west-only";

    // The ARN resolves in its own region.
    west.get_function()
        .function_name(west_arn)
        .send()
        .await
        .expect("GetFunction by ARN in its region");

    // From another region, AWS refuses to reach across.
    let err = east
        .get_function()
        .function_name(west_arn)
        .send()
        .await
        .expect_err("cross-region ARN must fail");
    let service_err = err.into_service_error();
    assert!(
        service_err.is_resource_not_found_exception(),
        "{service_err:?}"
    );
    assert_eq!(
        service_err.meta().message(),
        Some("Functions from 'eu-west-1' are not reachable in this region ('us-east-1')")
    );

    let err = east
        .invoke()
        .function_name(west_arn)
        .send()
        .await
        .expect_err("cross-region invoke must fail");
    assert!(err.into_service_error().is_resource_not_found_exception());

    // The bare name does not exist in us-east-1 either.
    let err = east
        .get_function()
        .function_name("west-only")
        .send()
        .await
        .expect_err("name is not in us-east-1");
    assert!(err.into_service_error().is_resource_not_found_exception());
}

#[tokio::test]
async fn layers_and_account_settings_are_per_region() {
    let server = TestServer::start().await;
    let east = lambda_in(&server, "us-east-1").await;
    let west = lambda_in(&server, "eu-west-1").await;

    for (client, region) in [(&east, "us-east-1"), (&west, "eu-west-1")] {
        let published = client
            .publish_layer_version()
            .layer_name("shared-layer")
            .content(
                aws_sdk_lambda::types::LayerVersionContentInput::builder()
                    .zip_file(Blob::new(zip_with(region.as_bytes())))
                    .build(),
            )
            .send()
            .await
            .expect("PublishLayerVersion");
        // Each region numbers its own versions.
        assert_eq!(published.version(), 1, "{region}");
        assert_eq!(
            published.layer_version_arn(),
            Some(format!("arn:aws:lambda:{region}:123456789012:layer:shared-layer:1").as_str())
        );
    }

    create(&east, "reserved", "east", zip_with(b"x")).await;
    east.put_function_concurrency()
        .function_name("reserved")
        .reserved_concurrent_executions(100)
        .send()
        .await
        .expect("PutFunctionConcurrency");

    let east_settings = east.get_account_settings().send().await.unwrap();
    let west_settings = west.get_account_settings().send().await.unwrap();
    assert_eq!(
        east_settings
            .account_limit()
            .unwrap()
            .unreserved_concurrent_executions(),
        Some(900)
    );
    assert_eq!(east_settings.account_usage().unwrap().function_count(), 1);
    // The reservation and the function count belong to us-east-1 only.
    assert_eq!(
        west_settings
            .account_limit()
            .unwrap()
            .unreserved_concurrent_executions(),
        Some(1000)
    );
    assert_eq!(west_settings.account_usage().unwrap().function_count(), 0);
}
