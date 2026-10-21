#!/usr/bin/env bash
# AWS Marketplace rejects non-S3 logo URLs (INVALID_MEDIA). This copies the
# GitHub logo into a public S3 bucket and records the resulting URL.
#
# Requirements AWS enforces on the logo: .png/.jpg/.gif, transparent or white
# background, under 5MB, between 110 and 10000 pixels on each side.
set -euo pipefail

export AWS_CA_BUNDLE=${AWS_CA_BUNDLE:-/etc/ssl/cert.pem}
export AWS_PAGER=''
PROFILE=marketplace
ACCOUNT=187692046617
BUCKET="pine-marketplace-assets-${ACCOUNT}"
KEY=logo.png
SRC=https://raw.githubusercontent.com/Leanmcp/.github/refs/heads/main/LOGO.png
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "== 0. confirm account =="
aws sts get-caller-identity --profile "$PROFILE" --query Account --output text | grep -qx "$ACCOUNT"

echo "== 1. download the logo =="
curl -fsSL "$SRC" -o "$TMP/$KEY"
file "$TMP/$KEY"
BYTES=$(wc -c < "$TMP/$KEY")
echo "  size: $BYTES bytes"
[ "$BYTES" -lt 5242880 ] || { echo "Logo exceeds the 5MB limit"; exit 1; }
echo "  Check the dimensions printed above are between 110 and 10000 px per side."

echo "== 2. create the asset bucket (public read, logo only) =="
aws s3api create-bucket --profile "$PROFILE" --region us-east-1 --bucket "$BUCKET" \
  || echo "  (bucket already exists, continuing)"

echo "== 3. allow public reads on this bucket =="
aws s3api put-public-access-block --profile "$PROFILE" --bucket "$BUCKET" \
  --public-access-block-configuration \
  "BlockPublicAcls=false,IgnorePublicAcls=false,BlockPublicPolicy=false,RestrictPublicBuckets=false"

cat > "$TMP/policy.json" <<JSON
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "PublicReadMarketplaceAssets",
      "Effect": "Allow",
      "Principal": "*",
      "Action": "s3:GetObject",
      "Resource": "arn:aws:s3:::${BUCKET}/*"
    }
  ]
}
JSON
aws s3api put-bucket-policy --profile "$PROFILE" --bucket "$BUCKET" \
  --policy "file://$TMP/policy.json"

echo "== 4. upload =="
aws s3api put-object --profile "$PROFILE" --bucket "$BUCKET" --key "$KEY" \
  --body "$TMP/$KEY" --content-type image/png >/dev/null

URL="https://${BUCKET}.s3.amazonaws.com/${KEY}"

echo "== 5. verify it is publicly readable =="
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$URL")
[ "$CODE" = 200 ] || { echo "Logo not public (HTTP $CODE)"; exit 1; }
echo "  HTTP 200"

echo "== 6. record the URL in config.json =="
python3 "$(cd "$(dirname "$0")" && pwd)/record_logo.py" "$URL"

echo
echo "DONE. Logo: $URL"
echo "Next: python3 workspace/marketplace_publish/build_changeset.py"
