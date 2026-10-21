# Publishing Pine Server on AWS Marketplace

Account `187692046617`, CLI profile `marketplace`. Catalog API calls go to
`us-east-1` regardless of where the AMI was built.

## Where things stand

| Item | State |
| --- | --- |
| Private AMI (us-west-2) | `ami-00a251a4a1b5414ed`, available, smoke-tested |
| Catalog product | `prod-lfxlsiyd4wqnq`, **Draft**, title only |
| Product code | `4mqchorng8cmd0s750luepdu0` |
| Listing information | not set |
| Delivery options | not set |
| Offer and pricing | not created |
| Visibility | Draft. Not Limited, not Public. |

## What still blocks a public listing

1. **Seller registration.** A paid listing cannot be released until the tax
   interview and bank account are complete in the AWS Marketplace Management
   Portal. That is a web form; there is no API for it.
2. ~~**Business inputs.**~~ Supplied and recorded in
   `workspace/marketplace_publish/config.json`. See *Listing decisions* below.
3. **AMI region.** The Catalog API only accepts source AMIs in `us-east-1`. The
   current image is in `us-west-2` and must be copied.
4. **Ingestion role.** An IAM role trusted by `assets.marketplace.amazonaws.com`
   with the `AWSMarketplaceAmiIngestion` managed policy does not exist yet.
5. **AWS review.** `ReleaseProduct` moves the product to **Limited**, not
   Public. Public visibility follows the AWS Marketplace security scan and
   listing review, requested through the Management Portal. No API call skips it.

## Runbook

Everything below lives in `workspace/marketplace_publish/`.

```bash
# 1. IAM ingestion role + copy the AMI into us-east-1. Prints the new AMI id.
time ./workspace/marketplace_publish/00_prereqs.sh

# 2. Fill in the business inputs.
cp workspace/marketplace_publish/config.example.json workspace/marketplace_publish/config.json
$EDITOR workspace/marketplace_publish/config.json     # replace every FILL_ME

# 3. Render the change set. Refuses to run while any FILL_ME remains.
python3 workspace/marketplace_publish/build_changeset.py

# 4. Read workspace/marketplace_publish/changeset.json, then submit.
./workspace/marketplace_publish/01_submit.sh          # prompts for the word PUBLISH

# 5. Poll validation and the AMI scan.
./workspace/marketplace_publish/02_status.sh
```

Step 4 is the irreversible one: it releases the product to Limited and creates a
live public offer. Read the rendered change set before running it.

## After a successful change set

The product sits at Limited visibility with a public offer attached. Request
public visibility in the AWS Marketplace Management Portal. AWS reviews the
listing copy and the AMI scan results and can send it back with required fixes.

## Listing copy

Descriptions, highlights, categories, security-group rules and buyer usage
instructions are generated in `build_changeset.py` from the tested behaviour of
the AMI. Edit them there, not in the console, so the file stays the source of
truth.

## Listing decisions

Recorded in `workspace/marketplace_publish/config.json`.

| Field | Value |
| --- | --- |
| Seller display name | Leanmcp |
| Support | contact@leanmcp.com (in `SupportDescription`) |
| Logo | `https://raw.githubusercontent.com/Leanmcp/.github/refs/heads/main/LOGO.png` |
| EULA | AWS Standard Contract for AWS Marketplace, version 2022-07-14 |
| Refund policy | No refund by default; discretionary for defects; none after 7 days |
| Regions | us-east-1, us-west-2, eu-west-1 |
| Recommended instance | t3.small |

### Pricing

Software price per instance-hour, billed on top of the buyer's EC2 and EBS costs.

Rule: **$1.40/hour below 4 vCPU; at 4 vCPU and above, `1.40 * (vCPU / 2)`.**

| Instance type | vCPU | Software $/hr |
| --- | --- | --- |
| t3.small | 2 | 1.40 |
| t3.medium | 2 | 1.40 |
| t3.large | 2 | 1.40 |
| m5.large | 2 | 1.40 |
| m5.xlarge | 4 | 2.80 |

Only `m5.xlarge` sits above the floor, so the proportional half of the rule is
barely visible on the current list. Adding larger types (m5.2xlarge at 8 vCPU
would be $5.60) makes the ladder meaningful; add them to both `instance_types`
and `hourly_prices_usd` using the same formula.

Note for the record: at $1.40/hour a t3.small costs roughly $1,020/month in
software on top of about $15/month of EC2. That is deliberate per the pricing
instruction, but AWS listing reviewers compare price against the buyer-visible
value proposition, and buyers see both numbers side by side on the listing page.

### AWS Marketplace listing notes

`AdditionalResources` is omitted because AWS expects a URL there and only an
email was supplied; the address appears in `SupportDescription` instead. Set
`support_url` in the config to a docs or support page to add the resource link.

`UpdateInformation` on an `AmiProduct` accepts only `ProductTitle`,
`ShortDescription`, `LongDescription`, `Highlights`, `SearchKeywords`,
`Categories`, `LogoUrl`, `VideoUrls`, `AdditionalResources` and
`SupportDescription`. `Manufacturer` appears when you read the entity back but is
rejected on write with `Object has properties which are not allowed`. The public
seller name ("Leanmcp") is a seller-profile setting in the Marketplace Management
Portal, not a change-set field.
