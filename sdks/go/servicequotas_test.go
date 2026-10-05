package fakecloud

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
)

// recorded is one request seen by the stub server.
type recorded struct {
	method string
	uri    string
	body   string
}

// stubServer answers every request with status and body, recording what it
// received.
func stubServer(t *testing.T, status int, body string) (*FakeCloud, *[]recorded) {
	t.Helper()
	var seen []recorded
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		seen = append(seen, recorded{method: r.Method, uri: r.URL.RequestURI(), body: string(b)})
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(srv.Close)
	return New(srv.URL), &seen
}

const quotaJSON = `{"serviceCode":"ec2","quotaCode":"L-0263D0A3","quotaName":"EC2-VPC Elastic IPs",` +
	`"global":false,"adjustable":true,"unit":"None","defaultValue":5,"appliedValue":1,"usage":null,` +
	`"enforceable":true,"enforced":true,"enforcementSource":"override"}`

const requestJSON = `{"accountId":"123456789012","requestId":"req/1","serviceCode":"ec2",` +
	`"quotaCode":"L-0263D0A3","quotaName":"EC2-VPC Elastic IPs","region":"us-east-1",` +
	`"desiredValue":10,"status":"APPROVED","caseId":null,` +
	`"created":"2026-10-05T00:00:00+00:00","lastUpdated":"2026-10-05T00:00:01+00:00"}`

func TestServiceQuotasGetQuotasQuery(t *testing.T) {
	fc, seen := stubServer(t, 200, `{"accountId":"123456789012","region":"eu-west-1","quotas":[`+quotaJSON+`]}`)
	out, err := fc.ServiceQuotas().GetQuotas(context.Background(), &ServiceQuotasListOptions{
		AccountID:   "123456789012",
		Region:      "eu-west-1",
		ServiceCode: "ec2",
	})
	if err != nil {
		t.Fatal(err)
	}
	got := (*seen)[0]
	want := "/_fakecloud/service-quotas/quotas?accountId=123456789012&region=eu-west-1&serviceCode=ec2"
	if got.method != http.MethodGet || got.uri != want {
		t.Fatalf("got %s %s, want GET %s", got.method, got.uri, want)
	}
	q := out.Quotas[0]
	if out.Region != "eu-west-1" || q.AppliedValue != 1 || q.DefaultValue != 5 || q.Usage != nil ||
		q.EnforcementSource != "override" {
		t.Fatalf("unexpected decode: %+v", out)
	}
}

func TestServiceQuotasGetQuotasNoOptions(t *testing.T) {
	fc, seen := stubServer(t, 200, `{"accountId":"123456789012","region":"us-east-1","quotas":[]}`)
	if _, err := fc.ServiceQuotas().GetQuotas(context.Background(), nil); err != nil {
		t.Fatal(err)
	}
	if (*seen)[0].uri != "/_fakecloud/service-quotas/quotas" {
		t.Fatalf("unexpected uri %s", (*seen)[0].uri)
	}
}

func TestServiceQuotasPutQuotaEnforcement(t *testing.T) {
	value := 1.0
	cases := []struct {
		name string
		req  *ServiceQuotasPutQuotaRequest
		body string
	}{
		{"omitted", &ServiceQuotasPutQuotaRequest{Value: &value}, `{"value":1}`},
		{"default", &ServiceQuotasPutQuotaRequest{Enforcement: QuotaEnforcementDefault.Ptr()}, `{"enforce":null}`},
		{"enforce", &ServiceQuotasPutQuotaRequest{
			AccountID:   "123456789012",
			Region:      "us-east-1",
			Value:       &value,
			Enforcement: QuotaEnforcementEnforce.Ptr(),
		}, `{"accountId":"123456789012","region":"us-east-1","value":1,"enforce":true}`},
		{"ignore", &ServiceQuotasPutQuotaRequest{Enforcement: QuotaEnforcementIgnore.Ptr()}, `{"enforce":false}`},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fc, seen := stubServer(t, 200, quotaJSON)
			out, err := fc.ServiceQuotas().PutQuota(context.Background(), "ec2", "L-0263D0A3", tc.req)
			if err != nil {
				t.Fatal(err)
			}
			got := (*seen)[0]
			if got.method != http.MethodPut || got.uri != "/_fakecloud/service-quotas/quotas/ec2/L-0263D0A3" {
				t.Fatalf("got %s %s", got.method, got.uri)
			}
			if got.body != tc.body {
				t.Fatalf("body %s, want %s", got.body, tc.body)
			}
			if !out.Enforced {
				t.Fatalf("unexpected decode: %+v", out)
			}
		})
	}
}

func TestServiceQuotasDeleteQuota(t *testing.T) {
	fc, seen := stubServer(t, 200, quotaJSON)
	if _, err := fc.ServiceQuotas().DeleteQuota(context.Background(), "ec2", "L 1", &ServiceQuotasDeleteQuotaOptions{
		AccountID: "123456789012",
	}); err != nil {
		t.Fatal(err)
	}
	got := (*seen)[0]
	if got.method != http.MethodDelete || got.uri != "/_fakecloud/service-quotas/quotas/ec2/L%201?accountId=123456789012" {
		t.Fatalf("got %s %s", got.method, got.uri)
	}
}

