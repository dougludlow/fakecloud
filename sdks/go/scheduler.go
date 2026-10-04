package fakecloud

import (
	"context"
	"net/url"
)

// SchedulerClient provides access to EventBridge Scheduler
// introspection endpoints. Exposes the two hooks integration tests
// need: enumerate schedules registered on the server, and trigger a
// specific schedule to fire on demand.
type SchedulerClient struct {
	fc *FakeCloud
}

// GetSchedules returns every schedule the server knows about, across
// every account. Order is stable: by account, then group, then name.
func (c *SchedulerClient) GetSchedules(ctx context.Context) (*SchedulerSchedulesResponse, error) {
	var out SchedulerSchedulesResponse
	if err := c.fc.doGet(ctx, "/_fakecloud/scheduler/schedules", &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// FireSchedule triggers the named schedule immediately, bypassing the
// wall-clock tick. Applies the same post-fire handling as the normal
// loop (last_fired update, ActionAfterCompletion=DELETE cleanup).
// The schedule is looked up in the server's default account and region.
func (c *SchedulerClient) FireSchedule(ctx context.Context, group, name string) (*FireScheduleResponse, error) {
	return c.FireScheduleIn(ctx, group, name, "", "")
}

// FireScheduleIn is FireSchedule for the schedule in a specific account and
// region (schedules are regional). An empty accountID or region means the
// server default.
func (c *SchedulerClient) FireScheduleIn(ctx context.Context, group, name, accountID, region string) (*FireScheduleResponse, error) {
	path := "/_fakecloud/scheduler/fire/" + url.PathEscape(group) + "/" + url.PathEscape(name)
	q := url.Values{}
	if accountID != "" {
		q.Set("accountId", accountID)
	}
	if region != "" {
		q.Set("region", region)
	}
	if len(q) > 0 {
		path += "?" + q.Encode()
	}
	var out FireScheduleResponse
	if err := c.fc.doPost(ctx, path, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}
