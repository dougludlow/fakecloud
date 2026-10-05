package fakecloud

import (
	"context"
	"fmt"
	"net/url"
)

// ServiceQuotasClient provides access to the Service Quotas
// admin/introspection endpoints: applied values, usage, enforcement and
// increase-request decisions.
type ServiceQuotasClient struct {
	fc *FakeCloud
}

func quotaPath(serviceCode, quotaCode string) string {
	return fmt.Sprintf(
		"/_fakecloud/service-quotas/quotas/%s/%s",
		url.PathEscape(serviceCode),
		url.PathEscape(quotaCode),
	)
}

func withQuery(path string, q url.Values) string {
	if len(q) == 0 {
		return path
	}
	return path + "?" + q.Encode()
}

// GetQuotas lists every catalog quota (or one service's) with its applied
// value, usage and enforcement state for an account and region. opts may be
// nil to use the server's account and region. Returns an APIError
// (StatusCode 404) for an unknown service code.
func (c *ServiceQuotasClient) GetQuotas(ctx context.Context, opts *ServiceQuotasListOptions) (*ServiceQuotasResponse, error) {
	q := url.Values{}
	if opts != nil {
		if opts.AccountID != "" {
			q.Set("accountId", opts.AccountID)
		}
		if opts.Region != "" {
			q.Set("region", opts.Region)
		}
		if opts.ServiceCode != "" {
			q.Set("serviceCode", opts.ServiceCode)
		}
	}
	var out ServiceQuotasResponse
	if err := c.fc.doGet(ctx, withQuery("/_fakecloud/service-quotas/quotas", q), &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// PutQuota sets one quota's applied value (which may be below the AWS
// default) and/or its enforcement override, returning the updated quota.
// Returns an APIError (StatusCode 404) for an unknown quota and 400 when
// neither Value nor Enforcement is set.
func (c *ServiceQuotasClient) PutQuota(
	ctx context.Context,
	serviceCode, quotaCode string,
	req *ServiceQuotasPutQuotaRequest,
) (*ServiceQuota, error) {
	if req == nil {
		req = &ServiceQuotasPutQuotaRequest{}
	}
	var out ServiceQuota
	if err := c.fc.doPut(ctx, quotaPath(serviceCode, quotaCode), req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteQuota puts a quota back to its AWS default and drops its enforcement
// override: the account's when opts.AccountID is set, else the server-wide
// one. opts may be nil.
func (c *ServiceQuotasClient) DeleteQuota(
	ctx context.Context,
	serviceCode, quotaCode string,
	opts *ServiceQuotasDeleteQuotaOptions,
) (*ServiceQuota, error) {
	q := url.Values{}
	if opts != nil {
		if opts.AccountID != "" {
			q.Set("accountId", opts.AccountID)
		}
		if opts.Region != "" {
			q.Set("region", opts.Region)
		}
	}
	var out ServiceQuota
	if err := c.fc.doDelete(ctx, withQuery(quotaPath(serviceCode, quotaCode), q), &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetEnforcement returns the global enforcement switch and every
// server-wide and per-account override.
func (c *ServiceQuotasClient) GetEnforcement(ctx context.Context) (*ServiceQuotasEnforcementResponse, error) {
	var out ServiceQuotasEnforcementResponse
	if err := c.fc.doGet(ctx, "/_fakecloud/service-quotas/enforcement", &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// PutEnforcement changes the global switch and/or a batch of overrides.
// Every change is validated before any is applied.
func (c *ServiceQuotasClient) PutEnforcement(
	ctx context.Context,
	req *ServiceQuotasPutEnforcementRequest,
) (*ServiceQuotasEnforcementResponse, error) {
	if req == nil {
		req = &ServiceQuotasPutEnforcementRequest{}
	}
	var out ServiceQuotasEnforcementResponse
	if err := c.fc.doPut(ctx, "/_fakecloud/service-quotas/enforcement", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetRequestApproval returns how quota increase requests are decided:
// "auto" or "manual".
func (c *ServiceQuotasClient) GetRequestApproval(ctx context.Context) (*ServiceQuotasRequestApprovalResponse, error) {
	var out ServiceQuotasRequestApprovalResponse
	if err := c.fc.doGet(ctx, "/_fakecloud/service-quotas/request-approval", &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// SetRequestApproval switches how increase requests are decided. mode is
// "auto" (decided on submission) or "manual" (held PENDING until
// ApproveRequest or DenyRequest).
func (c *ServiceQuotasClient) SetRequestApproval(ctx context.Context, mode string) (*ServiceQuotasRequestApprovalResponse, error) {
	var out ServiceQuotasRequestApprovalResponse
	if err := c.fc.doPut(
		ctx,
		"/_fakecloud/service-quotas/request-approval",
		&ServiceQuotasRequestApprovalRequest{Mode: mode},
		&out,
	); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetRequests lists quota increase requests across accounts, newest first.
// opts may be nil.
func (c *ServiceQuotasClient) GetRequests(ctx context.Context, opts *ServiceQuotaRequestsOptions) (*ServiceQuotaRequestsResponse, error) {
	q := url.Values{}
	if opts != nil {
		if opts.AccountID != "" {
			q.Set("accountId", opts.AccountID)
		}
		if opts.Status != "" {
			q.Set("status", opts.Status)
		}
	}
	var out ServiceQuotaRequestsResponse
	if err := c.fc.doGet(ctx, withQuery("/_fakecloud/service-quotas/requests", q), &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ApproveRequest approves a PENDING or CASE_OPENED request, raising the
// account's applied value to the requested one. Returns an APIError
// (StatusCode 409) when the request is already decided.
func (c *ServiceQuotasClient) ApproveRequest(ctx context.Context, requestID string) (*ServiceQuotaRequest, error) {
	path := fmt.Sprintf("/_fakecloud/service-quotas/requests/%s/approve", url.PathEscape(requestID))
	var out ServiceQuotaRequest
	if err := c.fc.doPost(ctx, path, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DenyRequest closes a PENDING or CASE_OPENED request without raising the
// quota. status is one of DENIED, NOT_APPROVED, CASE_CLOSED or
// INVALID_REQUEST; empty means DENIED.
func (c *ServiceQuotasClient) DenyRequest(ctx context.Context, requestID, status string) (*ServiceQuotaRequest, error) {
	path := fmt.Sprintf("/_fakecloud/service-quotas/requests/%s/deny", url.PathEscape(requestID))
	var out ServiceQuotaRequest
	if err := c.fc.doPost(ctx, path, &ServiceQuotasDenyRequest{Status: status}, &out); err != nil {
		return nil, err
	}
	return &out, nil
}
