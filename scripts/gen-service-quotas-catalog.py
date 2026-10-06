#!/usr/bin/env python3
"""Regenerate the Service Quotas catalog from AWS's published default quotas.

Source of truth: the Service Quotas API itself. `ListServices` names every
service that has quotas and `ListAWSDefaultServiceQuotas` returns each one's
default quotas (code, name, description, value, unit, adjustability, global
flag, period, usage metric, quota context). fakecloud vendors the result as
`crates/fakecloud-servicequotas/data/quotas.json.gz`, which
`fakecloud_servicequotas::catalog` decodes once at first use. The quotas
fakecloud enforces and the documented maximum values (neither is in the API)
are a hand-maintained overlay in `catalog.rs`.

Two steps:

  1. dump: call the AWS CLI and write the raw responses to a directory
     (`services.json` plus `defaults/<service-code>.json`, exactly what
     `aws service-quotas list-services` and
     `aws service-quotas list-aws-default-service-quotas --service-code X`
     print). Needs credentials allowed to call `servicequotas:ListServices`
     and `servicequotas:ListAWSDefaultServiceQuotas`; defaults do not depend
     on the account, only on the region (use us-east-1, the region the
     vendored catalog is taken from).

       python3 scripts/gen-service-quotas-catalog.py dump DUMP_DIR \\
           [--profile PROFILE] [--region us-east-1]

  2. build: turn a dump directory into the vendored data file. Deterministic:
     services sorted by code, quotas by quota code, gzip with no timestamp,
     so rebuilding an unchanged dump produces an identical file.

       python3 scripts/gen-service-quotas-catalog.py build DUMP_DIR \\
           [--out crates/fakecloud-servicequotas/data/quotas.json.gz]

`all` runs both (dump into DUMP_DIR, then build from it).

Vendored schema (compact JSON, gzipped):

    {"region": "us-east-1",
     "services": [{"ServiceCode": ..., "ServiceName": ...,
                   "Quotas": [{"QuotaCode", "QuotaName", "Description",
                               "Value", "Unit", "Adjustable", "GlobalQuota",
                               "Period"?, "UsageMetric"?, "QuotaContext"?}]}]}

Member names are the API's. `QuotaArn`, `ServiceCode` and `ServiceName` are
dropped per quota: the ARN is derived from the region, account and codes, and
the service name is kept once per service. Strings (names, descriptions) are
kept verbatim, including any non-ASCII characters AWS returns.
"""
import argparse
import gzip
import io
import json
import os
import subprocess
import sys
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
DEFAULT_OUT = os.path.join(
    ROOT, "crates", "fakecloud-servicequotas", "data", "quotas.json.gz"
)

# Per-quota members kept in the vendored file, in output order.
QUOTA_FIELDS = (
    "QuotaCode",
    "QuotaName",
    "Description",
    "Value",
    "Unit",
    "Adjustable",
    "GlobalQuota",
    "Period",
    "UsageMetric",
    "QuotaContext",
)
REQUIRED_FIELDS = (
    "QuotaCode",
    "QuotaName",
    "Value",
    "Unit",
    "Adjustable",
    "GlobalQuota",
)

RETRYABLE = (
    "Throttling",
    "TooManyRequests",
    "RequestLimitExceeded",
    "ServiceException",
    "InternalServerError",
    "ServiceUnavailable",
    "Could not connect",
    "Read timeout",
    "Connection was closed",
)


def aws(args, profile, region, attempts=8):
    """Run `aws <args> --output json` and return the parsed output, retrying
    throttling and transient errors with exponential backoff. The CLI follows
    NextToken itself, so the result holds every page."""
    cmd = ["aws", *args, "--region", region, "--output", "json"]
    if profile:
        cmd += ["--profile", profile]
    env = dict(os.environ, AWS_RETRY_MODE="adaptive", AWS_MAX_ATTEMPTS="10")
    delay = 1.0
    for attempt in range(1, attempts + 1):
        proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
        if proc.returncode == 0:
            return json.loads(proc.stdout or "{}")
        err = proc.stderr.strip()
        if attempt == attempts or not any(r in err for r in RETRYABLE):
            raise RuntimeError(f"{' '.join(cmd)} failed: {err}")
        print(f"  retry {attempt}/{attempts} in {delay:.0f}s: {err[:120]}",
              file=sys.stderr)
        time.sleep(delay)
        delay = min(delay * 2, 60.0)
    raise AssertionError("unreachable")


