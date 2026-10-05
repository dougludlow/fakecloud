package dev.fakecloud;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.sun.net.httpserver.HttpServer;
import dev.fakecloud.Types.QuotaEnforcement;
import dev.fakecloud.Types.ServiceQuotasOverrideChange;
import dev.fakecloud.Types.ServiceQuotasPutEnforcementRequest;
import dev.fakecloud.Types.ServiceQuotasPutQuotaRequest;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/** Exercises the Service Quotas sub-client against an in-process mock server. */
class ServiceQuotasClientTest {

    private static final String QUOTA = "{\"serviceCode\":\"ec2\",\"quotaCode\":\"L-1216C47A\","
            + "\"quotaName\":\"Running On-Demand Standard instances\",\"global\":false,"
            + "\"adjustable\":true,\"unit\":\"None\",\"defaultValue\":5.0,\"appliedValue\":2.0,"
            + "\"usage\":null,\"enforceable\":true,\"enforced\":true,"
            + "\"enforcementSource\":\"override\"}";

    private static final String ENFORCEMENT = "{\"enforceAll\":true,"
            + "\"overrides\":[{\"serviceCode\":\"ec2\",\"quotaCode\":\"L-1216C47A\",\"enforce\":false}],"
            + "\"accountOverrides\":[{\"accountId\":\"111122223333\",\"serviceCode\":\"ec2\","
            + "\"quotaCode\":\"L-1216C47A\",\"enforce\":true}]}";

    private static final String REQUEST = "{\"accountId\":\"123456789012\",\"requestId\":\"req-1\","
            + "\"serviceCode\":\"ec2\",\"quotaCode\":\"L-1216C47A\","
            + "\"quotaName\":\"Running On-Demand Standard instances\",\"region\":\"us-east-1\","
            + "\"desiredValue\":10.0,\"status\":\"APPROVED\",\"caseId\":null,"
            + "\"created\":\"2026-10-05T00:00:00+00:00\",\"lastUpdated\":\"2026-10-05T00:01:00+00:00\"}";

    private record Captured(String method, String uri, String body) {}

    private HttpServer server;
    private final List<Captured> captured = new ArrayList<>();
    private volatile int status = 200;
    private volatile String response = "{}";
    private FakeCloud fc;

