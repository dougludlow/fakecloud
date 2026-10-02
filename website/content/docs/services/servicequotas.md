+++
title = "Service Quotas"
description = "Service Quotas on fakecloud: all 26 operations, a catalog of real quota codes with AWS default values, increase requests that raise the applied value, the Organizations quota request template, tags, automatic management and utilization reports. EC2 enforces the security-group quotas at their applied values."
weight = 81
+++

fakecloud implements **Service Quotas** (`servicequotas`). All **26 operations**
from the AWS Smithy model ship, backed by account-partitioned state that
persists across restarts in persistent mode. The wire protocol is awsJson1.1
(x-amz-target `ServiceQuotasV20190624.<Op>`), signing as `servicequotas`.

Quotas are not just reported: EC2 reads the applied value of the
security-group quotas from Service Quotas, so raising one with
`RequestServiceQuotaIncrease` changes what EC2 accepts.

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

- **`RequestServiceQuotaIncrease`** returns the request as `PENDING` and
  decides it straight away. It is `APPROVED` and the applied value raised,
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
- **`CreateSupportCase`** opens a case for a `PENDING` request
  (`InvalidResourceStateException` for any other status).

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
  terms as `RequestServiceQuotaIncrease`. Accounts that were already members, or
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

## Enforcement in EC2

| Quota | Enforced by |
|---|---|
| Security groups per network interface (`vpc`/`L-2AFB9258`) | `CreateNetworkInterface`, `ModifyNetworkInterfaceAttribute` (`SecurityGroupsPerInterfaceLimitExceeded`); `RunInstances`, `ModifyInstanceAttribute`, Auto Scaling and CloudFormation launches (`SecurityGroupsPerInstanceLimitExceeded`); `ValidateSecurityGroupQuotasForInterface` |
| Inbound or outbound rules per security group (`vpc`/`L-0EA8095F`) | `AuthorizeSecurityGroupIngress`, `AuthorizeSecurityGroupEgress`, `ModifySecurityGroupRules`, `ValidateSecurityGroupQuotasForInterface` (`RulesPerSecurityGroupLimitExceeded`) |

As on AWS, the rules quota applies to each direction separately and counts
IPv4 and IPv6 rules separately. A rule that references a security group counts
toward both. A rule that references a customer-managed prefix list counts as
the list's maximum number of entries, toward the list's address family.
`ModifyManagedPrefixList` honours this too: a larger `MaxEntries` that would
push a referencing group over the quota leaves the list at its old size in
`modify-failed`, with the group ids in `StateMessage`.

## Known limitations

- Increase requests are decided immediately. Nothing waits on a support case,
  so a request never stays `PENDING` or reaches `CASE_OPENED`.
- Only the two security-group quotas are enforced. Other catalog quotas are
  reported and can be raised, but fakecloud does not refuse requests that go
  past them.
- The catalog covers the services above, not every AWS service.