def write_json(path, value):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(value, f, indent=4, ensure_ascii=False)
        f.write("\n")


def dump(dump_dir, profile, region):
    os.makedirs(os.path.join(dump_dir, "defaults"), exist_ok=True)
    services = aws(["service-quotas", "list-services"], profile, region)
    write_json(os.path.join(dump_dir, "services.json"), services)
    codes = sorted(s["ServiceCode"] for s in services["Services"])
    print(f"{len(codes)} services")
    for i, code in enumerate(codes, 1):
        out = aws(
            ["service-quotas", "list-aws-default-service-quotas",
             "--service-code", code],
            profile,
            region,
        )
        write_json(os.path.join(dump_dir, "defaults", f"{code}.json"), out)
        print(f"[{i}/{len(codes)}] {code}: {len(out.get('Quotas', []))} quotas")
    with open(os.path.join(dump_dir, "region"), "w") as f:
        f.write(region + "\n")


def compact_quota(q, service_code):
    missing = [f for f in REQUIRED_FIELDS if f not in q]
    if missing:
        raise ValueError(f"{service_code}/{q.get('QuotaCode')}: missing {missing}")
    if q.get("ServiceCode", service_code) != service_code:
        raise ValueError(f"{q['QuotaCode']} listed under {service_code} "
                         f"but belongs to {q['ServiceCode']}")
    out = {}
    for field in QUOTA_FIELDS:
        if field in q:
            out[field] = q[field]
    out.setdefault("Description", "")
    out["Value"] = float(out["Value"])
    return out


def build(dump_dir, out_path, region):
    with open(os.path.join(dump_dir, "services.json"), encoding="utf-8") as f:
        listed = json.load(f)["Services"]
    services = []
    total = 0
    for s in sorted(listed, key=lambda s: s["ServiceCode"]):
        code = s["ServiceCode"]
        path = os.path.join(dump_dir, "defaults", f"{code}.json")
        with open(path, encoding="utf-8") as f:
            quotas = json.load(f).get("Quotas", [])
        compact = [compact_quota(q, code) for q in quotas]
        compact.sort(key=lambda q: q["QuotaCode"])
        seen = set()
        for q in compact:
            if q["QuotaCode"] in seen:
                raise ValueError(f"duplicate quota {code}/{q['QuotaCode']}")
            seen.add(q["QuotaCode"])
        total += len(compact)
        services.append(
            {"ServiceCode": code, "ServiceName": s["ServiceName"],
             "Quotas": compact}
        )
    if len({s["ServiceCode"] for s in services}) != len(services):
        raise ValueError("duplicate service code in services.json")
    payload = (json.dumps({"region": region, "services": services},
                          separators=(",", ":"), ensure_ascii=False,
                          sort_keys=False) + "\n").encode("utf-8")
    buf = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=buf, mtime=0,
                       compresslevel=9) as f:
        f.write(payload)
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    with open(out_path, "wb") as f:
        f.write(buf.getvalue())
    print(f"wrote {len(services)} services, {total} quotas -> "
          f"{os.path.relpath(out_path)} ({len(payload)} bytes raw, "
          f"{len(buf.getvalue())} bytes gzipped)")


def dump_region(dump_dir, fallback):
    try:
        with open(os.path.join(dump_dir, "region")) as f:
            return f.read().strip() or fallback
    except FileNotFoundError:
        return fallback


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("dump", "all"):
        d = sub.add_parser(name)
        d.add_argument("dump_dir")
        d.add_argument("--profile")
        d.add_argument("--region", default="us-east-1")
        if name == "all":
            d.add_argument("--out", default=DEFAULT_OUT)
    b = sub.add_parser("build")
    b.add_argument("dump_dir")
    b.add_argument("--out", default=DEFAULT_OUT)
    b.add_argument("--region", default=None,
                   help="region the dump was taken in (default: the dump's "
                        "`region` file, else us-east-1)")
    args = p.parse_args()
    if args.cmd in ("dump", "all"):
        dump(args.dump_dir, args.profile, args.region)
    if args.cmd in ("build", "all"):
        region = getattr(args, "region", None) or dump_region(args.dump_dir,
                                                              "us-east-1")
        build(args.dump_dir, args.out, region)
    return 0


if __name__ == "__main__":
    sys.exit(main())
