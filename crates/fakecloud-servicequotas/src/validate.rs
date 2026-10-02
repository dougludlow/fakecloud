//! Input validation against the Service Quotas Smithy model constraints.
//!
//! Every Service Quotas operation that takes input declares
//! `IllegalArgumentException`, which is what AWS returns when a member breaks
//! its `@required`, `@length`, `@pattern` or `@range` constraint. The message
//! follows the AWS validation format.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use http::StatusCode;
use regex::Regex;
use serde_json::Value;

use fakecloud_core::service::AwsServiceError;

pub fn illegal_argument(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "IllegalArgumentException",
        msg.into(),
    )
}

fn constraint_error(value: &str, member: &str, constraint: &str) -> AwsServiceError {
    illegal_argument(format!(
        "1 validation error detected: Value '{value}' at '{}' failed to satisfy constraint: \
         {constraint}",
        lower_first(member)
    ))
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

/// A string member's model constraints.
pub struct StrRule {
    pub min: usize,
    pub max: usize,
    pub pattern: Option<&'static str>,
}

pub const SERVICE_CODE: StrRule = StrRule {
    min: 1,
    max: 63,
    pattern: Some("^[a-zA-Z][a-zA-Z0-9-]{1,63}$"),
};
pub const QUOTA_CODE: StrRule = StrRule {
    min: 1,
    max: 128,
    pattern: Some("^[a-zA-Z][a-zA-Z0-9-]{1,128}$"),
};
pub const AWS_REGION: StrRule = StrRule {
    min: 1,
    max: 64,
    pattern: Some("^[a-zA-Z][a-zA-Z0-9-]{1,128}$"),
};
pub const REQUEST_ID: StrRule = StrRule {
    min: 1,
    max: 128,
    pattern: Some("^[0-9a-zA-Z][a-zA-Z0-9-]{1,128}$"),
};
pub const NEXT_TOKEN: StrRule = StrRule {
    min: 0,
    max: 2048,
    pattern: Some("^[a-zA-Z0-9/+]*={0,2}$"),
};
pub const AMAZON_RESOURCE_NAME: StrRule = StrRule {
    min: 1,
    max: 1011,
    pattern: Some(r"^arn:aws(-[\w]+)*:*:.+:[0-9]{12}:.+$"),
};
pub const TAG_KEY: StrRule = StrRule {
    min: 1,
    max: 128,
    pattern: Some(r"^([\p{L}\p{Z}\p{N}_.:/=+\-@]*)$"),
};
pub const TAG_VALUE: StrRule = StrRule {
    min: 0,
    max: 256,
    pattern: Some(r"^([\p{L}\p{Z}\p{N}_.:/=+\-@]*)$"),
};
pub const EXCLUDED_SERVICE: StrRule = StrRule {
    min: 1,
    max: 128,
    pattern: Some("^[A-Za-z0-9-_ /]{1,128}$"),
};

/// Whether `value` matches a model `@pattern`. Compiled patterns are cached;
/// there are only a handful.
fn matches(pattern: &'static str, value: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Regex>>> = OnceLock::new();
    let re = CACHE
        .get_or_init(Default::default)
        .lock()
        .entry(pattern)
        .or_insert_with(|| Regex::new(pattern).expect("model pattern compiles"))
        .clone();
    re.is_match(value)
}

/// Check a string value against `rule`.
pub fn check_str(member: &str, value: &str, rule: &StrRule) -> Result<(), AwsServiceError> {
    let n = value.chars().count();
    if n < rule.min || n > rule.max {
        return Err(constraint_error(
            value,
            member,
            &format!(
                "Member must have length between {} and {}, inclusive",
                rule.min, rule.max
            ),
        ));
    }
    if let Some(p) = rule.pattern {
        if !matches(p, value) {
            return Err(constraint_error(
                value,
                member,
                &format!("Member must satisfy regular expression pattern: {p}"),
            ));
        }
    }
    Ok(())
}

fn type_error(member: &str, expected: &str) -> AwsServiceError {
    illegal_argument(format!(
        "1 validation error detected: Value at '{}' failed to satisfy constraint: Member must \
         be a {expected}",
        lower_first(member)
    ))
}

/// A required string member.
pub fn req_str<'a>(b: &'a Value, member: &str, rule: &StrRule) -> Result<&'a str, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Err(illegal_argument(format!(
            "1 validation error detected: Value null at '{}' failed to satisfy constraint: \
             Member must not be null",
            lower_first(member)
        ))),
        Some(Value::String(s)) => {
            check_str(member, s, rule)?;
            Ok(s)
        }
        Some(_) => Err(type_error(member, "string")),
    }
}

