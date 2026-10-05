#!/usr/bin/env python3
"""Regenerate the EKS add-on catalog served by DescribeAddonVersions.

Source of truth: the `plumdog/eks-addon-configuration` repo, which captures the
real `aws eks describe-addon-versions` output once a day and commits it
(`data/data.json`, keyed by `./<addon>/addon.json`). Pinning a capture commit
gives a reproducible snapshot of what AWS returned on that day: every add-on
version string, its architectures, compute types, per-cluster-version
compatibilities (including the `defaultVersion` flag), and the
`requiresIamPermissions` / `requiresConfiguration` flags.

The snapshot is pinned to the same date as fakecloud's DescribeClusterVersions
table (`CLUSTER_VERSIONS` in `crates/fakecloud-eks/src/eks_helpers.rs`), so the
two stay consistent: only the AWS-owned and EKS-published community add-ons are
kept (Marketplace listings depend on third-party subscriptions), and only the
compatibilities for the cluster versions in that table.

Run:  python3 scripts/generate-eks-addon-catalog.py
(network: downloads one capture file from GitHub; no AWS credentials needed.)
"""
import json
import os
import urllib.request

CAPTURE_REPO = "plumdog/eks-addon-configuration"
# Capture taken 2025-11-25T00:35:57Z, the last daily capture before Kubernetes
# 1.28 left extended support (2025-11-26). Matches the cluster-version pin.
CAPTURE_COMMIT = "1a07655bd15e8759a55b2f514b9a4c08c2b5d647"
CAPTURED_AT = "2025-11-25T00:35:57Z"
CLUSTER_VERSIONS = ["1.28", "1.29", "1.30", "1.31", "1.32", "1.33", "1.34"]
OWNERS = ("aws", "community")

OUT = os.path.join(
    os.path.dirname(__file__),
    "..",
    "crates",
    "fakecloud-eks",
    "src",
    "addon_catalog.json",
)


def load_capture():
    url = (
        f"https://raw.githubusercontent.com/{CAPTURE_REPO}/"
        f"{CAPTURE_COMMIT}/data/data.json"
    )
    with urllib.request.urlopen(url) as resp:
        return json.load(resp)


def trim(capture):
    addons = []
    for key, addon in sorted(capture.items()):
        if not key.endswith("/addon.json") or addon.get("owner") not in OWNERS:
            continue
        versions = []
        for ver in addon["addonVersions"]:
            compat = [
                {
                    "clusterVersion": c["clusterVersion"],
                    "platformVersions": c["platformVersions"],
                    "defaultVersion": c["defaultVersion"],
                }
                for c in ver["compatibilities"]
                if c["clusterVersion"] in CLUSTER_VERSIONS
            ]
            if not compat:
                continue
            versions.append(
                {
                    "addonVersion": ver["addonVersion"],
                    "architecture": ver["architecture"],
                    "computeTypes": ver["computeTypes"],
                    "compatibilities": compat,
                    "requiresConfiguration": ver["requiresConfiguration"],
                    "requiresIamPermissions": ver["requiresIamPermissions"],
                }
            )
        if not versions:
            continue
        addons.append(
            {
                "addonName": addon["addonName"],
                "type": addon["type"],
                "owner": addon["owner"],
                "publisher": addon["publisher"],
                "defaultNamespace": addon["defaultNamespace"],
                "addonVersions": versions,
            }
        )
    return addons


def render(addons):
    """One add-on header per line and one add-on version per line, so a
    refresh diffs as added/removed versions rather than one giant line."""
    dump = lambda v: json.dumps(v, separators=(",", ":"))
    lines = ["{"]
    lines.append(
        '"source":'
        + dump(
            {
                "repo": f"https://github.com/{CAPTURE_REPO}",
                "commit": CAPTURE_COMMIT,
                "capturedAt": CAPTURED_AT,
            }
        )
        + ","
    )
    lines.append('"addons":[')
    for i, addon in enumerate(addons):
        header = {k: v for k, v in addon.items() if k != "addonVersions"}
        lines.append(dump(header)[:-1] + ',"addonVersions":[')
        vers = addon["addonVersions"]
        for j, ver in enumerate(vers):
            lines.append(dump(ver) + ("," if j < len(vers) - 1 else ""))
        lines.append("]}" + ("," if i < len(addons) - 1 else ""))
    lines.append("]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main():
    addons = trim(load_capture())
    text = render(addons)
    json.loads(text)  # sanity: the hand-rendered layout is valid JSON
    with open(OUT, "w") as f:
        f.write(text)
    print(f"wrote {len(addons)} add-ons to {os.path.normpath(OUT)}")


if __name__ == "__main__":
    main()
