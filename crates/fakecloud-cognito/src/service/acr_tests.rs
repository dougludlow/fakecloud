//! ACR level names (`AcrConfiguration`), identity provider ACR mapping
//! (`AcrMapping`) and step-up sign-in with `TARGET_ACR_VALUES`.

use super::*;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn req(action: &str, body: Value) -> AwsRequest {
    AwsRequest {
        service: "cognito-idp".to_string(),
        action: action.to_string(),
        region: "us-east-1".to_string(),
        account_id: "123456789012".to_string(),
        request_id: "test".to_string(),
        headers: http::HeaderMap::new(),
        query_params: std::collections::HashMap::new(),
        body: bytes::Bytes::from(body.to_string()),
        body_stream: parking_lot::Mutex::new(None),
        path_segments: vec![],
        raw_path: "/".to_string(),
        raw_query: String::new(),
        method: http::Method::POST,
        is_query_protocol: false,
        access_key_id: None,
        principal: None,
    }
}

fn json_of(resp: &AwsResponse) -> Value {
    serde_json::from_slice(resp.body.expect_bytes()).unwrap()
}

fn err_code(r: Result<AwsResponse, AwsServiceError>) -> String {
    match r {
        Ok(resp) => panic!("expected an error, got {}", json_of(&resp)),
        Err(e) => e.code().to_string(),
    }
}

fn svc() -> CognitoService {
    CognitoService::new(std::sync::Arc::new(parking_lot::RwLock::new(
        fakecloud_core::multi_account::MultiAccountState::new(
            "123456789012",
            "us-east-1",
            "http://localhost:4566",
        ),
    )))
}

fn create_pool(svc: &CognitoService, extra: Value) -> Result<Value, AwsServiceError> {
    let mut body = json!({ "PoolName": "acr" });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    block_on(svc.create_user_pool(&req("CreateUserPool", body)))
        .map(|r| json_of(&r)["UserPool"].clone())
}

fn describe_pool(svc: &CognitoService, pool_id: &str) -> Value {
    json_of(
        &svc.describe_user_pool(&req("DescribeUserPool", json!({ "UserPoolId": pool_id })))
            .unwrap(),
    )["UserPool"]
        .clone()
}

fn claims(jwt: &str) -> Value {
    use base64::Engine;
    let payload = jwt.split('.').nth(1).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap()
}

const PASSWORD: &str = "Step!Up9pass";

/// An Essentials-tier pool (or `tier`) with a USER_AUTH client and a
/// confirmed user `alice`. `totp` gives her a verified authenticator app.
struct Fixture {
    svc: CognitoService,
    pool_id: String,
    client_id: String,
    totp_secret: Option<String>,
}

fn fixture(tier: &str, totp: bool) -> Fixture {
    let svc = svc();
    let pool_id = create_pool(&svc, json!({ "UserPoolTier": tier })).unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let client_id = json_of(
        &svc.create_user_pool_client(&req(
            "CreateUserPoolClient",
            json!({
                "UserPoolId": pool_id,
                "ClientName": "app",
                "ExplicitAuthFlows": ["ALLOW_USER_AUTH", "ALLOW_REFRESH_TOKEN_AUTH"],
            }),
        ))
        .unwrap(),
    )["UserPoolClient"]["ClientId"]
        .as_str()
        .unwrap()
        .to_string();
    block_on(svc.admin_create_user(&req(
        "AdminCreateUser",
        json!({ "UserPoolId": pool_id, "Username": "alice", "MessageAction": "SUPPRESS" }),
    )))
    .unwrap();
    svc.admin_set_user_password(&req(
        "AdminSetUserPassword",
        json!({ "UserPoolId": pool_id, "Username": "alice", "Password": PASSWORD,
                "Permanent": true }),
    ))
    .unwrap();
    let totp_secret = totp.then(|| {
        let secret = generate_totp_secret();
        let mut accounts = svc.state.write();
        let user = accounts
            .get_or_create("123456789012")
            .users
            .get_mut(&pool_id)
            .unwrap()
            .get_mut("alice")
            .unwrap();
        user.totp_secret = Some(secret.clone());
        user.totp_verified = true;
        secret
    });
    Fixture {
        svc,
        pool_id,
        client_id,
        totp_secret,
    }
}