func TestServiceQuotasPutEnforcement(t *testing.T) {
	fc, seen := stubServer(t, 200, `{"enforceAll":true,"overrides":[{"serviceCode":"ec2","quotaCode":"L-1","enforce":false}],`+
		`"accountOverrides":[{"accountId":"123456789012","serviceCode":"ec2","quotaCode":"L-2","enforce":true}]}`)
	on := true
	out, err := fc.ServiceQuotas().PutEnforcement(context.Background(), &ServiceQuotasPutEnforcementRequest{
		EnforceAll: &on,
		Overrides: []ServiceQuotasOverrideChange{
			{ServiceCode: "ec2", QuotaCode: "L-1", Enforcement: QuotaEnforcementIgnore},
			{ServiceCode: "ec2", QuotaCode: "L-2", AccountID: "123456789012", Enforcement: QuotaEnforcementEnforce},
			{ServiceCode: "ec2", QuotaCode: "L-3", Enforcement: QuotaEnforcementDefault},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	got := (*seen)[0]
	want := `{"enforceAll":true,"overrides":[` +
		`{"serviceCode":"ec2","quotaCode":"L-1","enforce":false},` +
		`{"serviceCode":"ec2","quotaCode":"L-2","accountId":"123456789012","enforce":true},` +
		`{"serviceCode":"ec2","quotaCode":"L-3","enforce":null}]}`
	if got.method != http.MethodPut || got.uri != "/_fakecloud/service-quotas/enforcement" || got.body != want {
		t.Fatalf("got %s %s %s", got.method, got.uri, got.body)
	}
	if !out.EnforceAll || out.Overrides[0].Enforce || out.AccountOverrides[0].AccountID != "123456789012" {
		t.Fatalf("unexpected decode: %+v", out)
	}
}

func TestServiceQuotasRequestApproval(t *testing.T) {
	fc, seen := stubServer(t, 200, `{"mode":"manual"}`)
	out, err := fc.ServiceQuotas().SetRequestApproval(context.Background(), "manual")
	if err != nil {
		t.Fatal(err)
	}
	got := (*seen)[0]
	if got.method != http.MethodPut || got.uri != "/_fakecloud/service-quotas/request-approval" ||
		got.body != `{"mode":"manual"}` || out.Mode != "manual" {
		t.Fatalf("got %s %s %s -> %+v", got.method, got.uri, got.body, out)
	}
}

func TestServiceQuotasGetRequests(t *testing.T) {
	fc, seen := stubServer(t, 200, `{"requests":[`+requestJSON+`]}`)
	out, err := fc.ServiceQuotas().GetRequests(context.Background(), &ServiceQuotaRequestsOptions{Status: "PENDING"})
	if err != nil {
		t.Fatal(err)
	}
	if (*seen)[0].uri != "/_fakecloud/service-quotas/requests?status=PENDING" {
		t.Fatalf("unexpected uri %s", (*seen)[0].uri)
	}
	r := out.Requests[0]
	if r.RequestID != "req/1" || r.DesiredValue != 10 || r.CaseID != nil {
		t.Fatalf("unexpected decode: %+v", r)
	}
}

func TestServiceQuotasApproveAndDeny(t *testing.T) {
	fc, seen := stubServer(t, 200, requestJSON)
	if _, err := fc.ServiceQuotas().ApproveRequest(context.Background(), "req/1"); err != nil {
		t.Fatal(err)
	}
	if _, err := fc.ServiceQuotas().DenyRequest(context.Background(), "req/1", "CASE_CLOSED"); err != nil {
		t.Fatal(err)
	}
	if _, err := fc.ServiceQuotas().DenyRequest(context.Background(), "req/1", ""); err != nil {
		t.Fatal(err)
	}
	approve, deny, denyDefault := (*seen)[0], (*seen)[1], (*seen)[2]
	if approve.method != http.MethodPost || approve.uri != "/_fakecloud/service-quotas/requests/req%2F1/approve" || approve.body != "" {
		t.Fatalf("approve: %+v", approve)
	}
	if deny.uri != "/_fakecloud/service-quotas/requests/req%2F1/deny" || deny.body != `{"status":"CASE_CLOSED"}` {
		t.Fatalf("deny: %+v", deny)
	}
	if denyDefault.body != `{}` {
		t.Fatalf("deny default: %+v", denyDefault)
	}
}

func TestServiceQuotasErrorPropagates(t *testing.T) {
	fc, _ := stubServer(t, 400, `{"error":"nothing to change: give value and/or enforce"}`)
	_, err := fc.ServiceQuotas().PutQuota(context.Background(), "ec2", "L-0263D0A3", nil)
	var apiErr *APIError
	if !errors.As(err, &apiErr) || apiErr.StatusCode != 400 ||
		apiErr.Body != `{"error":"nothing to change: give value and/or enforce"}` {
		t.Fatalf("expected APIError 400, got %v", err)
	}
}

func TestQuotaEnforcementJSON(t *testing.T) {
	var e QuotaEnforcement
	for _, tc := range []struct {
		in   string
		want QuotaEnforcement
	}{{"true", QuotaEnforcementEnforce}, {"false", QuotaEnforcementIgnore}, {"null", QuotaEnforcementDefault}} {
		if err := e.UnmarshalJSON([]byte(tc.in)); err != nil || e != tc.want {
			t.Fatalf("%s -> %q, %v", tc.in, e, err)
		}
	}
	if _, err := QuotaEnforcement("bogus").MarshalJSON(); err == nil {
		t.Fatal("expected error for unknown enforcement")
	}
}