    @BeforeEach
    void start() throws IOException {
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", exchange -> {
            String body = new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            synchronized (captured) {
                captured.add(new Captured(
                        exchange.getRequestMethod(), exchange.getRequestURI().getRawPath()
                                + (exchange.getRequestURI().getRawQuery() == null
                                        ? ""
                                        : "?" + exchange.getRequestURI().getRawQuery()),
                        body));
            }
            byte[] out = response.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().add("Content-Type", "application/json");
            exchange.sendResponseHeaders(status, out.length);
            try (OutputStream os = exchange.getResponseBody()) {
                os.write(out);
            }
        });
        server.start();
        fc = new FakeCloud("http://127.0.0.1:" + server.getAddress().getPort());
    }

    @AfterEach
    void stop() {
        server.stop(0);
    }

    private Captured last() {
        synchronized (captured) {
            return captured.get(captured.size() - 1);
        }
    }

    @Test
    void getQuotasSendsQueryParamsAndParsesResponse() {
        response = "{\"accountId\":\"111122223333\",\"region\":\"eu-west-1\",\"quotas\":[" + QUOTA + "]}";
        var res = fc.serviceQuotas().getQuotas("111122223333", "eu-west-1", "ec2");
        assertEquals("GET", last().method());
        assertEquals(
                "/_fakecloud/service-quotas/quotas?accountId=111122223333&region=eu-west-1&serviceCode=ec2",
                last().uri());
        assertEquals("111122223333", res.accountId());
        assertEquals(1, res.quotas().size());
        var q = res.quotas().get(0);
        assertEquals("L-1216C47A", q.quotaCode());
        assertEquals(5.0, q.defaultValue());
        assertEquals(2.0, q.appliedValue());
        assertNull(q.usage());
        assertTrue(q.enforced());
        assertEquals("override", q.enforcementSource());
    }

    @Test
    void getQuotasWithoutOptionsSendsNoQuery() {
        response = "{\"accountId\":\"123456789012\",\"region\":\"us-east-1\",\"quotas\":[]}";
        fc.serviceQuotas().getQuotas();
        assertEquals("/_fakecloud/service-quotas/quotas", last().uri());
    }

    @Test
    void putQuotaOmitsEnforceWhenUnset() {
        response = QUOTA;
        var q = fc.serviceQuotas().putQuota(
                "ec2", "L-1216C47A", new ServiceQuotasPutQuotaRequest(null, null, 2.0, null));
        assertEquals("PUT", last().method());
        assertEquals("/_fakecloud/service-quotas/quotas/ec2/L-1216C47A", last().uri());
        assertEquals("{\"value\":2.0}", last().body());
        assertEquals(2.0, q.appliedValue());
    }

    @Test
    void putQuotaSendsExplicitNullForDefault() {
        response = QUOTA;
        fc.serviceQuotas().putQuota(
                "ec2",
                "L-1216C47A",
                new ServiceQuotasPutQuotaRequest(null, null, null, QuotaEnforcement.DEFAULT));
        assertEquals("{\"enforce\":null}", last().body());
    }

    @Test
    void putQuotaSendsTrueForEnforceWithScope() {
        response = QUOTA;
        fc.serviceQuotas().putQuota(
                "ec2",
                "L-1216C47A",
                new ServiceQuotasPutQuotaRequest(
                        "111122223333", "eu-west-1", 3.0, QuotaEnforcement.ENFORCE));
        assertEquals(
                "{\"accountId\":\"111122223333\",\"region\":\"eu-west-1\",\"value\":3.0,\"enforce\":true}",
                last().body());
    }

    @Test
    void putQuotaEncodesPathSegments() {
        response = QUOTA;
        fc.serviceQuotas().putQuota(
                "a b", "c/d", new ServiceQuotasPutQuotaRequest(null, null, null, QuotaEnforcement.IGNORE));
        assertEquals("/_fakecloud/service-quotas/quotas/a%20b/c%2Fd", last().uri());
        assertEquals("{\"enforce\":false}", last().body());
    }

    @Test
    void deleteQuotaSendsScopeAsQuery() {
        response = QUOTA;
        fc.serviceQuotas().deleteQuota("ec2", "L-1216C47A", "111122223333", null);
        assertEquals("DELETE", last().method());
        assertEquals(
                "/_fakecloud/service-quotas/quotas/ec2/L-1216C47A?accountId=111122223333",
                last().uri());
    }

    @Test
    void getEnforcementParsesOverrides() {
        response = ENFORCEMENT;
        var res = fc.serviceQuotas().getEnforcement();
        assertEquals("GET", last().method());
        assertEquals("/_fakecloud/service-quotas/enforcement", last().uri());
        assertTrue(res.enforceAll());
        assertFalse(res.overrides().get(0).enforce());
        assertEquals("111122223333", res.accountOverrides().get(0).accountId());
        assertTrue(res.accountOverrides().get(0).enforce());
    }

    @Test
    void putEnforcementSerializesOverrides() {
        response = ENFORCEMENT;
        fc.serviceQuotas().putEnforcement(new ServiceQuotasPutEnforcementRequest(
                true,
                List.of(
                        new ServiceQuotasOverrideChange(
                                "ec2", "L-1216C47A", null, QuotaEnforcement.IGNORE),
                        new ServiceQuotasOverrideChange(
                                "ec2", "L-1216C47A", "111122223333", QuotaEnforcement.DEFAULT))));
        assertEquals("PUT", last().method());
        assertEquals("/_fakecloud/service-quotas/enforcement", last().uri());
        assertEquals(
                "{\"enforceAll\":true,\"overrides\":["
                        + "{\"serviceCode\":\"ec2\",\"quotaCode\":\"L-1216C47A\",\"enforce\":false},"
                        + "{\"serviceCode\":\"ec2\",\"quotaCode\":\"L-1216C47A\","
                        + "\"accountId\":\"111122223333\",\"enforce\":null}]}",
                last().body());
    }

    @Test
    void putEnforcementOmitsUnsetFields() {
        response = ENFORCEMENT;
        fc.serviceQuotas().putEnforcement(new ServiceQuotasPutEnforcementRequest(false, null));
        assertEquals("{\"enforceAll\":false}", last().body());
    }

    @Test
    void requestApprovalRoundTrips() {
        response = "{\"mode\":\"manual\"}";
        var res = fc.serviceQuotas().setRequestApproval("manual");
        assertEquals("PUT", last().method());
        assertEquals("/_fakecloud/service-quotas/request-approval", last().uri());
        assertEquals("{\"mode\":\"manual\"}", last().body());
        assertEquals("manual", res.mode());

        assertEquals("manual", fc.serviceQuotas().getRequestApproval().mode());
        assertEquals("GET", last().method());
    }

    @Test
    void getRequestsSendsFiltersAndParses() {
        response = "{\"requests\":[" + REQUEST + "]}";
        var res = fc.serviceQuotas().getRequests("123456789012", "PENDING");
        assertEquals(
                "/_fakecloud/service-quotas/requests?accountId=123456789012&status=PENDING",
                last().uri());
        var r = res.requests().get(0);
        assertEquals("req-1", r.requestId());
        assertEquals(10.0, r.desiredValue());
        assertNull(r.caseId());
        assertEquals("2026-10-05T00:01:00+00:00", r.lastUpdated());
    }

    @Test
    void approveRequestPostsWithoutBody() {
        response = REQUEST;
        var r = fc.serviceQuotas().approveRequest("req-1");
        assertEquals("POST", last().method());
        assertEquals("/_fakecloud/service-quotas/requests/req-1/approve", last().uri());
        assertEquals("", last().body());
        assertEquals("APPROVED", r.status());
    }

    @Test
    void denyRequestSendsStatusWhenGiven() {
        response = REQUEST.replace("APPROVED", "CASE_CLOSED");
        var r = fc.serviceQuotas().denyRequest("req-1", "CASE_CLOSED");
        assertEquals("POST", last().method());
        assertEquals("/_fakecloud/service-quotas/requests/req-1/deny", last().uri());
        assertEquals("{\"status\":\"CASE_CLOSED\"}", last().body());
        assertEquals("CASE_CLOSED", r.status());

        response = REQUEST.replace("APPROVED", "DENIED");
        fc.serviceQuotas().denyRequest("req-1");
        assertEquals("{}", last().body());
    }

    @Test
    void errorResponseIsSurfacedAsFakeCloudError() {
        status = 400;
        response = "{\"error\":\"nothing to change: give value and/or enforce\"}";
        FakeCloudError err = assertThrows(
                FakeCloudError.class,
                () -> fc.serviceQuotas().putQuota(
                        "ec2", "L-1216C47A", new ServiceQuotasPutQuotaRequest(null, null, null, null)));
        assertEquals(400, err.status());
        assertTrue(err.body().contains("nothing to change"));
        assertEquals("{}", last().body());
    }
}