impl Fixture {
    fn initiate(&self, params: Value) -> Result<AwsResponse, AwsServiceError> {
        let mut auth = json!({ "USERNAME": "alice" });
        for (k, v) in params.as_object().unwrap() {
            auth[k] = v.clone();
        }
        block_on(self.svc.initiate_auth(&req(
            "InitiateAuth",
            json!({ "ClientId": self.client_id, "AuthFlow": "USER_AUTH", "AuthParameters": auth }),
        )))
    }

    fn respond(&self, challenge: &str, session: &str, responses: Value) -> Value {
        let mut r = json!({ "USERNAME": "alice" });
        for (k, v) in responses.as_object().unwrap() {
            r[k] = v.clone();
        }
        json_of(
            &block_on(self.svc.respond_to_auth_challenge(&req(
                "RespondToAuthChallenge",
                json!({ "ClientId": self.client_id, "ChallengeName": challenge,
                        "Session": session, "ChallengeResponses": r }),
            )))
            .unwrap(),
        )
    }

    /// Answer a SELECT_CHALLENGE with the password.
    fn answer_password(&self, select: &Value) -> Value {
        assert_eq!(select["ChallengeName"], "SELECT_CHALLENGE", "{select}");
        self.respond(
            "SELECT_CHALLENGE",
            select["Session"].as_str().unwrap(),
            json!({ "ANSWER": "PASSWORD", "PASSWORD": PASSWORD }),
        )
    }

    fn answer_totp(&self, challenge: &Value) -> Value {
        assert_eq!(
            challenge["ChallengeName"], "SOFTWARE_TOKEN_MFA",
            "{challenge}"
        );
        let code = crate::totp::compute_totp_now(self.totp_secret.as_ref().unwrap()).unwrap();
        self.respond(
            "SOFTWARE_TOKEN_MFA",
            challenge["Session"].as_str().unwrap(),
            json!({ "SOFTWARE_TOKEN_MFA_CODE": code }),
        )
    }
}

#[test]
fn acr_configuration_reports_effective_names_and_validates() {
    let svc = svc();
    // Every pool reports all four levels, defaults merged in.
    let pool = create_pool(&svc, json!({})).unwrap();
    assert_eq!(
        pool["AcrConfiguration"],
        json!({
            "Level1": {"AcrValue": "urn:cognito:loa:1"},
            "Level2": {"AcrValue": "urn:cognito:loa:2"},
            "Level3": {"AcrValue": "urn:cognito:loa:3"},
            "Level4": {"AcrValue": "urn:cognito:loa:4"},
        })
    );

    let pool = create_pool(
        &svc,
        json!({ "AcrConfiguration": {
            "Level2": {"AcrValue": "urn:my:loa:2"},
            "Level3": {"AcrValue": "urn:my:loa:3"},
        } }),
    )
    .unwrap();
    let pool_id = pool["Id"].as_str().unwrap().to_string();
    let expected = json!({
        "Level1": {"AcrValue": "urn:cognito:loa:1"},
        "Level2": {"AcrValue": "urn:my:loa:2"},
        "Level3": {"AcrValue": "urn:my:loa:3"},
        "Level4": {"AcrValue": "urn:cognito:loa:4"},
    });
    assert_eq!(pool["AcrConfiguration"], expected);
    assert_eq!(describe_pool(&svc, &pool_id)["AcrConfiguration"], expected);

    // UpdateUserPool replaces the custom names: Level2/3 go back to defaults.
    svc.update_user_pool(&req(
        "UpdateUserPool",
        json!({ "UserPoolId": pool_id,
                "AcrConfiguration": {"Level4": {"AcrValue": "https://example.com/gold"}} }),
    ))
    .unwrap();
    assert_eq!(
        describe_pool(&svc, &pool_id)["AcrConfiguration"],
        json!({
            "Level1": {"AcrValue": "urn:cognito:loa:1"},
            "Level2": {"AcrValue": "urn:cognito:loa:2"},
            "Level3": {"AcrValue": "urn:cognito:loa:3"},
            "Level4": {"AcrValue": "https://example.com/gold"},
        })
    );
    // An update that leaves AcrConfiguration out keeps the names.
    svc.update_user_pool(&req(
        "UpdateUserPool",
        json!({ "UserPoolId": pool_id, "MfaConfiguration": "OFF" }),
    ))
    .unwrap();
    assert_eq!(
        describe_pool(&svc, &pool_id)["AcrConfiguration"]["Level4"]["AcrValue"],
        "https://example.com/gold"
    );

    // A name colliding with an uncustomized level's default is rejected and
    // leaves the pool unchanged.
    assert_eq!(
        err_code(svc.update_user_pool(&req(
            "UpdateUserPool",
            json!({ "UserPoolId": pool_id, "MfaConfiguration": "ON",
                    "AcrConfiguration": {"Level1": {"AcrValue": "urn:cognito:loa:2"}} }),
        ))),
        "InvalidParameterException"
    );
    let pool = describe_pool(&svc, &pool_id);
    assert_eq!(pool["MfaConfiguration"], "OFF");
    assert_eq!(
        pool["AcrConfiguration"]["Level4"]["AcrValue"],
        "https://example.com/gold"
    );
    assert_eq!(
        create_pool(
            &svc,
            json!({ "AcrConfiguration": {"Level9": {"AcrValue": "x"}} })
        )
        .unwrap_err()
        .code(),
        "InvalidParameterException"
    );
}

