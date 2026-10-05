#!/usr/bin/env python3
"""Regenerate the address ranges of the AWS-managed prefix lists.

Source of truth: AWS's published IP address ranges,
https://ip-ranges.amazonaws.com/ip-ranges.json (documented at
https://docs.aws.amazon.com/vpc/latest/userguide/aws-ip-ranges.html). We keep
the services behind an AWS-managed prefix list and emit
`crates/fakecloud-ec2/src/service/aws_prefix_lists/ranges.json`:

    {"syncToken": ..., "createDate": ...,
     "ipv4": {"<SERVICE>": {"<region>": ["<cidr>", ...]}},
     "ipv6": {"<SERVICE>": {"<region>": ["<cidr>", ...]}}}

fakecloud-ec2 decides per list which regions' ranges belong to it (a
regional list takes its region's ranges, the CloudFront list all of them).

Run:  python3 scripts/gen-aws-prefix-lists.py
(network: downloads ip-ranges.json; no AWS creds needed.)
"""
import json
import os
import urllib.request

URL = "https://ip-ranges.amazonaws.com/ip-ranges.json"
OUT = os.path.join(
    os.path.dirname(__file__),
    "..",
    "crates",
    "fakecloud-ec2",
    "src",
    "service",
    "aws_prefix_lists",
    "ranges.json",
)

# ip-ranges.json services backing an AWS-managed prefix list, per family. The
# S3 and DynamoDB gateway-endpoint lists are IPv4-only.
SERVICES = {
    "ipv4": [
        "CLOUDFRONT_ORIGIN_FACING",
        "DYNAMODB",
        "EC2_INSTANCE_CONNECT",
        "ROUTE53_HEALTHCHECKS",
        "S3",
    ],
    "ipv6": [
        "CLOUDFRONT_ORIGIN_FACING",
        "EC2_INSTANCE_CONNECT",
        "ROUTE53_HEALTHCHECKS",
    ],
}


def collect(prefixes, key, services):
    out = {s: {} for s in services}
    for p in prefixes:
        service = p["service"]
        if service not in out:
            continue
        cidrs = out[service].setdefault(p["region"], [])
        if p[key] not in cidrs:
            cidrs.append(p[key])
    for regions in out.values():
        for cidrs in regions.values():
            cidrs.sort()
    return {s: dict(sorted(r.items())) for s, r in out.items()}


def main() -> int:
    with urllib.request.urlopen(URL) as resp:
        data = json.load(resp)
    out = {
        "syncToken": data["syncToken"],
        "createDate": data["createDate"],
        "ipv4": collect(data["prefixes"], "ip_prefix", SERVICES["ipv4"]),
        "ipv6": collect(data["ipv6_prefixes"], "ipv6_prefix", SERVICES["ipv6"]),
    }
    with open(OUT, "w") as f:
        json.dump(out, f, indent=1, sort_keys=False)
        f.write("\n")
    print(f"wrote {OUT} (syncToken {data['syncToken']})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
