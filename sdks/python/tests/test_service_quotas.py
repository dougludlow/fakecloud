"""Unit tests for the Service Quotas sub-client against a mocked transport.

No fakecloud server is needed: each test serves canned responses through
``httpx.MockTransport`` and asserts on the requests the SDK sends.
"""

from __future__ import annotations

import json
from typing import Any, Callable, Dict, List

import httpx
import pytest

from fakecloud.client import (
    FakeCloudError,
    ServiceQuotasClient,
    _SyncServiceQuotasClient,
)
from fakecloud.types import QuotaEnforcement, ServiceQuotaEnforcementChange

BASE = "http://fc.test"

QUOTA: Dict[str, Any] = {
    "serviceCode": "vpc",
    "quotaCode": "L-0EA8095F",
    "quotaName": "Inbound or outbound rules per security group",
    "global": False,
    "adjustable": True,
    "unit": "None",
    "defaultValue": 5,
    "appliedValue": 2.0,
    "usage": None,
    "enforceable": True,
    "enforced": True,
    "enforcementSource": "override",
}

REQUEST: Dict[str, Any] = {
    "accountId": "123456789012",
    "requestId": "req/1",
    "serviceCode": "vpc",
    "quotaCode": "L-0EA8095F",
    "quotaName": "Inbound or outbound rules per security group",
    "region": "us-east-1",
    "desiredValue": 10,
    "status": "APPROVED",
    "caseId": None,
    "created": "2026-10-05T00:00:00+00:00",
    "lastUpdated": "2026-10-05T00:00:01+00:00",
}

ENFORCEMENT: Dict[str, Any] = {
    "enforceAll": True,
    "overrides": [{"serviceCode": "vpc", "quotaCode": "L-0EA8095F", "enforce": False}],
    "accountOverrides": [
        {
            "accountId": "111111111111",
            "serviceCode": "vpc",
            "quotaCode": "L-0EA8095F",
            "enforce": True,
        }
    ],
}


