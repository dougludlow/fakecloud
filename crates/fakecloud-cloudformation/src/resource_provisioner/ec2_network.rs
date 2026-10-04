//! Standalone VPC networking resources: `AWS::EC2::SecurityGroupIngress`,
//! `SecurityGroupEgress`, `VPCGatewayAttachment`, `Route`, `EIP` and
//! `NatGateway`. Each creates and deletes through the real EC2 handlers (the
//! same ones the API dispatches to), so a stack's ingress rule, IGW
//! attachment, default route, Elastic IP and NAT gateway exist in EC2 instead
//! of being recorded with no backing state.

use std::collections::HashMap;

use serde_json::Value;

use super::{ProvisionResult, ResourceDefinition, ResourceProvisioner, StackResource};

fn prop_str<'a>(props: &'a Value, key: &str) -> Option<&'a str> {
    props.get(key).and_then(|v| v.as_str())
}

fn num_str(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The text of the first `<tag>...</tag>` element of an EC2 query response.
fn xml_elem(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].to_string())
}

/// CFN `Tags` as `TagSpecification.1.*` params for a create call.
fn tag_spec_params(props: &Value, resource_type: &str, params: &mut HashMap<String, String>) {
    let Some(tags) = props.get("Tags").and_then(|v| v.as_array()) else {
        return;
    };
    params.insert(
        "TagSpecification.1.ResourceType".to_string(),
        resource_type.to_string(),
    );
    for (i, t) in tags.iter().enumerate() {
        if let (Some(k), Some(v)) = (prop_str(t, "Key"), prop_str(t, "Value")) {
            params.insert(format!("TagSpecification.1.Tag.{}.Key", i + 1), k.to_string());
            params.insert(
                format!("TagSpecification.1.Tag.{}.Value", i + 1),
                v.to_string(),
            );
        }
    }
}

/// The route destination a CFN `AWS::EC2::Route` names, as the
/// `(query param, value)` CreateRoute / DeleteRoute take.
fn route_destination(props: &Value) -> Option<(&'static str, String)> {
    [
        "DestinationCidrBlock",
        "DestinationIpv6CidrBlock",
        "DestinationPrefixListId",
    ]
    .into_iter()
    .find_map(|k| prop_str(props, k).map(|v| (k, v.to_string())))
}

/// The query param a route destination value goes in, recovered from the
/// value alone (for delete, which only has the physical id).
fn destination_param(dest: &str) -> &'static str {
    if dest.starts_with("pl-") {
        "DestinationPrefixListId"
    } else if dest.contains(':') {
        "DestinationIpv6CidrBlock"
    } else {
        "DestinationCidrBlock"
    }
}

