#!/usr/bin/env python3
"""Render the AWS Marketplace change set for the Pine Server listing.

Reads config.json (see config.example.json), writes changeset.json next to it.
Does not call AWS. Submit with 01_submit.sh after reviewing the output.
"""
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
cfg = json.loads((HERE / "config.json").read_text())

# Refuse to build a listing that still contains placeholders.
blanks = []
def scan(node, path="config"):
    if isinstance(node, dict):
        for k, v in node.items():
            if not k.startswith("_"):
                scan(v, f"{path}.{k}")
    elif isinstance(node, list):
        for i, v in enumerate(node):
            scan(v, f"{path}[{i}]")
    elif isinstance(node, str) and "FILL_ME" in node:
        blanks.append(path)
scan(cfg)
if blanks:
    sys.exit("Unfilled config values:\n  " + "\n  ".join(blanks))

product = {"Type": "AmiProduct@1.0", "Identifier": cfg["product_id"]}
offer = {"Type": "Offer@1.0", "Identifier": "$CreateOfferChange.Entity.Identifier"}

long_description = (
    "Pine Server is a single-node capture server for Claude Code and other "
    "coding-agent CLI traffic. Clients running the Pine proxy upload captured "
    "sessions over HTTPS with an organization upload key; the server stores them in "
    "a local SQLite database and serves a browser UI for reading them back.\n\n"
    "The image runs the server as an unprivileged systemd service bound to localhost, "
    "with Caddy available for optional public HTTPS. Each instance generates its own "
    "upload key on first boot, so no credential is shared between buyers and the "
    "image ships with an empty database.\n\n"
    "Administration is documented on the instance: upload policy in "
    "/etc/pine/config.yaml, captured data in /var/lib/pine, and the "
    "generated key readable by root at /root/pine-upload-key.\n\n"
    "Upload keys currently grant organization-wide read and write access; browser SSO "
    "and per-user permissions are not included. Captured sessions may contain "
    "confidential information, so restrict network access and back up the data volume."
)

