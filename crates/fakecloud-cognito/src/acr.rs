//! Authentication context class reference (ACR) levels and authentication
//! methods reference (AMR) values for user pool sign-in.
//!
//! Cognito defines four fixed levels, each satisfied by a fixed combination
//! of factors; a pool can only rename them (`AcrConfiguration`):
//!
//! | Level | Default name        | Satisfying factors                           |
//! |-------|---------------------|----------------------------------------------|
//! | 1     | `urn:cognito:loa:1` | password                                     |
//! | 2     | `urn:cognito:loa:2` | a single SMS or email one-time password      |
//! | 3     | `urn:cognito:loa:3` | a passkey; or password + SMS / email OTP     |
//! | 4     | `urn:cognito:loa:4` | password + TOTP from an authenticator app    |
//!
//! A sign-in is credited with the highest level its completed factors fully
//! satisfy. The `amr` claim lists those factors (`pwd`, `otp`, `sms`,
//! `email_otp`, `swk`, `hwk`) plus `mfa` once two or more distinct factors
//! were completed.

use std::collections::BTreeMap;

use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::AwsServiceError;

/// The `AcrConfiguration` / `AcrMapping` map keys, by level.
pub(crate) const LEVEL_KEYS: [&str; 4] = ["Level1", "Level2", "Level3", "Level4"];

/// Cognito's names for the four levels when a pool does not customize them.
pub(crate) const DEFAULT_ACR_VALUES: [&str; 4] = [
    "urn:cognito:loa:1",
    "urn:cognito:loa:2",
    "urn:cognito:loa:3",
    "urn:cognito:loa:4",
];

/// AMR value for a password-family factor (password or SRP).
pub(crate) const AMR_PWD: &str = "pwd";
/// AMR value for a TOTP from an authenticator app.
pub(crate) const AMR_OTP: &str = "otp";
/// AMR value for a one-time code delivered by SMS.
pub(crate) const AMR_SMS: &str = "sms";
/// AMR value for a one-time code delivered by email.
pub(crate) const AMR_EMAIL_OTP: &str = "email_otp";
/// AMR value for a passkey on a platform authenticator.
pub(crate) const AMR_SWK: &str = "swk";
/// AMR value for a passkey on a roaming authenticator.
pub(crate) const AMR_HWK: &str = "hwk";
/// AMR value added once two or more distinct factors were completed.
pub(crate) const AMR_MFA: &str = "mfa";

fn invalid(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "InvalidParameterException",
        msg.into(),
    )
}

/// `FeatureUnavailableInTierException`: the feature needs a higher feature
/// plan than the pool's `UserPoolTier`.
pub(crate) fn feature_unavailable(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "FeatureUnavailableInTierException",
        msg.into(),
    )
}

/// Custom ACR level names and step-up authentication need the Essentials or
/// Plus feature plan.
pub(crate) fn tier_supports_acr(tier: &str) -> bool {
    !tier.eq_ignore_ascii_case("LITE")
}

/// Validate a `Level1`..`Level4` map key and return its level (1-4).
fn level_of_key(key: &str, field: &str) -> Result<u8, AwsServiceError> {
    LEVEL_KEYS
        .iter()
        .position(|k| *k == key)
        .map(|i| i as u8 + 1)
        .ok_or_else(|| {
            invalid(format!(
                "1 validation error detected: Value at '{field}' failed to satisfy constraint: \
                 Map keys must satisfy constraint: [Member must satisfy regular expression \
                 pattern: ^Level[1-4]$]"
            ))
        })
}

/// Validate an `AcrValueType`: 1-64 printable ASCII characters other than
/// space, double quote and backslash.
fn validate_acr_value(value: &str, field: &str) -> Result<(), AwsServiceError> {
    let len = value.chars().count();
    if len == 0 {
        return Err(invalid(format!(
            "1 validation error detected: Value '{value}' at '{field}' failed to satisfy \
             constraint: Member must have length greater than or equal to 1"
        )));
    }
    if len > 64 {
        return Err(invalid(format!(
            "1 validation error detected: Value '{value}' at '{field}' failed to satisfy \
             constraint: Member must have length less than or equal to 64"
        )));
    }
    let ok = value
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\');
    if !ok {
        return Err(invalid(format!(
            "1 validation error detected: Value '{value}' at '{field}' failed to satisfy \
             constraint: Member must satisfy regular expression pattern: \
             ^[\\x21\\x23-\\x5B\\x5D-\\x7E]+$"
        )));
    }
    Ok(())
}

