import { afterEach, describe, expect, it, vi } from "vitest";
import { FakeCloud, FakeCloudError } from "../src/client.js";

interface Call {
  url: string;
  method: string;
  body: string | undefined;
}

/** Stub `fetch` with one canned reply and record what was sent. */
function mockFetch(status: number, reply: unknown): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      calls.push({
        url,
        method: init?.method ?? "GET",
        body: init?.body as string | undefined,
      });
      return new Response(JSON.stringify(reply), {
        status,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
  return calls;
}

const BASE = "http://localhost:4566/_fakecloud/service-quotas";

const QUOTA = {
  serviceCode: "ec2",
  quotaCode: "L-0263D0A3",
  quotaName: "EC2-VPC Elastic IPs",
  global: false,
  adjustable: true,
  unit: "None",
  defaultValue: 5,
  appliedValue: 1,
  usage: null,
  enforceable: true,
  enforced: true,
  enforcementSource: "override",
};

const REQUEST = {
  accountId: "123456789012",
  requestId: "req-1",
  serviceCode: "ec2",
  quotaCode: "L-0263D0A3",
  quotaName: "EC2-VPC Elastic IPs",
  region: "us-east-1",
  desiredValue: 10,
  status: "APPROVED",
  caseId: null,
  created: "2026-10-05T00:00:00+00:00",
  lastUpdated: "2026-10-05T00:00:01+00:00",
};

describe("ServiceQuotasClient", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("getQuotas sends no query string without filters", async () => {
    const calls = mockFetch(200, {
      accountId: "123456789012",
      region: "us-east-1",
      quotas: [QUOTA],
    });
    const fc = new FakeCloud();
    const resp = await fc.serviceQuotas.getQuotas();
    expect(calls[0].url).toBe(`${BASE}/quotas`);
    expect(calls[0].method).toBe("GET");
    expect(resp.quotas[0].usage).toBeNull();
    expect(resp.quotas[0].enforcementSource).toBe("override");
  });

  it("getQuotas passes accountId, region and serviceCode", async () => {
    const calls = mockFetch(200, {
      accountId: "111111111111",
      region: "eu-west-1",
      quotas: [],
    });
    await new FakeCloud().serviceQuotas.getQuotas({
      accountId: "111111111111",
      region: "eu-west-1",
      serviceCode: "ec2",
    });
    expect(calls[0].url).toBe(
      `${BASE}/quotas?accountId=111111111111&region=eu-west-1&serviceCode=ec2`,
    );
  });

  it("putQuota omits enforce when unset", async () => {
    const calls = mockFetch(200, QUOTA);
    const q = await new FakeCloud().serviceQuotas.putQuota(
      "ec2",
      "L-0263D0A3",
      {
        value: 1,
      },
    );
    expect(calls[0].url).toBe(`${BASE}/quotas/ec2/L-0263D0A3`);
    expect(calls[0].method).toBe("PUT");
    expect(JSON.parse(calls[0].body!)).toEqual({ value: 1 });
    expect(q.appliedValue).toBe(1);
  });

  it("putQuota sends explicit null to clear the override", async () => {
    const calls = mockFetch(200, QUOTA);
    await new FakeCloud().serviceQuotas.putQuota("ec2", "L-0263D0A3", {
      enforce: null,
    });
    expect(calls[0].body).toBe('{"enforce":null}');
  });

  it("putQuota sends enforce true with account scope", async () => {
    const calls = mockFetch(200, QUOTA);
    await new FakeCloud().serviceQuotas.putQuota("ec2", "L-0263D0A3", {
      accountId: "123456789012",
      region: "us-east-1",
      value: 2,
      enforce: true,
    });
    expect(JSON.parse(calls[0].body!)).toEqual({
      accountId: "123456789012",
      region: "us-east-1",
      value: 2,
      enforce: true,
    });
  });

  it("putQuota and deleteQuota encode path segments", async () => {
    const calls = mockFetch(200, QUOTA);
    const sq = new FakeCloud().serviceQuotas;
    await sq.putQuota("a/b", "c d", { enforce: false });
    await sq.deleteQuota("a/b", "c d", {
      accountId: "123456789012",
      region: "us-east-1",
    });
    expect(calls[0].url).toBe(`${BASE}/quotas/a%2Fb/c%20d`);
    expect(calls[0].body).toBe('{"enforce":false}');
    expect(calls[1].method).toBe("DELETE");
    expect(calls[1].url).toBe(
      `${BASE}/quotas/a%2Fb/c%20d?accountId=123456789012&region=us-east-1`,
    );
    expect(calls[1].body).toBeUndefined();
  });

  it("putEnforcement serializes the global switch and overrides", async () => {
    const reply = {
      enforceAll: true,
      overrides: [],
      accountOverrides: [
        {
          accountId: "123456789012",
          serviceCode: "ec2",
          quotaCode: "L-0263D0A3",
          enforce: false,
        },
      ],
    };
    const calls = mockFetch(200, reply);
    const resp = await new FakeCloud().serviceQuotas.putEnforcement({
      enforceAll: true,
      overrides: [
        {
          serviceCode: "ec2",
          quotaCode: "L-0263D0A3",
          accountId: "123456789012",
          enforce: false,
        },
        { serviceCode: "vpc", quotaCode: "L-F678F1CE", enforce: null },
      ],
    });
    expect(calls[0].url).toBe(`${BASE}/enforcement`);
    expect(calls[0].method).toBe("PUT");
    expect(JSON.parse(calls[0].body!)).toEqual({
      enforceAll: true,
      overrides: [
        {
          serviceCode: "ec2",
          quotaCode: "L-0263D0A3",
          accountId: "123456789012",
          enforce: false,
        },
        { serviceCode: "vpc", quotaCode: "L-F678F1CE", enforce: null },
      ],
    });
    expect(resp.accountOverrides[0].enforce).toBe(false);
  });

  it("getEnforcement and getRequestApproval are plain GETs", async () => {
    const calls = mockFetch(200, { mode: "manual" });
    const sq = new FakeCloud().serviceQuotas;
    await sq.getEnforcement();
    const mode = await sq.getRequestApproval();
    expect(calls.map((c) => [c.method, c.url])).toEqual([
      ["GET", `${BASE}/enforcement`],
      ["GET", `${BASE}/request-approval`],
    ]);
    expect(mode.mode).toBe("manual");
  });

  it("setRequestApproval sends the mode", async () => {
    const calls = mockFetch(200, { mode: "manual" });
    const resp = await new FakeCloud().serviceQuotas.setRequestApproval(
      "manual",
    );
    expect(calls[0].method).toBe("PUT");
    expect(calls[0].url).toBe(`${BASE}/request-approval`);
    expect(calls[0].body).toBe('{"mode":"manual"}');
    expect(resp.mode).toBe("manual");
  });

  it("getRequests passes filters", async () => {
    const calls = mockFetch(200, { requests: [REQUEST] });
    const resp = await new FakeCloud().serviceQuotas.getRequests({
      accountId: "123456789012",
      status: "PENDING",
    });
    expect(calls[0].url).toBe(
      `${BASE}/requests?accountId=123456789012&status=PENDING`,
    );
    expect(resp.requests[0].caseId).toBeNull();
  });

  it("approveRequest posts without a body", async () => {
    const calls = mockFetch(200, REQUEST);
    const r = await new FakeCloud().serviceQuotas.approveRequest("req/1");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].url).toBe(`${BASE}/requests/req%2F1/approve`);
    expect(calls[0].body).toBeUndefined();
    expect(r.status).toBe("APPROVED");
  });

  it("denyRequest sends the status when given", async () => {
    const calls = mockFetch(200, { ...REQUEST, status: "CASE_CLOSED" });
    const sq = new FakeCloud().serviceQuotas;
    await sq.denyRequest("req-1", "CASE_CLOSED");
    await sq.denyRequest("req-1");
    expect(calls[0].url).toBe(`${BASE}/requests/req-1/deny`);
    expect(calls[0].body).toBe('{"status":"CASE_CLOSED"}');
    expect(calls[1].body).toBe("{}");
  });

  it("surfaces a 400 as FakeCloudError", async () => {
    mockFetch(400, { error: "nothing to change: give value and/or enforce" });
    const err = await new FakeCloud().serviceQuotas
      .putQuota("ec2", "L-0263D0A3", {})
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(FakeCloudError);
    expect((err as FakeCloudError).status).toBe(400);
    expect((err as FakeCloudError).body).toContain("nothing to change");
  });
});