impl ResourceProvisioner {
    /// `AWS::EC2::SecurityGroupIngress` / `SecurityGroupEgress`: one rule
    /// authorized on the group. `Ref` returns the security group rule id.
    pub(super) fn create_ec2_sg_rule(
        &self,
        resource: &ResourceDefinition,
        egress: bool,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let group_id = match prop_str(props, "GroupId") {
            Some(id) => id.to_string(),
            // Ingress may name a default-VPC group by name instead.
            None => {
                let name = prop_str(props, "GroupName")
                    .filter(|_| !egress)
                    .ok_or("SecurityGroup rule requires GroupId")?;
                let accounts = self.ec2_state.read();
                let state = accounts
                    .get(&self.account_id)
                    .ok_or_else(|| format!("Security group {name} does not exist"))?;
                let default_vpc = state
                    .vpcs
                    .values()
                    .find(|v| v.is_default)
                    .map(|v| v.vpc_id.clone())
                    .unwrap_or_default();
                state
                    .security_groups
                    .values()
                    .find(|g| g.group_name == name && g.vpc_id == default_vpc)
                    .map(|g| g.group_id.clone())
                    .ok_or_else(|| format!("Security group {name} does not exist"))?
            }
        };
        let mut params = HashMap::new();
        params.insert("GroupId".to_string(), group_id.clone());
        let p = "IpPermissions.1";
        params.insert(
            format!("{p}.IpProtocol"),
            prop_str(props, "IpProtocol").unwrap_or("-1").to_string(),
        );
        if let Some(from) = props.get("FromPort").and_then(num_str) {
            params.insert(format!("{p}.FromPort"), from);
        }
        if let Some(to) = props.get("ToPort").and_then(num_str) {
            params.insert(format!("{p}.ToPort"), to);
        }
        let description = prop_str(props, "Description");
        let mut source = |kind: &str, field: &str, value: &str| {
            params.insert(format!("{p}.{kind}.1.{field}"), value.to_string());
            if let Some(d) = description {
                params.insert(format!("{p}.{kind}.1.Description"), d.to_string());
            }
        };
        if let Some(cidr) = prop_str(props, "CidrIp") {
            source("IpRanges", "CidrIp", cidr);
        } else if let Some(cidr) = prop_str(props, "CidrIpv6") {
            source("Ipv6Ranges", "CidrIpv6", cidr);
        } else if let Some(g) = prop_str(props, "SourceSecurityGroupId")
            .or_else(|| prop_str(props, "DestinationSecurityGroupId"))
        {
            source("Groups", "GroupId", g);
        } else if let Some(pl) = prop_str(props, "SourcePrefixListId")
            .or_else(|| prop_str(props, "DestinationPrefixListId"))
        {
            source("PrefixListIds", "PrefixListId", pl);
        } else if let Some(name) = prop_str(props, "SourceSecurityGroupName") {
            source("Groups", "GroupName", name);
        }
        let action = if egress {
            "AuthorizeSecurityGroupEgress"
        } else {
            "AuthorizeSecurityGroupIngress"
        };
        let body = self.ec2_dispatch(action, params)?;
        let rule_id = xml_elem(&body, "securityGroupRuleId")
            .ok_or_else(|| format!("{action} returned no securityGroupRuleId"))?;
        Ok(ProvisionResult::new(rule_id.clone()).with("Id", rule_id))
    }

    /// Revoke a standalone SG rule by its rule id.
    fn delete_ec2_sg_rule(&self, rule_id: &str, egress: bool) {
        let group_id = {
            let accounts = self.ec2_state.read();
            accounts.get(&self.account_id).and_then(|s| {
                s.security_groups
                    .values()
                    .find(|g| g.rules.iter().any(|r| r.rule_id == rule_id))
                    .map(|g| g.group_id.clone())
            })
        };
        let Some(group_id) = group_id else {
            return;
        };
        let mut params = HashMap::new();
        params.insert("GroupId".to_string(), group_id);
        params.insert("SecurityGroupRuleId.1".to_string(), rule_id.to_string());
        let action = if egress {
            "RevokeSecurityGroupEgress"
        } else {
            "RevokeSecurityGroupIngress"
        };
        let _ = self.ec2_dispatch(action, params);
    }

    /// `AWS::EC2::VPCGatewayAttachment`: attach an internet or VPN gateway to
    /// a VPC. The physical id is `IGW|<vpc>` / `VGW|<vpc>`, as on AWS.
    pub(super) fn create_ec2_vpc_gateway_attachment(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let vpc_id = prop_str(props, "VpcId").ok_or("VPCGatewayAttachment requires VpcId")?;
        let mut params = HashMap::new();
        params.insert("VpcId".to_string(), vpc_id.to_string());
        let kind = if let Some(igw) = prop_str(props, "InternetGatewayId") {
            params.insert("InternetGatewayId".to_string(), igw.to_string());
            self.ec2_dispatch("AttachInternetGateway", params)?;
            "IGW"
        } else if let Some(vgw) = prop_str(props, "VpnGatewayId") {
            params.insert("VpnGatewayId".to_string(), vgw.to_string());
            self.ec2_dispatch("AttachVpnGateway", params)?;
            "VGW"
        } else {
            return Err(
                "VPCGatewayAttachment requires InternetGatewayId or VpnGatewayId".to_string(),
            );
        };
        Ok(ProvisionResult::new(format!("{kind}|{vpc_id}")))
    }

