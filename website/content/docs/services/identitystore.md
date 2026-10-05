+++
title = "IAM Identity Center Identity Store"
description = "AWS IAM Identity Center Identity Store (identitystore) on fakecloud: identity stores, users, groups, group memberships, revisions, attribute lookups, and IsMemberInGroups. awsJson1.1."
weight = 49
+++

fakecloud implements the **AWS IAM Identity Center Identity Store**
(`identitystore`) as an awsJson1.1 control plane. **The complete 22-operation
surface** ships: the identity stores themselves, plus the users, groups, and
memberships that make up each directory, backed by account-partitioned state that persists across restarts in persistent
mode.

An Identity Store is a per-account directory keyed by `IdentityStoreId`
(`d-xxxxxxxxxx`). Real AWS provisions the store when an IAM Identity Center
instance is enabled, and fakecloud reads the store ids from the
[SSO Admin](@/docs/services/ssoadmin.md) instances (including the default
instance seeded for the configured account). fakecloud also creates a
directory lazily on the first write to any store id, so the directory API is
usable immediately. Every `IdentityStoreId` input accepts either the bare id or
the store ARN (`arn:aws:identitystore::<account>:identitystore/d-xxxxxxxxxx`). Nested SCIM attribute bags (`Name`,
`Emails`, `Addresses`, `PhoneNumbers`, `Photos`, `Roles`, ...) are stored as
submitted and round-trip verbatim on describe. The model's `@length` and
`@range` constraints (IdentityStoreId 1-93, ResourceId 1-47, UserName 1-128,
free-form profile attributes 1-1024, `MaxResults` 1-100, `NextToken` 1-65535)
are enforced with `ValidationException`.

## Supported now (all 22 operations)

- **Identity stores**: `ListIdentityStores`, `DescribeIdentityStore`,
  `UpdateIdentityStore`. Lists every store the account owns (its Identity
  Center instances' stores plus any directory created by a write) with its
  `IdentityStoreArn`. `UpdateIdentityStore` replaces the store's
  `NetworkConfiguration` (`VpceAccessRequired`, `ApiRestrictSourceVpcs`,
  `ApiAllowSourceIps`, `ScimAllowSourceIps`, validated against the model's VPC
  id and CIDR patterns) and `DescribeIdentityStore` returns it. The
  configuration is stored and reported but not enforced: fakecloud has no VPC
  endpoint or source-IP context to filter requests on. An unknown store returns
  `ResourceNotFoundException` with `ResourceType: IDENTITY_STORE`.
- **Resource ids and ARNs**: users, groups, and memberships created in a
  store with a canonical `d-<10 hex>` id get AWS's `<10 hex>-<UUID>` ids, and
  every response carries the resource ARN (`UserArn`, `GroupArn`,
  `MembershipArn`, in the global form `arn:aws:identitystore:::user/<id>`).
- **Revisions**: users and groups carry a `Revision` that starts at `1` and
  increments on every `UpdateUser` / `UpdateGroup` (whose responses now return
  the id, store id, ARN, and new revision). Passing `Revision` to
  `UpdateUser`/`UpdateGroup`/`DeleteUser`/`DeleteGroup` makes the call
  conditional: a stale value returns `ConflictException` with
  `Reason: CONCURRENT_MODIFICATION` (uniqueness conflicts carry
  `Reason: UNIQUENESS_CONSTRAINT_VIOLATION`).
- **Users** — `CreateUser`, `DescribeUser`, `UpdateUser`, `DeleteUser`,
  `ListUsers`, `GetUserId`. `UserName` is unique per store (duplicate ->
  `ConflictException`). `UpdateUser` applies SCIM `AttributeOperations`
  (add/replace/remove) over dotted attribute paths. `ListUsers` supports the
  legacy equality `Filters` shape and `MaxResults`/`NextToken` pagination.
- **Groups** — `CreateGroup`, `DescribeGroup`, `UpdateGroup`, `DeleteGroup`,
  `ListGroups`, `GetGroupId`. `DisplayName` is unique per store. Deleting a
  group also removes its memberships.
- **Group memberships** — `CreateGroupMembership`, `DescribeGroupMembership`,
  `DeleteGroupMembership`, `GetGroupMembershipId`, `ListGroupMemberships`,
  `ListGroupMembershipsForMember`. A membership links a `MemberId` (`{UserId}`)
  into a group; the referenced user and group must exist, and a duplicate pair
  returns `ConflictException`.
- **Attribute lookups** — `GetUserId` / `GetGroupId` resolve an
  `AlternateIdentifier`'s `UniqueAttribute` (e.g. `UserName` / `DisplayName`) to
  the resource id; unknown identifiers return `ResourceNotFoundException`.
- **Membership test** — `IsMemberInGroups` returns, for each queried group id,
  whether the member belongs to it.

## Not implemented

- `ExternalId`-based `AlternateIdentifier` lookups (no external identity
  provider is modeled), and the SCIM external-id/attribute provisioning that a
  real Sync engine would drive. The paired **SSO Admin** control plane
  (instances, permission sets, account assignments, applications) is a separate
  service.