fn require_map<'a>(
    v: &'a Value,
    field: &str,
) -> Result<&'a serde_json::Map<String, Value>, AwsServiceError> {
    let map = v
        .as_object()
        .ok_or_else(|| invalid(format!("{field} must be a map of ACR levels.")))?;
    if map.len() > LEVEL_KEYS.len() {
        return Err(invalid(format!(
            "1 validation error detected: Value at '{field}' failed to satisfy constraint: \
             Member must have length less than or equal to 4"
        )));
    }
    Ok(map)
}

/// Parse and validate an `AcrConfiguration` request member into the stored
/// overrides (`Level*` -> custom name). Every effective name, including the
/// defaults of the levels left alone, must be unique.
pub(crate) fn parse_acr_configuration(
    v: &Value,
) -> Result<BTreeMap<String, String>, AwsServiceError> {
    let map = require_map(v, "acrConfiguration")?;
    let mut overrides = BTreeMap::new();
    for (key, entry) in map {
        level_of_key(key, "acrConfiguration")?;
        let field = format!("acrConfiguration.{key}.member.acrValue");
        let value = entry
            .get("AcrValue")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid(format!(
                    "1 validation error detected: Value null at '{field}' failed to satisfy \
                     constraint: Member must not be null"
                ))
            })?;
        validate_acr_value(value, &field)?;
        overrides.insert(key.clone(), value.to_string());
    }
    let names = effective_names(&overrides);
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            return Err(invalid(format!(
                "AcrValue {name} is not unique. Each ACR level of a user pool must have a \
                 unique name, including the default names of levels you don't customize."
            )));
        }
    }
    Ok(overrides)
}

/// Parse and validate an `AcrMapping` request member (`Level*` -> the
/// identity provider's ACR value).
pub(crate) fn parse_acr_mapping(v: &Value) -> Result<BTreeMap<String, String>, AwsServiceError> {
    let map = require_map(v, "acrMapping")?;
    let mut mapping = BTreeMap::new();
    for (key, value) in map {
        level_of_key(key, "acrMapping")?;
        let field = format!("acrMapping.{key}.member");
        let value = value.as_str().ok_or_else(|| {
            invalid(format!(
                "1 validation error detected: Value null at '{field}' failed to satisfy \
                 constraint: Member must not be null"
            ))
        })?;
        validate_acr_value(value, &field)?;
        mapping.insert(key.clone(), value.to_string());
    }
    Ok(mapping)
}

/// The pool's name for each level, `Level1` first.
pub(crate) fn effective_names(overrides: &BTreeMap<String, String>) -> [String; 4] {
    std::array::from_fn(|i| {
        overrides
            .get(LEVEL_KEYS[i])
            .cloned()
            .unwrap_or_else(|| DEFAULT_ACR_VALUES[i].to_string())
    })
}

/// The effective `AcrConfiguration` a pool reports: every level, with the
/// default name for any level the pool does not customize.
pub(crate) fn acr_configuration_json(overrides: &BTreeMap<String, String>) -> Value {
    let names = effective_names(overrides);
    let mut out = serde_json::Map::new();
    for (key, name) in LEVEL_KEYS.iter().zip(names) {
        out.insert(key.to_string(), json!({ "AcrValue": name }));
    }
    Value::Object(out)
}

/// The level (1-4) a pool calls `name`, if any.
pub(crate) fn level_named(overrides: &BTreeMap<String, String>, name: &str) -> Option<u8> {
    effective_names(overrides)
        .iter()
        .position(|n| n == name)
        .map(|i| i as u8 + 1)
}

/// The pool's name for `level` (1-4).
pub(crate) fn level_name(overrides: &BTreeMap<String, String>, level: u8) -> String {
    let i = usize::from(level.clamp(1, 4)) - 1;
    effective_names(overrides)[i].clone()
}

/// The highest level a set of completed factors fully satisfies.
pub(crate) fn level_for_factors(factors: &[String]) -> Option<u8> {
    let has = |f: &str| factors.iter().any(|x| x == f);
    let pwd = has(AMR_PWD);
    let otp_code = has(AMR_SMS) || has(AMR_EMAIL_OTP);
    if pwd && has(AMR_OTP) {
        Some(4)
    } else if has(AMR_SWK) || has(AMR_HWK) || (pwd && otp_code) {
        Some(3)
    } else if otp_code {
        Some(2)
    } else if pwd {
        Some(1)
    } else {
        None
    }
}