    /// Detach the gateway a `VPCGatewayAttachment` attached.
    fn delete_ec2_vpc_gateway_attachment(&self, physical_id: &str) {
        let Some((kind, vpc_id)) = physical_id.split_once('|') else {
            return;
        };
        let gateway = {
            let accounts = self.ec2_state.read();
            accounts.get(&self.account_id).and_then(|s| match kind {
                "IGW" => s
                    .internet_gateways
                    .values()
                    .find(|g| g.attachments.iter().any(|(v, _)| v == vpc_id))
                    .map(|g| g.internet_gateway_id.clone()),
                _ => s
                    .vpn_gateways
                    .values()
                    .find(|g| g.attachments.iter().any(|v| v == vpc_id))
                    .map(|g| g.id.clone()),
            })
        };
        let Some(gateway) = gateway else {
            return;
        };
        let mut params = HashMap::new();
        params.insert("VpcId".to_string(), vpc_id.to_string());
        let action = if kind == "IGW" {
            params.insert("InternetGatewayId".to_string(), gateway);
            "DetachInternetGateway"
        } else {
            params.insert("VpnGatewayId".to_string(), gateway);
            "DetachVpnGateway"
        };
        let _ = self.ec2_dispatch(action, params);
    }

    /// `AWS::EC2::Route`: a route in a route table. The physical id is
    /// `<route table>|<destination>`.
    pub(super) fn create_ec2_route(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let rtb = prop_str(props, "RouteTableId").ok_or("Route requires RouteTableId")?;
        let (dest_key, dest) = route_destination(props).ok_or(
            "Route requires DestinationCidrBlock, DestinationIpv6CidrBlock or DestinationPrefixListId",
        )?;
        let mut params = HashMap::new();
        params.insert("RouteTableId".to_string(), rtb.to_string());
        params.insert(dest_key.to_string(), dest.clone());
        for target in [
            "GatewayId",
            "NatGatewayId",
            "InstanceId",
            "NetworkInterfaceId",
            "VpcPeeringConnectionId",
            "TransitGatewayId",
            "EgressOnlyInternetGatewayId",
            "CarrierGatewayId",
            "LocalGatewayId",
            "VpcEndpointId",
            "CoreNetworkArn",
        ] {
            if let Some(v) = prop_str(props, target) {
                params.insert(target.to_string(), v.to_string());
            }
        }
        self.ec2_dispatch("CreateRoute", params)?;
        Ok(ProvisionResult::new(format!("{rtb}|{dest}")))
    }

    fn delete_ec2_route(&self, physical_id: &str) {
        let Some((rtb, dest)) = physical_id.split_once('|') else {
            return;
        };
        let mut params = HashMap::new();
        params.insert("RouteTableId".to_string(), rtb.to_string());
        params.insert(destination_param(dest).to_string(), dest.to_string());
        let _ = self.ec2_dispatch("DeleteRoute", params);
    }

    /// `AWS::EC2::EIP`: allocate an Elastic IP (and associate it with
    /// `InstanceId` when given). `Ref` returns the public IP.
    pub(super) fn create_ec2_eip(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let mut params = HashMap::new();
        params.insert(
            "Domain".to_string(),
            prop_str(props, "Domain").unwrap_or("vpc").to_string(),
        );
        for key in ["NetworkBorderGroup", "PublicIpv4Pool", "Address", "IpamPoolId"] {
            if let Some(v) = prop_str(props, key) {
                params.insert(key.to_string(), v.to_string());
            }
        }
        tag_spec_params(props, "elastic-ip", &mut params);
        let body = self.ec2_dispatch("AllocateAddress", params)?;
        let public_ip = xml_elem(&body, "publicIp").ok_or("AllocateAddress returned no publicIp")?;
        let allocation_id =
            xml_elem(&body, "allocationId").ok_or("AllocateAddress returned no allocationId")?;
        if let Some(instance) = prop_str(props, "InstanceId") {
            let mut assoc = HashMap::new();
            assoc.insert("AllocationId".to_string(), allocation_id.clone());
            assoc.insert("InstanceId".to_string(), instance.to_string());
            if let Err(e) = self.ec2_dispatch("AssociateAddress", assoc) {
                self.release_eip(&allocation_id);
                return Err(e);
            }
        }
        Ok(ProvisionResult::new(public_ip.clone())
            .with("PublicIp", public_ip)
            .with("AllocationId", allocation_id))
    }

