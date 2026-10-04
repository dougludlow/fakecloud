+++
title = "EventBridge Scheduler"
description = "Standalone scheduler service — at, rate, cron expressions, SQS targets, DLQ routing, one-shot self-delete."
weight = 5
+++

fakecloud implements the full **EventBridge Scheduler** surface (`scheduler.amazonaws.com`). This is a different service from EventBridge Rules — it ships in its own SDK (`@aws-sdk/client-scheduler`) and has its own data model (Schedule, ScheduleGroup, FlexibleTimeWindow, DeadLetterConfig, ActionAfterCompletion).

## Supported features

- **Schedules** — `at(yyyy-mm-ddThh:mm:ss)` one-shot, `rate(N unit)` recurring, `cron(...)` recurring
- **Schedule groups** — create/get/delete/list; a `default` group is seeded per account and cannot be deleted
- **Targets** — SQS (with `Target.Input` JSON), SNS, Lambda, Step Functions, EventBridge; `Target.RoleArn` is accepted (STS assume-role is a no-op in fakecloud)
- **Cross-account targets** — `Target.Arn` may point to a queue/topic/function/state-machine/event-bus in a different account. Delivery routes by the ARN's account segment, matching fakecloud's ARN-routed multi-account model. Trust verification is a no-op (matches the existing fakecloud STS posture)
- **Flexible time window** — `OFF` and `FLEXIBLE` modes; `MaximumWindowInMinutes` (1-1440) is required when `FLEXIBLE`
- **ActionAfterCompletion: DELETE** — one-shot `at(...)` schedules self-delete after firing
- **Dead-letter routing** — when target delivery fails (e.g. queue missing), the `Target.Input` is forwarded to `DeadLetterConfig.Arn` with `X-Amz-Scheduler-*` metadata attributes (Attempt, Schedule-Arn, Target-Arn, Error-Code, Error-Message, Group)
- **Tagging** — `TagResource` / `UntagResource` / `ListTagsForResource` on schedule groups
- **IAM enforcement** — `scheduler:*` actions, `scheduler:ScheduleGroup` condition key, `aws:ResourceTag/*` + `aws:RequestTag/*` ABAC

## Protocol

REST-JSON protocol over `scheduler.<region>.amazonaws.com` endpoint. SigV4 credential scope: `scheduler/aws4_request`.

## Introspection

- `GET /_fakecloud/scheduler/schedules` — list every schedule across all accounts and regions (each entry names its `accountId` and `region`)
- `POST /_fakecloud/scheduler/fire/{group}/{name}` — fire a specific schedule immediately (bypasses the wall-clock tick); optional `?accountId=&region=`, default the server's

## Cross-service delivery

Scheduler shares fakecloud's cross-service delivery bus, so targets resolve to live SNS / SQS / Lambda / StepFunctions / EventBridge implementations — the same plumbing that powers EventBridge Rules.

## Firing semantics

- **Tick rate**: 1 second. Every enabled schedule is evaluated against the current wall clock each tick.
- **Cron grammar**: simplified six-field (`min hour dom month dow year`). Wildcards (`*` / `?`) and single numeric values only — ranges (`1-3`), lists (`1,3,5`), and step values (`*/5`) are rejected rather than silently broadened, so unsupported schedules never fire instead of firing every minute.
- **Dedup**: cron schedules dedupe by `(year, ordinal, hour, minute)`, so daily/weekly crons fire every matching day.
- **FIFO SQS targets**: a dedup ID is synthesized per fire (UUID, 36 chars — well under SQS's 128-char limit) so messages aren't dropped for FIFO queues that lack content-based dedup.

## Non-goals

- KMS encryption on schedule state
- End-to-end IAM policy evaluation beyond the existing `test/test` root bypass

## Source

- [`crates/fakecloud-scheduler`](https://github.com/faiscadev/fakecloud/tree/main/crates/fakecloud-scheduler)
- [AWS EventBridge Scheduler API reference](https://docs.aws.amazon.com/scheduler/latest/APIReference/Welcome.html)