#[test]
fn acr_configuration_needs_essentials_or_plus() {
    let svc = svc();
    let custom = json!({ "Level4": {"AcrValue": "urn:my:loa:4"} });
    assert_eq!(
        create_pool(
            &svc,
            json!({ "UserPoolTier": "LITE", "AcrConfiguration": custom })
        )
        .unwrap_err()
        .code(),
        "FeatureUnavailableInTierException"
    );
    // A Lite pool can still send an empty configuration (all defaults).
    let lite = create_pool(
        &svc,
        json!({ "UserPoolTier": "LITE", "AcrConfiguration": {} }),
    )
    .unwrap();
    let lite_id = lite["Id"].as_str().unwrap();
    assert_eq!(
        err_code(svc.update_user_pool(&req(
            "UpdateUserPool",
            json!({ "UserPoolId": lite_id, "AcrConfiguration": custom }),
        ))),
        "FeatureUnavailableInTierException"
    );
    // Upgrading in the same request makes the custom names allowed.
    svc.update_user_pool(&req(
        "UpdateUserPool",
        json!({ "UserPoolId": lite_id, "UserPoolTier": "PLUS", "AcrConfiguration": custom }),
    ))
    .unwrap();
    // ...and a downgrade to Lite while custom names are set is refused.
    assert_eq!(
        err_code(svc.update_user_pool(&req(
            "UpdateUserPool",
            json!({ "UserPoolId": lite_id, "UserPoolTier": "LITE" }),
        ))),
        "FeatureUnavailableInTierException"
    );
    assert_eq!(describe_pool(&svc, lite_id)["UserPoolTier"], "PLUS");
}