changeset = [
    {
        "ChangeType": "UpdateInformation",
        "Entity": product,
        "DetailsDocument": {
            "ProductTitle": "Pine Server",
            "ShortDescription": (
                "Self-hosted capture server for Claude Code and coding-agent CLI "
                "sessions, with authenticated uploads and a browser UI."
            ),
            "LongDescription": long_description,
            "Highlights": [
                "Single-node server: systemd service, SQLite storage, no external dependencies.",
                "Unique upload key generated on first boot; the image ships with no preset credential and no data.",
                "Optional public HTTPS via a one-command Caddy setup, or private access over an SSH tunnel.",
            ],
            "SearchKeywords": [
                "claude code", "llm observability", "agent logging",
                "session capture", "developer tools",
            ],
            # Valid names come from the AWS Marketplace category list; the
            # top-level bucket is "DevOps" and these are its subcategories.
            # "Developer Tools" is not a real category and fails with
            # INVALID_CATEGORY_NAMES. Max three.
            "Categories": ["Log Analysis", "Monitoring"],
            # Must be a direct, publicly readable S3 URL; anything else fails
            # with INVALID_MEDIA. Populated by 03_upload_logo.sh.
            "LogoUrl": cfg["logo_url"],
            # AWS wants a URL here, not an address; omitted when we only have an
            # email, which already appears in SupportDescription.
            "AdditionalResources": (
                [{"Type": "Support", "Url": cfg["support_url"]}]
                if cfg.get("support_url") else []
            ),
            "SupportDescription": cfg["support_description"],
            # No seller-name field here: the display name comes from the seller
            # profile in the Marketplace Management Portal, not the change set.
        },
    },
    {
        "ChangeType": "AddRegions",
        "Entity": product,
        "DetailsDocument": {"Regions": cfg["regions"]},
    },
    {
        "ChangeType": "AddInstanceTypes",
        "Entity": product,
        "DetailsDocument": {"InstanceTypes": cfg["instance_types"]},
    },
    {
        "ChangeType": "AddDeliveryOptions",
        "Entity": product,
        "DetailsDocument": {
            "Version": {
                "VersionTitle": cfg["version_title"],
                "ReleaseNotes": cfg["release_notes"],
            },
            "DeliveryOptions": [
                {
                    "Details": {
                        "AmiDeliveryOptionDetails": {
                            "AmiSource": {
                                "AmiId": cfg["ami_id"],
                                "AccessRoleArn": cfg["access_role_arn"],
                                "UserName": "ubuntu",
                                "OperatingSystemName": "UBUNTU",
                                "OperatingSystemVersion": "Ubuntu 24.04 LTS (Noble Numbat) x86_64",
                                "ScanningPort": 22,
                            },
                            "UsageInstructions": (
                                "Launch with your own EC2 key pair and allow SSH only from your "
                                "admin address. SSH in as ubuntu and read the generated upload key "
                                "with: sudo cat /root/pine-upload-key\n\n"
                                "For private access, forward the UI to your workstation:\n"
                                "  ssh -i your-key.pem -L 8080:127.0.0.1:8080 ubuntu@INSTANCE_IP\n"
                                "then open http://127.0.0.1:8080 and enter the key.\n\n"
                                "For a shared server, point DNS at the instance, allow TCP 80 and 443, "
                                "and run: sudo pine-https capture.example.com\n"
                                "Do not expose port 8080 directly.\n\n"
                                "Service: sudo systemctl status pine. Logs: sudo journalctl -u pine. "
                                "Data: /var/lib/pine. Policy: /etc/pine/config.yaml."
                            ),
                            "RecommendedInstanceType": cfg["recommended_instance_type"],
                            "SecurityGroups": [
                                {"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22,
                                 "IpRanges": ["0.0.0.0/0"]},
                                {"IpProtocol": "tcp", "FromPort": 80, "ToPort": 80,
                                 "IpRanges": ["0.0.0.0/0"]},
                                {"IpProtocol": "tcp", "FromPort": 443, "ToPort": 443,
                                 "IpRanges": ["0.0.0.0/0"]},
                            ],
                        }
                    }
                }
            ],
        },
    },
    {
        "ChangeType": "AddDimensions",
        "Entity": product,
        "DetailsDocument": [
            {"Key": it, "Name": it, "Description": f"Pine Server on {it}",
             "Types": ["Metered"], "Unit": "Hrs"}
            for it in cfg["instance_types"]
        ],
    },
    {"ChangeType": "ReleaseProduct", "Entity": product, "DetailsDocument": {}},
    {
        "ChangeType": "CreateOffer",
        "ChangeName": "CreateOfferChange",
        "Entity": {"Type": "Offer@1.0"},
        "DetailsDocument": {"ProductId": cfg["product_id"]},
    },
    {
        "ChangeType": "UpdateInformation",
        "Entity": offer,
        "DetailsDocument": {
            "Name": "Pine Server public offer",
            "Description": "Hourly software pricing per instance, billed in addition to AWS infrastructure costs.",
        },
    },
    {
        "ChangeType": "UpdatePricingTerms",
        "Entity": offer,
        "DetailsDocument": {
            "PricingModel": "Usage",
            "Terms": [
                {
                    "Type": "UsageBasedPricingTerm",
                    "CurrencyCode": "USD",
                    "RateCards": [
                        {
                            "RateCard": [
                                {"DimensionKey": it, "Price": cfg["hourly_prices_usd"][it]}
                                for it in cfg["instance_types"]
                            ]
                        }
                    ],
                }
            ],
        },
    },
    {
        "ChangeType": "UpdateLegalTerms",
        "Entity": offer,
        "DetailsDocument": {
            "Terms": [
                {
                    "Type": "LegalTerm",
                    "Documents": (
                        [{"Type": "StandardEula", "Version": "2022-07-14"}]
                        if cfg["eula"]["type"] == "standard"
                        else [{"Type": "CustomEula", "Url": cfg["eula"]["url"]}]
                    ),
                }
            ]
        },
    },
    {
        "ChangeType": "UpdateSupportTerms",
        "Entity": offer,
        "DetailsDocument": {
            "Terms": [{"Type": "SupportTerm", "RefundPolicy": cfg["refund_policy"]}]
        },
    },
    {"ChangeType": "ReleaseOffer", "Entity": offer, "DetailsDocument": {}},
]

missing = [it for it in cfg["instance_types"] if it not in cfg["hourly_prices_usd"]]
if missing:
    sys.exit(f"No hourly price for: {', '.join(missing)}")

out = HERE / "changeset.json"
out.write_text(json.dumps(changeset, indent=2) + "\n")
print(f"Wrote {out}")
print("Review it, then run 01_submit.sh")
