+++
title = "EKS"
description = "Amazon EKS (eks) on fakecloud: complete 70-op Elastic Kubernetes Service control plane — clusters, node groups, Fargate profiles, add-ons, access entries, identity-provider configs, pod identity, insights, capabilities, certificate authorities, and EKS Anywhere. restJson1."
weight = 46
+++

fakecloud implements **Amazon EKS** (`eks`), the managed Kubernetes service, as a
restJson1 control plane. **The complete 70-operation surface** ships — clusters,
managed node groups, Fargate profiles, add-ons, access entries, OIDC
identity-provider configs, pod-identity associations, upgrade insights,
capabilities, cluster certificate authorities, connected-cluster registration,
encryption config, and EKS Anywhere subscriptions — backed by account-partitioned state that persists across restarts
in persistent mode.

## Supported now (all 70 operations)

- **Cluster lifecycle** — `CreateCluster`, `DescribeCluster`, `ListClusters`,
  `DeleteCluster`. Clusters are created with the requested `roleArn`,
  `resourcesVpcConfig`, `version` (default 1.31), and tags, and transition
  `CREATING` -> `ACTIVE` on describe (deterministic, no background timer).
  Like EKS, `CreateCluster` (and `AWS::EKS::Cluster`) creates the cluster
  security group in EC2, in the VPC of the cluster's subnets: named
  `eks-cluster-sg-<cluster>-<id>`, tagged `aws:eks:cluster-name` and
  `kubernetes.io/cluster/<cluster>=owned`, with a self-referencing all-traffic
  ingress rule and an all-traffic egress rule. `resourcesVpcConfig` returns it
  as `clusterSecurityGroupId` (with the subnets' `vpcId`), it can be described,
  tagged and given rules through EC2, and `DeleteCluster` deletes it.
- **Cluster updates** — `UpdateClusterConfig`, `UpdateClusterVersion`, each
  minting a tracked `Update` that settles `InProgress` -> `Successful` on
  describe; `DescribeUpdate` and `ListUpdates` return the update history.
- **Managed node groups** — `CreateNodegroup`, `DescribeNodegroup`,
  `ListNodegroups`, `DeleteNodegroup`, plus `UpdateNodegroupConfig` and
  `UpdateNodegroupVersion` (tracked updates). Node groups carry `nodeRole`,
  `subnets`, `scalingConfig`, and transition `CREATING` -> `ACTIVE` on describe.
  Like EKS, each node group runs as a real EC2 Auto Scaling group
  (`resources.autoScalingGroups`), tagged `eks:cluster-name`,
  `eks:nodegroup-name` and the cluster-autoscaler discovery tags, with zones
  from the node group's subnets: it can be described and tagged through Auto
  Scaling, follows `UpdateNodegroupConfig` scaling changes, and is deleted with
  the node group. `UpdateNodegroupVersion` applies a `launchTemplate` version
  (the template's `id` / `name` must match the node group's).
- **Fargate profiles** — `CreateFargateProfile`, `DescribeFargateProfile`,
  `ListFargateProfiles`, `DeleteFargateProfile` with `podExecutionRoleArn` and
  `selectors`, their own `CREATING` -> `ACTIVE` transition.
- **Add-ons** — `CreateAddon`, `DescribeAddon`, `ListAddons`, `DeleteAddon`,
  `UpdateAddon` (tracked version updates), plus the read-only catalogue ops
  `DescribeAddonVersions` and `DescribeAddonConfiguration`. The catalogue
  carries the AWS-owned add-ons (vpc-cni, coredns, kube-proxy, the EBS/EFS/FSx/
  Mountpoint for S3 CSI drivers, snapshot-controller, eks-pod-identity-agent,
  aws-guardduty-agent, aws-secrets-store-csi-driver-provider,
  amazon-cloudwatch-observability, adot, eks-node-monitoring-agent,
  aws-network-flow-monitoring-agent) and the EKS-published community add-ons
  (metrics-server, kube-state-metrics, prometheus-node-exporter, cert-manager,
  external-dns), each with versions and per-Kubernetes-version compatibilities
  flagging the default version (coredns and kube-proxy track the cluster's
  Kubernetes minor). `kubernetesVersion`, `addonName`, `types`, `owners`, and
  `publishers` filter it, and `CreateAddon` without an `addonVersion` installs
  the default for the cluster's version. An add-on's
  `podIdentityAssociations` become real pod identity associations in the
  add-on's namespace, owned by the add-on (`ownerArn`): they resolve through
  `DescribePodIdentityAssociation`, an `UpdateAddon` keeps the association (and
  ARN) of a service account that stays, and `DeleteAddon` deletes them.
- **Access entries** — `CreateAccessEntry`, `DescribeAccessEntry`,
  `ListAccessEntries`, `DeleteAccessEntry`, `UpdateAccessEntry`, plus
  `AssociateAccessPolicy`, `DisassociateAccessPolicy`,
  `ListAssociatedAccessPolicies` (cluster/namespace `accessScope`), and the
  read-only `ListAccessPolicies` catalogue of the `AmazonEKS*` cluster-access
  policies.
- **OIDC identity-provider configs** — `AssociateIdentityProviderConfig` and
  `DisassociateIdentityProviderConfig` (each minting a tracked cluster `Update`),
  `DescribeIdentityProviderConfig`, `ListIdentityProviderConfigs`.
- **Pod-identity associations** — `CreatePodIdentityAssociation`,
  `DescribePodIdentityAssociation`, `ListPodIdentityAssociations`,
  `UpdatePodIdentityAssociation`, `DeletePodIdentityAssociation` (map a
  `namespace`/`serviceAccount` to a `roleArn`, `a-`-prefixed `associationId`).
- **Upgrade insights** — `ListInsights`, `DescribeInsight` (seeded `PASSING`
  UPGRADE_READINESS findings per cluster), `StartInsightsRefresh`,
  `DescribeInsightsRefresh`.
- **Capabilities** — `CreateCapability`, `DescribeCapability`, `ListCapabilities`,
  `UpdateCapability` (tracked `Update`), `DeleteCapability`.
- **Certificate authorities** — `CreateCertificateAuthority`,
  `DescribeCertificateAuthority`, `ListCertificateAuthorities`,
  `DeleteCertificateAuthority`, `ActivateCertificateAuthority`. Every cluster is
  created with an EKS-signed CA already `IN_USE`; a customer-created CA starts
  `NOT_USED`/`IN_PROGRESS`, settles to `COMPLETE` on describe, and activation
  promotes it while demoting the previous signer to `NOT_USED` with
  `rollbackAvailable`. Deleting the signing CA is refused with
  `ResourceInUseException`.
- **Connected clusters** — `RegisterCluster` (creates a `PENDING` cluster with a
  `connectorConfig`) and `DeregisterCluster`.
- **Cluster maintenance** — `AssociateEncryptionConfig` and `CancelUpdate` (both
  operate on tracked `Update` records), plus the read-only `DescribeClusterVersions`
  catalogue (Kubernetes 1.28-1.32 with platform versions and standard/extended
  support windows).
- **EKS Anywhere subscriptions** — `CreateEksAnywhereSubscription`,
  `DescribeEksAnywhereSubscription`, `ListEksAnywhereSubscriptions`,
  `UpdateEksAnywhereSubscription`, `DeleteEksAnywhereSubscription`
  (account-scoped, `term`/`licenseQuantity`/`autoRenew`).
- **Tagging** — `TagResource`, `UntagResource`, `ListTagsForResource` (EKS uses
  a `map<String,String>` tag shape, keyed by resource ARN).

100% conformance across the full surface: all 1,995 generated Smithy probe
variants for the 70 operations pass. There is no real Kubernetes API-server
endpoint; the control plane models the AWS management API, not `kubectl` traffic.

## Example

```python
import boto3
eks = boto3.client("eks", endpoint_url="http://localhost:4566")

eks.create_cluster(
    name="app",
    roleArn="arn:aws:iam::123456789012:role/eksClusterRole",
    resourcesVpcConfig={"subnetIds": ["subnet-1", "subnet-2"]},
    version="1.31",
)

cluster = eks.describe_cluster(name="app")["cluster"]
print(cluster["status"], cluster["version"])  # ACTIVE 1.31
```