#[test]
fn identity_provider_acr_mapping_round_trips_for_oidc_only() {
    let svc = svc();
    let pool_id = create_pool(&svc, json!({})).unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let oidc_details = json!({
        "client_id": "c", "client_secret": "s", "attributes_request_method": "GET",
        "oidc_issuer": "https://idp.example.com", "authorize_scopes": "openid",
    });
    let created = json_of(
        &svc.create_identity_provider(&req(
            "CreateIdentityProvider",
            json!({ "UserPoolId": pool_id, "ProviderName": "corp", "ProviderType": "OIDC",
                    "ProviderDetails": oidc_details,
                    "AcrMapping": {"Level1": "silver", "Level4": "gold"} }),
        ))
        .unwrap(),
    );
    assert_eq!(
        created["IdentityProvider"]["AcrMapping"],
        json!({"Level1": "silver", "Level4": "gold"})
    );
    let describe = |svc: &CognitoService| {
        json_of(
            &svc.describe_identity_provider(&req(
                "DescribeIdentityProvider",
                json!({ "UserPoolId": pool_id, "ProviderName": "corp" }),
            ))
            .unwrap(),
        )["IdentityProvider"]
            .clone()
    };
    assert_eq!(describe(&svc)["AcrMapping"]["Level4"], "gold");

    // Update without AcrMapping keeps it; with one replaces it.
    svc.update_identity_provider(&req(
        "UpdateIdentityProvider",
        json!({ "UserPoolId": pool_id, "ProviderName": "corp",
                "AttributeMapping": {"email": "email"} }),
    ))
    .unwrap();
    assert_eq!(describe(&svc)["AcrMapping"]["Level1"], "silver");
    let updated = json_of(
        &svc.update_identity_provider(&req(
            "UpdateIdentityProvider",
            json!({ "UserPoolId": pool_id, "ProviderName": "corp",
                    "AcrMapping": {"Level3": "urn:idp:mfa"} }),
        ))
        .unwrap(),
    );
    assert_eq!(
        updated["IdentityProvider"]["AcrMapping"],
        json!({"Level3": "urn:idp:mfa"})
    );

    // Only OIDC providers take an ACR mapping, and keys are Level1-Level4.
    assert_eq!(
        err_code(svc.create_identity_provider(&req(
            "CreateIdentityProvider",
            json!({ "UserPoolId": pool_id, "ProviderName": "Google", "ProviderType": "Google",
                    "ProviderDetails": {"client_id": "c", "client_secret": "s",
                                        "authorize_scopes": "openid"},
                    "AcrMapping": {"Level1": "x"} }),
        ))),
        "InvalidParameterException"
    );
    assert_eq!(
        err_code(svc.update_identity_provider(&req(
            "UpdateIdentityProvider",
            json!({ "UserPoolId": pool_id, "ProviderName": "corp",
                    "AcrMapping": {"Gold": "x"} }),
        ))),
        "InvalidParameterException"
    );
}

#[test]
fn sign_in_without_target_has_no_acr_claims() {
    let f = fixture("ESSENTIALS", false);
    let select = json_of(&f.initiate(json!({})).unwrap());
    let auth = f.answer_password(&select);
    let id = claims(auth["AuthenticationResult"]["IdToken"].as_str().unwrap());
    assert!(id.get("acr").is_none());
    assert!(id.get("amr").is_none());
    let access = claims(
        auth["AuthenticationResult"]["AccessToken"]
            .as_str()
            .unwrap(),
    );
    assert!(access.get("acr").is_none());
    assert!(access["auth_time"].is_number());
}

#[test]
fn target_level_four_requires_password_and_totp() {
    let f = fixture("ESSENTIALS", true);
    let select = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4" }))
            .unwrap(),
    );
    // The pool has MFA OFF, yet reaching level 4 needs the TOTP factor.
    let mfa = f.answer_password(&select);
    let auth = f.answer_totp(&mfa);
    let result = &auth["AuthenticationResult"];
    for token in ["IdToken", "AccessToken"] {
        let c = claims(result[token].as_str().unwrap());
        assert_eq!(c["acr"], "urn:cognito:loa:4", "{token}");
        assert_eq!(c["amr"], json!(["pwd", "otp", "mfa"]), "{token}");
    }
    let auth_time = claims(result["IdToken"].as_str().unwrap())["auth_time"].clone();

    // A refresh carries acr / amr / auth_time over unchanged.
    let refreshed = json_of(
        &block_on(f.svc.initiate_auth(&req(
            "InitiateAuth",
            json!({ "ClientId": f.client_id, "AuthFlow": "REFRESH_TOKEN_AUTH",
                    "AuthParameters": {"REFRESH_TOKEN": result["RefreshToken"]} }),
        )))
        .unwrap(),
    );
    let id = claims(
        refreshed["AuthenticationResult"]["IdToken"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(id["acr"], "urn:cognito:loa:4");
    assert_eq!(id["amr"], json!(["pwd", "otp", "mfa"]));
    assert_eq!(id["auth_time"], auth_time);
}

#[test]
fn target_falls_back_to_a_level_the_user_can_reach() {
    let f = fixture("ESSENTIALS", false);
    // No TOTP: level 4 is out of reach, so the request falls back to level 1.
    let select = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4 urn:cognito:loa:1" }))
            .unwrap(),
    );
    let auth = f.answer_password(&select);
    let id = claims(auth["AuthenticationResult"]["IdToken"].as_str().unwrap());
    assert_eq!(id["acr"], "urn:cognito:loa:1");
    assert_eq!(id["amr"], json!(["pwd"]));

    // Nothing reachable, or nothing recognized: an error.
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4" }))),
        "InvalidParameterException"
    );
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:other:gold" }))),
        "InvalidParameterException"
    );
}

