#!/usr/bin/env bash
# Prerequisites for publishing Pine Server (prod-lfxlsiyd4wqnq) on AWS Marketplace.
# Run this yourself. It is idempotent-ish: re-running create-role will fail harmlessly.
set -euo pipefail

export AWS_CA_BUNDLE=${AWS_CA_BUNDLE:-/etc/ssl/cert.pem}
export AWS_PAGER=''
PROFILE=marketplace
ACCOUNT=187692046617
SRC_REGION=us-west-2
SRC_AMI=ami-00a251a4a1b5414ed

echo "== 0. confirm account =="
aws sts get-caller-identity --profile "$PROFILE" --query Account --output text | grep -qx "$ACCOUNT"

echo "== 1. IAM role for AMI ingestion =="
cat > /tmp/awsmp-trust.json <<'JSON'
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "Service": "assets.marketplace.amazonaws.com" },
      "Action": "sts:AssumeRole"
    }
  ]
}
JSON
aws iam create-role --profile "$PROFILE" \
  --role-name AWSMarketplaceAmiIngestion \
  --description "Lets AWS Marketplace copy and scan Pine AMIs for listing" \
  --assume-role-policy-document file:///tmp/awsmp-trust.json || \
  echo "  (role already exists, continuing)"
aws iam attach-role-policy --profile "$PROFILE" \
  --role-name AWSMarketplaceAmiIngestion \
  --policy-arn arn:aws:iam::aws:policy/AWSMarketplaceAmiIngestion

echo "== 2. copy the AMI into us-east-1 (Catalog API only accepts source AMIs there) =="
DEST_AMI=$(aws ec2 copy-image --profile "$PROFILE" --region us-east-1 \
  --source-region "$SRC_REGION" --source-image-id "$SRC_AMI" \
  --name "pine-server-$(date +%s)" \
  --description "Pine single-node capture server; unique keys generated at first boot; Ubuntu 24.04 x86_64" \
  --query ImageId --output text)
echo "  us-east-1 AMI: $DEST_AMI"

echo "== 3. wait for it to become available =="
aws ec2 wait image-available --profile "$PROFILE" --region us-east-1 --image-ids "$DEST_AMI"

echo "== 4. require IMDSv2 on the copy =="
aws ec2 modify-image-attribute --profile "$PROFILE" --region us-east-1 \
  --image-id "$DEST_AMI" --imds-support Value=v2.0

echo "== 5. record the AMI id in config.json =="
python3 "$(cd "$(dirname "$0")" && pwd)/record_ami.py" "$DEST_AMI"

echo
echo "DONE. us-east-1 AMI: $DEST_AMI"
echo "Next: python3 workspace/marketplace_publish/build_changeset.py"
