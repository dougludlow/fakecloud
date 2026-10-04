package fakecloud

import (
	"context"
	"fmt"
	"net/url"
)

// SQSClient provides access to SQS introspection endpoints.
type SQSClient struct {
	fc *FakeCloud
}

// GetMessages lists all messages across all SQS queues.
func (c *SQSClient) GetMessages(ctx context.Context) (*SQSMessagesResponse, error) {
	var out SQSMessagesResponse
	if err := c.fc.doGet(ctx, "/_fakecloud/sqs/messages", &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// TickExpiration ticks the SQS message expiration processor.
func (c *SQSClient) TickExpiration(ctx context.Context) (*ExpirationTickResponse, error) {
	var out ExpirationTickResponse
	if err := c.fc.doPost(ctx, "/_fakecloud/sqs/expiration-processor/tick", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ForceDLQ forces all messages in a queue to its dead-letter queue. The
// queue is looked up in the server's default account and region.
func (c *SQSClient) ForceDLQ(ctx context.Context, queueName string) (*ForceDLQResponse, error) {
	return c.ForceDLQIn(ctx, queueName, "", "")
}

// ForceDLQIn is ForceDLQ for the queue of that name in a specific account and
// region (queue names are unique only within one account and region). An
// empty accountID or region means the server default.
func (c *SQSClient) ForceDLQIn(ctx context.Context, queueName, accountID, region string) (*ForceDLQResponse, error) {
	path := fmt.Sprintf("/_fakecloud/sqs/%s/force-dlq", url.PathEscape(queueName))
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
	var out ForceDLQResponse
	if err := c.fc.doPost(ctx, path, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}