#[test]
fn mfa_stays_a_floor_below_the_target() {
    let f = fixture("ESSENTIALS", true);
    f.svc
        .update_user_pool(&req(
            "UpdateUserPool",
            json!({ "UserPoolId": f.pool_id, "MfaConfiguration": "ON" }),
        ))
        .unwrap();
    let select = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:1" }))
            .unwrap(),
    );
    // Level 1 alone needs no TOTP, but the pool requires MFA; the user ends
    // up at the level the completed factors reach.
    let mfa = f.answer_password(&select);
    let auth = f.answer_totp(&mfa);
    let id = claims(auth["AuthenticationResult"]["IdToken"].as_str().unwrap());
    assert_eq!(id["acr"], "urn:cognito:loa:4");
}

#[test]
fn step_up_from_an_access_token_asks_only_for_the_missing_factor() {
    let f = fixture("ESSENTIALS", true);
    let select = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:1" }))
            .unwrap(),
    );
    let first = f.answer_password(&select);
    let level1_token = first["AuthenticationResult"]["AccessToken"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(claims(&level1_token)["acr"], "urn:cognito:loa:1");

    // Stepping up from the level-1 token skips the password.
    let mfa = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                            "ACCESS_TOKEN": level1_token }))
            .unwrap(),
    );
    let auth = f.answer_totp(&mfa);
    let level4_token = auth["AuthenticationResult"]["AccessToken"]
        .as_str()
        .unwrap()
        .to_string();
    let c = claims(&level4_token);
    assert_eq!(c["acr"], "urn:cognito:loa:4");
    assert_eq!(c["amr"], json!(["pwd", "otp", "mfa"]));

    // A token that already meets the target is refused, not re-issued.
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                                    "ACCESS_TOKEN": level4_token }))),
        "InvalidParameterException"
    );

    // MAX_AGE: a sign-in older than the window earns no credit, so the user
    // authenticates from scratch toward the target.
    {
        let mut accounts = f.svc.state.write();
        let data = accounts
            .get_or_create("123456789012")
            .access_tokens
            .get_mut(&level1_token)
            .unwrap();
        data.auth_context.as_mut().unwrap().auth_time -= 600;
    }
    let fresh = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                            "ACCESS_TOKEN": level1_token, "MAX_AGE": "60" }))
            .unwrap(),
    );
    assert_eq!(fresh["ChallengeName"], "SELECT_CHALLENGE");
    // Within the window the credit applies.
    let credited = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                            "ACCESS_TOKEN": level1_token, "MAX_AGE": "3600" }))
            .unwrap(),
    );
    assert_eq!(credited["ChallengeName"], "SOFTWARE_TOKEN_MFA");

    // ACCESS_TOKEN needs TARGET_ACR_VALUES; MAX_AGE must be non-negative.
    assert_eq!(
        err_code(f.initiate(json!({ "ACCESS_TOKEN": level1_token }))),
        "InvalidParameterException"
    );
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                                    "ACCESS_TOKEN": level1_token, "MAX_AGE": "-1" }))),
        "InvalidParameterException"
    );
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4",
                                    "ACCESS_TOKEN": "not-a-token" }))),
        "NotAuthorizedException"
    );
}

