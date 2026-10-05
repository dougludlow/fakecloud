+++
title = "Service Quotas"
description = "Service Quotas on fakecloud: all 26 operations, a catalog of real quota codes with AWS default values, increase requests that raise the applied value, the Organizations quota request template, tags, automatic management and utilization reports. Opt-in enforcement (globally or per quota), applied values settable below the AWS default, and manual request approval through the introspection API."
weight = 81
+++

fakecloud implements **Service Quotas** (`servicequotas`). All **26 operations**
from the AWS Smithy model ship, backed by account-partitioned state that
persists across restarts in persistent mode. The wire protocol is awsJson1.1
(x-amz-target `ServiceQuotasV20190624.<Op>`), signing as `servicequotas`.

Quotas are not just reported. Once you switch enforcement on, EC2 reads the
applied value of the security-group quotas from Service Quotas, so raising one
with `RequestServiceQuotaIncrease` changes what EC2 accepts. Enforcement is
**opt-in**: by default nothing is enforced, so a local test suite that creates
more resources than a fresh AWS account allows keeps working. See
[Enforcement](#enforcement).

## Quota catalog

fakecloud carries real quota codes, names and AWS default values for a new
account:

| Service code | Quotas |
|---|---|
| `vpc` | 25 quotas, including `L-2AFB9258` Security groups per network interface (5), `L-0EA8095F` Inbound or outbound rules per security group (60), `L-F678F1CE` VPCs per Region (5), `L-407747CB` Subnets per VPC (200), `L-E79EC296` VPC security groups per Region (2500) |
| `ec2` | On-Demand and Spot vCPU quotas (`L-1216C47A`, `L-34B43A08`, with their `AWS/Usage` usage metric), EC2-VPC Elastic IPs (`L-0263D0A3`), the accelerated-instance families, transit gateways, Site-to-Site VPN connections |
| `iam` | Users, roles, groups, managed policies per role/user/group, customer managed policies, server certificates, OIDC providers (global quotas) |
| `lambda`, `s3`, `dynamodb`, `kms` | Concurrent executions and function storage, general purpose buckets, tables, customer managed keys |

`ListServices` returns these services. An unknown service code or quota code
returns `NoSuchResourceException`.

## Supported features

### Quotas

- **`GetAWSDefaultServiceQuota`** / **`ListAWSDefaultServiceQuotas`** return
  the AWS default, with a default-quota ARN
  (`arn:<partition>:servicequotas:<region>::<service>/<quota>`).
- **`GetServiceQuota`** / **`ListServiceQuotas`** return the account's applied
  value, with an applied-quota ARN
  (`arn:<partition>:servicequotas:<region>:<account>:<service>/<quota>`).
  The partition follows the region (`aws-cn` for `cn-*`, `aws-us-gov` for
  `us-gov-*`), and global quotas (IAM) leave the region empty in both forms.
  `QuotaCode` and `QuotaAppliedAtLevel` filter the list.
  Every catalog quota applies at the account level, so `RESOURCE` matches
  nothing and a `ContextId` is rejected.

### Increase requests

- **`RequestServiceQuotaIncrease`** returns the request as `PENDING`. By
  default it is decided straight away (with `--quota-requests manual` it waits
  for the introspection API, see [Manual request approval](#manual-request-approval)).
  It is `APPROVED` and the applied value raised,
  unless it asks for more than AWS allows. Some quotas have a documented
  maximum (16 security groups per network interface, for example). For the two
  security-group quotas, AWS also requires that security groups per interface
  multiplied by rules per group stays at or below 1000. A request above
  either limit is `NOT_APPROVED`, and the applied value stays. A value at or
  below the current one, or a non-adjustable quota, is an
  `IllegalArgumentException`.
- **`GetRequestedServiceQuotaChange`**,
  **`ListRequestedServiceQuotaChangeHistory`** (filter by service, status and
  level) and **`ListRequestedServiceQuotaChangeHistoryByQuota`** read the
  request history, newest first. Requests are regional; global quotas' requests
  are visible from every region.
- **`CreateSupportCase`** opens a case for a `PENDING` request, moving it to
  `CASE_OPENED` (`InvalidResourceStateException` for any other status). Under
  automatic approval requests are decided on submission, so only manual
  approval leaves one to open a case for.

### Quota request template

Templates are used from the AWS Organizations management account, in
`us-east-1` only (`TemplatesNotAvailableInRegionException` elsewhere). Calls
from an account outside an organization return
`NoAvailableOrganizationException`, and calls from a member account return
`AccessDeniedException`.

- **`PutServiceQuotaIncreaseRequestIntoTemplate`**,
  **`GetServiceQuotaIncreaseRequestFromTemplate`**,
  **`DeleteServiceQuotaIncreaseRequestFromTemplate`** and
  **`ListServiceQuotaIncreaseRequestsInTemplate`** manage up to 10 entries
  (`QuotaExceededException` past that).
- **`AssociateServiceQuotaTemplate`** requires an organization with all
  features enabled (`OrganizationNotInAllFeaturesModeException` otherwise) and
  enables trusted access for `servicequotas.amazonaws.com`.
  **`GetAssociationForServiceQuotaTemplate`** reports `ASSOCIATED` or
  `DISASSOCIATED`, and **`DisassociateServiceQuotaTemplate`** detaches it.
- An associated template is applied to every account **created** in the
  organization after the association, when the account is created: each entry
  becomes an increase request in that account and region, decided on the same
  terms as `RequestServiceQuotaIncrease` (left `PENDING` under manual
  approval). Accounts that were already members, or
  that joined by invitation, are left alone.

### Tags

**`TagResource`**, **`UntagResource`** and **`ListTagsForResource`** tag the
caller's applied quotas by quota ARN (up to 50 tags; `aws:` keys are
reserved).

### Automatic management and utilization reports

- **`StartAutoManagement`**, **`GetAutoManagementConfiguration`**,
  **`UpdateAutoManagement`** and **`StopAutoManagement`** store the opt-in
  level and type, notification ARN and exclusion list per region.
- **`StartQuotaUtilizationReport`** / **`GetQuotaUtilizationReport`** report
  real usage for the quotas fakecloud can count from EC2 state (VPCs, internet
  gateways, egress-only internet gateways, security groups, network interfaces
  and Elastic IPs), as a percentage of the applied value.

## Enforcement

fakecloud enforces a quota only when you switch it on. A quota is enforced
when, in order of precedence:

1. the account has an override for that quota (`enforce` or `ignore`), else
2. there is a server-wide override for that quota, else
3. the global switch is on.

| Flag | Env var | Effect |
|---|---|---|
| `--enforce-quotas` | `FAKECLOUD_ENFORCE_QUOTAS` | Enforce every enforceable quota at its applied value |
| `--enforce-quota SERVICE/QUOTA` | `FAKECLOUD_ENFORCE_QUOTA` (comma-separated) | Enforce one quota, e.g. `--enforce-quota vpc/L-0EA8095F`. Repeatable |
| `--ignore-quota SERVICE/QUOTA` | `FAKECLOUD_IGNORE_QUOTA` (comma-separated) | Never enforce one quota, even with `--enforce-quotas`. Repeatable |
| `--quota-requests auto\|manual` | `FAKECLOUD_QUOTA_REQUESTS` | How increase requests are decided (default `auto`) |

The server refuses to start on an unknown quota code, or on `--enforce-quota`
for a quota no fakecloud service checks. The server-wide settings (the global
switch, server-wide overrides and the approval mode) come from these flags at
every start: changes made at runtime through the introspection API last until
a restart or `POST /_fakecloud/reset`, which puts them back to the flags.
Applied values, per-account overrides and requests are account data and
persist across restarts in persistent mode.

### Enforceable quotas

| Quota | Enforced by |
|---|---|
| Security groups per network interface (`vpc`/`L-2AFB9258`) | `CreateNetworkInterface`, `ModifyNetworkInterfaceAttribute` (`SecurityGroupsPerInterfaceLimitExceeded`); `RunInstances`, `ModifyInstanceAttribute`, Auto Scaling and CloudFormation launches (`SecurityGroupsPerInstanceLimitExceeded`) |
| Inbound or outbound rules per security group (`vpc`/`L-0EA8095F`) | `AuthorizeSecurityGroupIngress`, `AuthorizeSecurityGroupEgress`, `ModifySecurityGroupRules`, `ModifyManagedPrefixList`, CloudFormation security groups (`RulesPerSecurityGroupLimitExceeded`) |

As on AWS, the rules quota applies to each direction separately and counts
IPv4 and IPv6 rules separately. A rule that references a security group counts
toward both. A rule that references a customer-managed prefix list counts as
the list's maximum number of entries, toward the list's address family.
`ModifyManagedPrefixList` honours this too: a larger `MaxEntries` that would
push a referencing group over the quota leaves the list at its old size in
`modify-failed`, with the group ids in `StateMessage`.

`ValidateSecurityGroupQuotasForInterface` exists to ask whether groups fit the
quotas, so it always answers against the applied values, enforced or not.

Every other catalog quota is reported with `enforceable: false`; an override
on one (on or off) is refused rather than silently doing nothing.

## Introspection

`/_fakecloud/service-quotas/*` reads and changes quota state without going
through the AWS API, and is wrapped as the `serviceQuotas` sub-client in every
[SDK](/docs/sdks/). JSON is camelCase; errors are `{"error": "..."}` with 400
(bad input), 404 (unknown quota, service or request) or 409 (request already
decided). An omitted `accountId` or `region` means the server's.

| Endpoint | What it does |
|---|---|
| `GET /_fakecloud/service-quotas/quotas?accountId=&region=&serviceCode=` | Every quota (or one service's) with `defaultValue`, `appliedValue`, `usage` (when fakecloud counts it), `enforceable`, `enforced` and `enforcementSource` (`not_enforceable`, `account_override`, `override`, `global`) |
| `PUT /_fakecloud/service-quotas/quotas/{service}/{quota}` | Body `{accountId?, region?, value?, enforce?}`. Sets the applied value, **even below the AWS default** (AWS never lowers a quota; a test can, to hit a limit without creating the default number of resources). `enforce`: `true` enforces, `false` ignores, `null` clears the override, absent leaves it. With `accountId` the override is for that account only, otherwise server-wide |
| `DELETE /_fakecloud/service-quotas/quotas/{service}/{quota}?accountId=&region=` | Back to the AWS default, and drops the override (the account's when `accountId` is given, else the server-wide one) |
| `GET` / `PUT /_fakecloud/service-quotas/enforcement` | `{enforceAll, overrides, accountOverrides}`; `PUT` takes `{enforceAll?, overrides?: [{serviceCode, quotaCode, accountId?, enforce}]}` (`null` clears) and applies the batch only if every entry is valid |
| `GET` / `PUT /_fakecloud/service-quotas/request-approval` | `{mode: "auto" \| "manual"}` |
| `GET /_fakecloud/service-quotas/requests?accountId=&status=` | Increase requests across accounts, newest first |
| `POST /_fakecloud/service-quotas/requests/{id}/approve` | Approve a `PENDING` or `CASE_OPENED` request, raising the applied value |
| `POST /_fakecloud/service-quotas/requests/{id}/deny` | Body `{status?}`: `DENIED` (default), `NOT_APPROVED`, `CASE_CLOSED` or `INVALID_REQUEST` |

For example, to test how an app handles `SecurityGroupsPerInterfaceLimitExceeded`
with a single group:

```sh
curl -X PUT localhost:4566/_fakecloud/service-quotas/quotas/vpc/L-2AFB9258 \
  -H 'content-type: application/json' -d '{"value": 1, "enforce": true}'
```

### Manual request approval

With `--quota-requests manual` (or `PUT .../request-approval {"mode":"manual"}`),
`RequestServiceQuotaIncrease` and template entries stay `PENDING`, so code that
polls `GetRequestedServiceQuotaChange` can be tested through every status:
open a support case with `CreateSupportCase`, then approve or deny the request
through the endpoints above. A value AWS would never approve (past the quota's
maximum or the security-group product limit) is still `NOT_APPROVED` on
submission, and approving a request that stopped being approvable while it
waited returns 409; deny it instead.

## Known limitations

- Only the two security-group quotas are enforceable. Other catalog quotas are
  reported and can be raised or lowered, but fakecloud does not refuse
  requests that go past them.
- The catalog covers the services above, not every AWS service.