/// An optional string member.
pub fn opt_str<'a>(
    b: &'a Value,
    member: &str,
    rule: &StrRule,
) -> Result<Option<&'a str>, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            check_str(member, s, rule)?;
            Ok(Some(s))
        }
        Some(_) => Err(type_error(member, "string")),
    }
}

/// An optional unconstrained string member.
pub fn opt_plain_str<'a>(b: &'a Value, member: &str) -> Result<Option<&'a str>, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(type_error(member, "string")),
    }
}

/// An optional enum member.
pub fn opt_enum<'a>(
    b: &'a Value,
    member: &str,
    allowed: &[&str],
) -> Result<Option<&'a str>, AwsServiceError> {
    let Some(v) = opt_plain_str(b, member)? else {
        return Ok(None);
    };
    if !allowed.contains(&v) {
        return Err(constraint_error(
            v,
            member,
            &format!(
                "Member must satisfy enum value set: [{}]",
                allowed.join(", ")
            ),
        ));
    }
    Ok(Some(v))
}

/// A required enum member.
pub fn req_enum<'a>(
    b: &'a Value,
    member: &str,
    allowed: &[&str],
) -> Result<&'a str, AwsServiceError> {
    opt_enum(b, member, allowed)?.ok_or_else(|| {
        illegal_argument(format!(
            "1 validation error detected: Value null at '{}' failed to satisfy constraint: \
             Member must not be null",
            lower_first(member)
        ))
    })
}

/// An optional integer member bounded by `@range`.
pub fn opt_int(
    b: &Value,
    member: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_i64().ok_or_else(|| type_error(member, "integer"))?;
            if n < min || n > max {
                return Err(constraint_error(
                    &n.to_string(),
                    member,
                    &format!("Member must have value between {min} and {max}, inclusive"),
                ));
            }
            Ok(Some(n))
        }
    }
}

/// A required double member bounded by `@range`.
pub fn req_double(b: &Value, member: &str, min: f64, max: f64) -> Result<f64, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Err(illegal_argument(format!(
            "1 validation error detected: Value null at '{}' failed to satisfy constraint: \
             Member must not be null",
            lower_first(member)
        ))),
        Some(v) => {
            let n = v.as_f64().ok_or_else(|| type_error(member, "number"))?;
            if !(min..=max).contains(&n) {
                return Err(constraint_error(
                    &n.to_string(),
                    member,
                    &format!("Member must have value between {min} and {max}, inclusive"),
                ));
            }
            Ok(n)
        }
    }
}

/// An optional boolean member.
pub fn opt_bool(b: &Value, member: &str) -> Result<Option<bool>, AwsServiceError> {
    match b.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(v)) => Ok(Some(*v)),
        Some(_) => Err(type_error(member, "boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_code_pattern_and_length() {
        assert!(check_str("ServiceCode", "ec2", &SERVICE_CODE).is_ok());
        assert!(check_str("ServiceCode", "e", &SERVICE_CODE).is_err());
        assert!(check_str("ServiceCode", "1ec2", &SERVICE_CODE).is_err());
        let err = check_str("ServiceCode", &"a".repeat(64), &SERVICE_CODE).unwrap_err();
        assert_eq!(err.code(), "IllegalArgumentException");
        assert!(err.message().contains("'serviceCode'"), "{}", err.message());
    }

    #[test]
    fn missing_required_member_is_illegal_argument() {
        let err = req_str(&json!({}), "QuotaCode", &QUOTA_CODE).unwrap_err();
        assert_eq!(err.code(), "IllegalArgumentException");
        assert!(err.message().contains("must not be null"));
    }

    #[test]
    fn ranges_and_enums() {
        assert!(opt_int(&json!({"MaxResults": 0}), "MaxResults", 1, 100).is_err());
        assert_eq!(
            opt_int(&json!({"MaxResults": 7}), "MaxResults", 1, 100).unwrap(),
            Some(7)
        );
        assert!(req_double(&json!({"DesiredValue": -1}), "DesiredValue", 0.0, 1e10).is_err());
        assert!(opt_enum(&json!({"Status": "NOPE"}), "Status", &["PENDING"]).is_err());
    }

    #[test]
    fn tag_patterns_accept_unicode_letters() {
        assert!(check_str("Key", "équipe:owner", &TAG_KEY).is_ok());
        assert!(check_str("Key", "bad*key", &TAG_KEY).is_err());
    }
}
