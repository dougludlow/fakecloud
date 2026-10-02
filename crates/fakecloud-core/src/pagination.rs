/// Offset-based pagination helper for AWS list operations.
///
/// Parses `next_token` as a numeric offset (defaulting to 0 if `None` or unparseable),
/// slices `items` starting at that offset, and returns at most `max_results` items
/// along with an optional next token for the following page.
///
/// Prefer [`paginate_checked`] for client-facing list ops: this variant treats a
/// malformed `next_token` as offset 0, which can drive an infinite client
/// pagination loop. It remains for callers whose service model declares no
/// invalid-token error (returning one would be an undeclared error).
#[must_use]
pub fn paginate<T: Clone>(
    items: &[T],
    next_token: Option<&str>,
    max_results: usize,
) -> (Vec<T>, Option<String>) {
    if max_results == 0 {
        return (Vec::new(), None);
    }
    let offset: usize = next_token.and_then(|s| s.parse().ok()).unwrap_or(0);
    let page = if offset < items.len() {
        &items[offset..]
    } else {
        &[][..]
    };
    let has_more = page.len() > max_results;
    let result: Vec<T> = page.iter().take(max_results).cloned().collect();
    let token = if has_more {
        Some((offset + max_results).to_string())
    } else {
        None
    };
    (result, token)
}

/// Error from [`paginate_checked`]: `next_token` was present but is not a valid
/// offset token (not produced by a prior page of the same list op). AWS rejects
/// such tokens with `InvalidNextToken` (or a service-specific equivalent);
/// callers map this to their wire error (bug-audit 2026-05-28, 1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidNextToken;

/// Strict variant of [`paginate`]: a `next_token` that is present but does not
/// parse as a non-negative offset is rejected with [`InvalidNextToken`] instead
/// of being silently treated as offset 0 (which can drive an infinite client
/// pagination loop). `None` still means "first page".
pub fn paginate_checked<T: Clone>(
    items: &[T],
    next_token: Option<&str>,
    max_results: usize,
) -> Result<(Vec<T>, Option<String>), InvalidNextToken> {
    let offset: usize = match next_token {
        None => 0,
        Some(tok) => tok.parse().map_err(|_| InvalidNextToken)?,
    };
    if max_results == 0 {
        return Ok((Vec::new(), None));
    }
    let page = if offset < items.len() {
        &items[offset..]
    } else {
        &[][..]
    };
    let has_more = page.len() > max_results;
    let result: Vec<T> = page.iter().take(max_results).cloned().collect();
    let token = if has_more {
        Some((offset + max_results).to_string())
    } else {
        None
    };
    Ok((result, token))
}

/// Parse a client-supplied offset token (as minted by [`paginate`] /
/// [`paginate_checked`] / [`page_json_response`]). Absent or empty is offset 0.
pub fn parse_offset_token(token: Option<&str>) -> Result<usize, InvalidNextToken> {
    match token.filter(|t| !t.is_empty()) {
        None => Ok(0),
        Some(t) => t.parse().map_err(|_| InvalidNextToken),
    }
}