/// The `amr` claim for a set of completed factors: each distinct factor once,
/// in completion order, plus `mfa` when two or more were completed.
pub(crate) fn amr_claim(factors: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in factors {
        if f != AMR_MFA && !out.contains(f) {
            out.push(f.clone());
        }
    }
    if out.len() >= 2 {
        out.push(AMR_MFA.to_string());
    }
    out
}

/// The factors an `amr` claim records (the claim minus `mfa`).
pub(crate) fn factors_of_amr(amr: &[String]) -> Vec<String> {
    amr.iter().filter(|f| *f != AMR_MFA).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn levels_follow_the_fixed_factor_table() {
        assert_eq!(level_for_factors(&s(&["pwd"])), Some(1));
        assert_eq!(level_for_factors(&s(&["sms"])), Some(2));
        assert_eq!(level_for_factors(&s(&["email_otp"])), Some(2));
        assert_eq!(level_for_factors(&s(&["swk"])), Some(3));
        assert_eq!(level_for_factors(&s(&["hwk"])), Some(3));
        assert_eq!(level_for_factors(&s(&["pwd", "sms"])), Some(3));
        assert_eq!(level_for_factors(&s(&["pwd", "email_otp"])), Some(3));
        assert_eq!(level_for_factors(&s(&["pwd", "otp"])), Some(4));
        assert_eq!(level_for_factors(&s(&["otp"])), None);
        assert_eq!(level_for_factors(&[]), None);
    }

    #[test]
    fn amr_adds_mfa_for_two_factors() {
        assert_eq!(amr_claim(&s(&["pwd"])), s(&["pwd"]));
        assert_eq!(amr_claim(&s(&["pwd", "otp"])), s(&["pwd", "otp", "mfa"]));
        assert_eq!(
            amr_claim(&s(&["pwd", "pwd", "mfa", "sms"])),
            s(&["pwd", "sms", "mfa"])
        );
        assert_eq!(
            factors_of_amr(&s(&["pwd", "otp", "mfa"])),
            s(&["pwd", "otp"])
        );
    }

    #[test]
    fn configuration_merges_defaults_and_validates() {
        let overrides = parse_acr_configuration(&json!({
            "Level2": {"AcrValue": "urn:my:loa:2"},
            "Level3": {"AcrValue": "urn:my:loa:3"},
        }))
        .unwrap();
        assert_eq!(
            acr_configuration_json(&overrides),
            json!({
                "Level1": {"AcrValue": "urn:cognito:loa:1"},
                "Level2": {"AcrValue": "urn:my:loa:2"},
                "Level3": {"AcrValue": "urn:my:loa:3"},
                "Level4": {"AcrValue": "urn:cognito:loa:4"},
            })
        );
        assert_eq!(level_named(&overrides, "urn:my:loa:3"), Some(3));
        assert_eq!(level_named(&overrides, "urn:cognito:loa:3"), None);
        assert_eq!(level_name(&overrides, 4), "urn:cognito:loa:4");

        let code = |v: Value| parse_acr_configuration(&v).unwrap_err().code().to_string();
        // A name that collides with an uncustomized level's default.
        assert_eq!(
            code(json!({"Level1": {"AcrValue": "urn:cognito:loa:4"}})),
            "InvalidParameterException"
        );
        assert_eq!(
            code(json!({"Level5": {"AcrValue": "x"}})),
            "InvalidParameterException"
        );
        assert_eq!(
            code(json!({"Level1": {"AcrValue": "has space"}})),
            "InvalidParameterException"
        );
        assert_eq!(
            code(json!({"Level1": {"AcrValue": "a\"b"}})),
            "InvalidParameterException"
        );
        assert_eq!(
            code(json!({"Level1": {"AcrValue": "x".repeat(65)}})),
            "InvalidParameterException"
        );
        assert_eq!(code(json!({"Level1": {}})), "InvalidParameterException");
    }

    #[test]
    fn mapping_validates_keys_and_values() {
        let m = parse_acr_mapping(&json!({"Level1": "silver", "Level4": "gold"})).unwrap();
        assert_eq!(m.get("Level4").map(String::as_str), Some("gold"));
        assert!(parse_acr_mapping(&json!({"Gold": "x"})).is_err());
        assert!(parse_acr_mapping(&json!({"Level1": ""})).is_err());
    }
}