    fn release_eip(&self, allocation_id: &str) {
        let mut params = HashMap::new();
        params.insert("AllocationId".to_string(), allocation_id.to_string());
        let _ = self.ec2_dispatch("ReleaseAddress", params);
    }

    /// Disassociate (when associated) and release a stack's Elastic IP.
    fn delete_ec2_eip(&self, public_ip: &str) {
        let found = {
            let accounts = self.ec2_state.read();
            accounts.get(&self.account_id).and_then(|s| {
                s.elastic_ips
                    .values()
                    .find(|e| e.public_ip == public_ip || e.allocation_id == public_ip)
                    .map(|e| (e.allocation_id.clone(), e.association_id.clone()))
            })
        };
        let Some((allocation_id, association_id)) = found else {
            return;
        };
        if let Some(association_id) = association_id {
            let mut params = HashMap::new();
            params.insert("AssociationId".to_string(), association_id);
            let _ = self.ec2_dispatch("DisassociateAddress", params);
        }
        self.release_eip(&allocation_id);
    }

    /// `AWS::EC2::NatGateway`: a NAT gateway in a subnet. `Ref` returns the
    /// NAT gateway id.
    pub(super) fn create_ec2_nat_gateway(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let subnet = prop_str(props, "SubnetId").ok_or("NatGateway requires SubnetId")?;
        let connectivity = prop_str(props, "ConnectivityType").unwrap_or("public");
        let mut params = HashMap::new();
        params.insert("SubnetId".to_string(), subnet.to_string());
        params.insert("ConnectivityType".to_string(), connectivity.to_string());
        if let Some(alloc) = prop_str(props, "AllocationId") {
            params.insert("AllocationId".to_string(), alloc.to_string());
        } else if connectivity == "public" {
            return Err("A public NatGateway requires AllocationId".to_string());
        }
        if let Some(ip) = prop_str(props, "PrivateIpAddress") {
            params.insert("PrivateIpAddress".to_string(), ip.to_string());
        }
        tag_spec_params(props, "natgateway", &mut params);
        let body = self.ec2_dispatch("CreateNatGateway", params)?;
        let id =
            xml_elem(&body, "natGatewayId").ok_or("CreateNatGateway returned no natGatewayId")?;
        Ok(ProvisionResult::new(id.clone()).with("NatGatewayId", id))
    }

    /// Delete the backing EC2 state of a standalone networking resource.
    pub(super) fn delete_ec2_network_resource(&self, resource: &StackResource) {
        let id = resource.physical_id.as_str();
        match resource.resource_type.as_str() {
            "AWS::EC2::SecurityGroupIngress" => self.delete_ec2_sg_rule(id, false),
            "AWS::EC2::SecurityGroupEgress" => self.delete_ec2_sg_rule(id, true),
            "AWS::EC2::VPCGatewayAttachment" => self.delete_ec2_vpc_gateway_attachment(id),
            "AWS::EC2::Route" => self.delete_ec2_route(id),
            "AWS::EC2::EIP" => self.delete_ec2_eip(id),
            "AWS::EC2::NatGateway" => {
                let mut params = HashMap::new();
                params.insert("NatGatewayId".to_string(), id.to_string());
                let _ = self.ec2_dispatch("DeleteNatGateway", params);
            }
            _ => {}
        }
    }
}