/// Page a successful JSON list response in place: the array at `items` is
/// sliced to `[start, start + size)` (`size` of `None` keeps everything from
/// `start`), and `token_key` is set to the offset token of the next page when
/// items remain, or removed when none do (AWS omits an exhausted token rather
/// than sending it empty). Responses that are not JSON objects with an array
/// at `items` are returned unchanged.
///
/// This lets a handler render its full, stably ordered listing and a service
/// apply `MaxResults` / `NextToken` uniformly at its dispatch boundary.
pub fn page_json_response(
    resp: crate::service::AwsResponse,
    items: &str,
    token_key: &str,
    start: usize,
    size: Option<usize>,
) -> crate::service::AwsResponse {
    use crate::service::ResponseBody;
    if !resp.status.is_success() {
        return resp;
    }
    let ResponseBody::Bytes(bytes) = &resp.body else {
        return resp;
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return resp;
    };
    let Some(obj) = value.as_object_mut() else {
        return resp;
    };
    let Some(list) = obj.get_mut(items).and_then(|v| v.as_array_mut()) else {
        return resp;
    };
    let total = list.len();
    let start = start.min(total);
    let end = size.map_or(total, |n| start.saturating_add(n).min(total));
    let page: Vec<serde_json::Value> = list.drain(start..end).collect();
    *list = page;
    if end < total {
        obj.insert(
            token_key.to_string(),
            serde_json::Value::String(end.to_string()),
        );
    } else {
        obj.remove(token_key);
    }
    crate::service::AwsResponse {
        body: ResponseBody::Bytes(bytes::Bytes::from(
            serde_json::to_vec(&value).expect("serde_json::Value serialization is infallible"),
        )),
        ..resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_body(resp: &crate::service::AwsResponse) -> serde_json::Value {
        match &resp.body {
            crate::service::ResponseBody::Bytes(b) => serde_json::from_slice(b).unwrap(),
            _ => panic!("not bytes"),
        }
    }

    #[test]
    fn page_json_response_slices_and_sets_or_clears_the_token() {
        let full = || {
            crate::service::AwsResponse::ok_json(
                serde_json::json!({"Things": [0, 1, 2, 3, 4], "NextToken": ""}),
            )
        };
        let p1 = json_body(&page_json_response(
            full(),
            "Things",
            "NextToken",
            0,
            Some(2),
        ));
        assert_eq!(p1["Things"], serde_json::json!([0, 1]));
        assert_eq!(p1["NextToken"], "2");
        let p3 = json_body(&page_json_response(
            full(),
            "Things",
            "NextToken",
            4,
            Some(2),
        ));
        assert_eq!(p3["Things"], serde_json::json!([4]));
        assert!(p3.get("NextToken").is_none(), "{p3}");
        let all = json_body(&page_json_response(full(), "Things", "NextToken", 0, None));
        assert_eq!(all["Things"].as_array().unwrap().len(), 5);
        assert!(all.get("NextToken").is_none());
        // A response without the list is left alone.
        let other = json_body(&page_json_response(
            crate::service::AwsResponse::ok_json(serde_json::json!({"X": 1})),
            "Things",
            "NextToken",
            0,
            Some(1),
        ));
        assert_eq!(other, serde_json::json!({"X": 1}));
    }

    #[test]
    fn parse_offset_token_rejects_non_offsets() {
        assert_eq!(parse_offset_token(None), Ok(0));
        assert_eq!(parse_offset_token(Some("")), Ok(0));
        assert_eq!(parse_offset_token(Some("7")), Ok(7));
        assert_eq!(parse_offset_token(Some("abc")), Err(InvalidNextToken));
    }

    #[test]
    fn first_page() {
        let items: Vec<i32> = (0..10).collect();
        let (page, token) = paginate(&items, None, 3);
        assert_eq!(page, vec![0, 1, 2]);
        assert_eq!(token, Some("3".to_string()));
    }

    #[test]
    fn middle_page() {
        let items: Vec<i32> = (0..10).collect();
        let (page, token) = paginate(&items, Some("3"), 3);
        assert_eq!(page, vec![3, 4, 5]);
        assert_eq!(token, Some("6".to_string()));
    }

    #[test]
    fn last_page() {
        let items: Vec<i32> = (0..10).collect();
        let (page, token) = paginate(&items, Some("9"), 3);
        assert_eq!(page, vec![9]);
        assert_eq!(token, None);
    }

    #[test]
    fn exact_page_boundary() {
        let items: Vec<i32> = (0..6).collect();
        let (page, token) = paginate(&items, Some("3"), 3);
        assert_eq!(page, vec![3, 4, 5]);
        assert_eq!(token, None);
    }

    #[test]
    fn offset_beyond_items() {
        let items: Vec<i32> = (0..3).collect();
        let (page, token) = paginate(&items, Some("100"), 3);
        assert!(page.is_empty());
        assert_eq!(token, None);
    }

    #[test]
    fn invalid_token_defaults_to_zero() {
        let items: Vec<i32> = (0..5).collect();
        let (page, token) = paginate(&items, Some("not_a_number"), 3);
        assert_eq!(page, vec![0, 1, 2]);
        assert_eq!(token, Some("3".to_string()));
    }

    #[test]
    fn zero_max_results_returns_empty_page_without_token() {
        // AWS list ops reject MaxResults=0 at the validation layer; if the helper
        // ever sees zero it returns an empty page with no continuation token so
        // callers can't accidentally paginate forever on a non-advancing offset.
        let items: Vec<i32> = (0..5).collect();
        let (page, token) = paginate(&items, None, 0);
        assert!(page.is_empty());
        assert_eq!(token, None);
    }

    #[test]
    fn empty_items() {
        let items: Vec<i32> = vec![];
        let (page, token) = paginate(&items, None, 10);
        assert!(page.is_empty());
        assert_eq!(token, None);
    }

    // bug-audit 2026-05-28, 1.7: paginate_checked rejects a malformed next_token
    // instead of silently treating it as offset 0.
    #[test]
    fn checked_none_is_first_page() {
        let items: Vec<i32> = (0..5).collect();
        let (page, token) = paginate_checked(&items, None, 3).unwrap();
        assert_eq!(page, vec![0, 1, 2]);
        assert_eq!(token, Some("3".to_string()));
    }

    #[test]
    fn checked_valid_token_advances() {
        let items: Vec<i32> = (0..5).collect();
        let (page, token) = paginate_checked(&items, Some("3"), 3).unwrap();
        assert_eq!(page, vec![3, 4]);
        assert_eq!(token, None);
    }

    #[test]
    fn checked_garbage_token_is_rejected() {
        let items: Vec<i32> = (0..5).collect();
        assert_eq!(
            paginate_checked(&items, Some("not_a_number"), 3),
            Err(InvalidNextToken)
        );
    }

    #[test]
    fn checked_negative_token_is_rejected() {
        let items: Vec<i32> = (0..5).collect();
        assert_eq!(
            paginate_checked(&items, Some("-1"), 3),
            Err(InvalidNextToken)
        );
    }
}
