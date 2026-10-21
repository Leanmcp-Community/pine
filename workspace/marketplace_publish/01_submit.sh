#!/usr/bin/env bash
# Submits the rendered change set. This is the point of no return for the
# Limited release: the offer becomes real and AWS begins its review.
set -euo pipefail
export AWS_CA_BUNDLE=${AWS_CA_BUNDLE:-/etc/ssl/cert.pem}
export AWS_PAGER=''
cd "$(dirname "$0")"

test -f changeset.json || { echo "Run build_changeset.py first"; exit 1; }
echo "About to submit $(python3 -c 'import json;print(len(json.load(open("changeset.json"))))') changes. Ctrl-C to abort."
read -r -p "Type PUBLISH to continue: " confirm
[ "$confirm" = "PUBLISH" ] || exit 1

aws marketplace-catalog start-change-set \
  --profile marketplace --region us-east-1 \
  --catalog AWSMarketplace \
  --change-set-name "Pine Server public release" \
  --change-set file://changeset.json \
  --output json | tee submit-result.json