def _sync(
    responder: Callable[[httpx.Request], httpx.Response],
) -> "tuple[_SyncServiceQuotasClient, List[httpx.Request]]":
    seen: List[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        return responder(req)

    client = httpx.Client(transport=httpx.MockTransport(handler))
    return _SyncServiceQuotasClient(client, BASE), seen


def _ok(body: Any) -> Callable[[httpx.Request], httpx.Response]:
    return lambda _req: httpx.Response(200, json=body)


def _body(req: httpx.Request) -> Any:
    return json.loads(req.content) if req.content else None


def test_get_quotas_sends_query_params() -> None:
    sq, seen = _sync(
        _ok({"accountId": "123456789012", "region": "eu-west-1", "quotas": [QUOTA]})
    )
    resp = sq.get_quotas(
        account_id="123456789012", region="eu-west-1", service_code="vpc"
    )
    req = seen[0]
    assert req.method == "GET"
    assert req.url.path == "/_fakecloud/service-quotas/quotas"
    assert dict(req.url.params) == {
        "accountId": "123456789012",
        "region": "eu-west-1",
        "serviceCode": "vpc",
    }
    assert resp.account_id == "123456789012"
    assert resp.region == "eu-west-1"
    q = resp.quotas[0]
    assert q.quota_code == "L-0EA8095F"
    assert q.global_ is False
    assert q.default_value == 5.0
    assert q.applied_value == 2.0
    assert q.usage is None
    assert q.enforcement_source == "override"


def test_get_quotas_without_params() -> None:
    sq, seen = _sync(_ok({"accountId": "1", "region": "r", "quotas": []}))
    sq.get_quotas()
    assert seen[0].url.query == b""


def test_put_quota_omits_enforce_when_unset() -> None:
    sq, seen = _sync(_ok(QUOTA))
    q = sq.put_quota("vpc", "L-0EA8095F", value=2)
    req = seen[0]
    assert req.method == "PUT"
    assert req.url.path == "/_fakecloud/service-quotas/quotas/vpc/L-0EA8095F"
    assert _body(req) == {"value": 2}
    assert q.applied_value == 2.0


def test_put_quota_default_sends_explicit_null() -> None:
    sq, seen = _sync(_ok(QUOTA))
    sq.put_quota(
        "vpc",
        "L-0EA8095F",
        account_id="123456789012",
        region="us-east-1",
        enforcement=QuotaEnforcement.DEFAULT,
    )
    body = _body(seen[0])
    assert body == {"accountId": "123456789012", "region": "us-east-1", "enforce": None}
    assert "enforce" in body


@pytest.mark.parametrize(
    "enforcement,wire",
    [(QuotaEnforcement.ENFORCE, True), (QuotaEnforcement.IGNORE, False)],
)
def test_put_quota_enforce_bool(enforcement: QuotaEnforcement, wire: bool) -> None:
    sq, seen = _sync(_ok(QUOTA))
    sq.put_quota("vpc", "L-0EA8095F", enforcement=enforcement)
    assert _body(seen[0]) == {"enforce": wire}


def test_quota_path_segments_are_encoded() -> None:
    sq, seen = _sync(_ok(QUOTA))
    sq.delete_quota("ec 2", "L/1", account_id="123456789012")
    req = seen[0]
    assert req.method == "DELETE"
    assert req.url.raw_path.startswith(
        b"/_fakecloud/service-quotas/quotas/ec%202/L%2F1?"
    )
    assert dict(req.url.params) == {"accountId": "123456789012"}


def test_put_enforcement_serializes_overrides() -> None:
    sq, seen = _sync(_ok(ENFORCEMENT))
    resp = sq.put_enforcement(
        enforce_all=True,
        overrides=[
            ServiceQuotaEnforcementChange("vpc", "L-0EA8095F", QuotaEnforcement.IGNORE),
            ServiceQuotaEnforcementChange(
                "vpc",
                "L-0EA8095F",
                QuotaEnforcement.DEFAULT,
                account_id="111111111111",
            ),
        ],
    )
    req = seen[0]
    assert req.method == "PUT"
    assert req.url.path == "/_fakecloud/service-quotas/enforcement"
    assert _body(req) == {
        "enforceAll": True,
        "overrides": [
            {"serviceCode": "vpc", "quotaCode": "L-0EA8095F", "enforce": False},
            {
                "serviceCode": "vpc",
                "quotaCode": "L-0EA8095F",
                "enforce": None,
                "accountId": "111111111111",
            },
        ],
    }
    assert resp.enforce_all is True
    assert resp.overrides[0].enforce is False
    assert resp.account_overrides[0].account_id == "111111111111"


def test_get_enforcement() -> None:
    sq, seen = _sync(_ok(ENFORCEMENT))
    resp = sq.get_enforcement()
    assert seen[0].method == "GET"
    assert resp.overrides[0].quota_code == "L-0EA8095F"
    assert resp.account_overrides[0].enforce is True


def test_request_approval_roundtrip() -> None:
    sq, seen = _sync(_ok({"mode": "manual"}))
    assert sq.set_request_approval("manual").mode == "manual"
    assert seen[0].method == "PUT"
    assert seen[0].url.path == "/_fakecloud/service-quotas/request-approval"
    assert _body(seen[0]) == {"mode": "manual"}
    assert sq.get_request_approval().mode == "manual"
    assert seen[1].method == "GET"


def test_get_requests_with_filters() -> None:
    sq, seen = _sync(_ok({"requests": [REQUEST]}))
    resp = sq.get_requests(account_id="123456789012", status="PENDING")
    assert dict(seen[0].url.params) == {
        "accountId": "123456789012",
        "status": "PENDING",
    }
    r = resp.requests[0]
    assert r.desired_value == 10.0
    assert r.case_id is None
    assert r.last_updated == "2026-10-05T00:00:01+00:00"


def test_approve_request() -> None:
    sq, seen = _sync(_ok(REQUEST))
    r = sq.approve_request("req/1")
    req = seen[0]
    assert req.method == "POST"
    assert req.url.raw_path == b"/_fakecloud/service-quotas/requests/req%2F1/approve"
    assert r.status == "APPROVED"


def test_deny_request_default_and_status() -> None:
    sq, seen = _sync(_ok({**REQUEST, "status": "DENIED"}))
    sq.deny_request("abc")
    sq.deny_request("abc", status="CASE_CLOSED")
    assert seen[0].url.path == "/_fakecloud/service-quotas/requests/abc/deny"
    assert _body(seen[0]) == {}
    assert _body(seen[1]) == {"status": "CASE_CLOSED"}


def test_error_propagates() -> None:
    sq, _ = _sync(
        lambda _req: httpx.Response(400, json={"error": "mode must be auto or manual"})
    )
    with pytest.raises(FakeCloudError) as exc:
        sq.set_request_approval("sometimes")
    assert exc.value.status == 400
    assert "mode must be auto or manual" in exc.value.body


async def test_async_client_put_quota_and_error() -> None:
    seen: List[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        if req.url.path.endswith("/approve"):
            return httpx.Response(409, json={"error": "already decided"})
        return httpx.Response(200, json=QUOTA)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        sq = ServiceQuotasClient(client, BASE)
        q = await sq.put_quota(
            "vpc", "L-0EA8095F", enforcement=QuotaEnforcement.DEFAULT
        )
        assert q.enforced is True
        assert _body(seen[0]) == {"enforce": None}
        with pytest.raises(FakeCloudError) as exc:
            await sq.approve_request("r1")
        assert exc.value.status == 409


async def test_main_clients_expose_service_quotas() -> None:
    from fakecloud import FakeCloud, FakeCloudSync

    with FakeCloudSync(BASE) as fc_sync:
        assert isinstance(fc_sync.service_quotas, _SyncServiceQuotasClient)
    async with FakeCloud(BASE) as fc:
        assert isinstance(fc.service_quotas, ServiceQuotasClient)