#[test]
fn step_up_uses_the_pool_level_names() {
    let f = fixture("PLUS", true);
    f.svc
        .update_user_pool(&req(
            "UpdateUserPool",
            json!({ "UserPoolId": f.pool_id,
                    "AcrConfiguration": {"Level4": {"AcrValue": "https://example.com/gold"}} }),
        ))
        .unwrap();
    // The default name of a renamed level is no longer recognized.
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:4" }))),
        "InvalidParameterException"
    );
    let select = json_of(
        &f.initiate(json!({ "TARGET_ACR_VALUES": "https://example.com/gold" }))
            .unwrap(),
    );
    let auth = f.answer_totp(&f.answer_password(&select));
    let id = claims(auth["AuthenticationResult"]["IdToken"].as_str().unwrap());
    assert_eq!(id["acr"], "https://example.com/gold");
}

#[test]
fn step_up_needs_essentials_or_plus() {
    let f = fixture("LITE", false);
    assert_eq!(
        err_code(f.initiate(json!({ "TARGET_ACR_VALUES": "urn:cognito:loa:1" }))),
        "FeatureUnavailableInTierException"
    );
    // Without a target the Lite pool signs in normally.
    let select = json_of(&f.initiate(json!({})).unwrap());
    assert_eq!(select["ChallengeName"], "SELECT_CHALLENGE");
}

#[test]
fn admin_initiate_auth_runs_user_auth_with_step_up() {
    let f = fixture("ESSENTIALS", true);
    let select = json_of(
        &block_on(f.svc.admin_initiate_auth(&req(
            "AdminInitiateAuth",
            json!({ "UserPoolId": f.pool_id, "ClientId": f.client_id, "AuthFlow": "USER_AUTH",
                    "AuthParameters": {"USERNAME": "alice",
                                       "TARGET_ACR_VALUES": "urn:cognito:loa:4"} }),
        )))
        .unwrap(),
    );
    assert_eq!(select["ChallengeName"], "SELECT_CHALLENGE");
    let admin_respond = |challenge: &str, session: &str, responses: Value| {
        json_of(
            &block_on(f.svc.admin_respond_to_auth_challenge(&req(
                "AdminRespondToAuthChallenge",
                json!({ "UserPoolId": f.pool_id, "ClientId": f.client_id,
                        "ChallengeName": challenge, "Session": session,
                        "ChallengeResponses": responses }),
            )))
            .unwrap(),
        )
    };
    let mfa = admin_respond(
        "SELECT_CHALLENGE",
        select["Session"].as_str().unwrap(),
        json!({ "USERNAME": "alice", "ANSWER": "PASSWORD", "PASSWORD": PASSWORD }),
    );
    assert_eq!(mfa["ChallengeName"], "SOFTWARE_TOKEN_MFA");
    let code = crate::totp::compute_totp_now(f.totp_secret.as_ref().unwrap()).unwrap();
    let auth = admin_respond(
        "SOFTWARE_TOKEN_MFA",
        mfa["Session"].as_str().unwrap(),
        json!({ "USERNAME": "alice", "SOFTWARE_TOKEN_MFA_CODE": code }),
    );
    let id = claims(auth["AuthenticationResult"]["IdToken"].as_str().unwrap());
    assert_eq!(id["acr"], "urn:cognito:loa:4");

    // The client must belong to the pool named in the request.
    let other_pool = create_pool(&f.svc, json!({})).unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        err_code(block_on(f.svc.admin_initiate_auth(&req(
            "AdminInitiateAuth",
            json!({ "UserPoolId": other_pool, "ClientId": f.client_id, "AuthFlow": "USER_AUTH",
                    "AuthParameters": {"USERNAME": "alice"} }),
        )))),
        "ResourceNotFoundException"
    );
}
