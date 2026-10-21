# Pine Server — AWS Marketplace publication TODO

Account `187692046617` · profile `marketplace` · product `prod-lfxlsiyd4wqnq`
Change set `9p8ywhtd65vfus0mp666ztmzp` FAILED 2026-09-08 13:53:25Z; see step 1.

Step 1 is a scripted fix-and-resubmit. Steps 2 and 3 are manual and gated on a
human at AWS. Full background: `marketplace/aws/PUBLISH.md`.

---

## 1. Fix the two listing errors and resubmit

Change set `9p8ywhtd65vfus0mp666ztmzp` **FAILED** at 2026-09-08 13:53:25Z and
rolled back. The product is back at **Draft**. Nothing was created.

**The AMI security scan passed** — `AddDeliveryOptions` returned zero errors, so
`ami-01b1063fb6b0be435` is accepted as-is. Both failures were listing metadata:

| Error | Cause | Fix |
| --- | --- | --- |
| `INVALID_MEDIA` | Logo URL was GitHub raw. AWS requires a direct, publicly readable S3 URL. | `03_upload_logo.sh` |
| `INVALID_CATEGORY_NAMES` | `"Developer Tools"` is not an AWS Marketplace category. | Already fixed in `build_changeset.py` → `["Log Analysis", "Monitoring"]` |

The category list is fixed in code. Only the logo needs a command:

```bash
# Copies the logo into a public S3 bucket and writes the URL into config.json.
time ./workspace/marketplace_publish/03_upload_logo.sh

# Re-render and resubmit.
python3 workspace/marketplace_publish/build_changeset.py
time ./workspace/marketplace_publish/01_submit.sh

# Poll. Note: 02_status.sh reads submit-result.json, so it follows the new set.
./workspace/marketplace_publish/02_status.sh
```

`03_upload_logo.sh` creates bucket `pine-marketplace-assets-187692046617`
with a public-read policy scoped to that bucket, uploads `logo.png`, verifies it
returns HTTP 200, and records the URL. It prints the image dimensions — confirm
they are between 110 and 10000 px per side, since AWS enforces that and the
script cannot check it portably.

- [ ] Logo uploaded and `config.json` `logo_url` points at S3
- [ ] Change set status reaches `SUCCEEDED`
- [ ] `list-entities` shows `Visibility: Limited`

**Do not use the console "Create product" wizard.** It appears because the
product is still Draft. Filling it in would diverge from `build_changeset.py`,
which is the source of truth for this listing.

Expect the resubmit to take another 30-60 minutes; the AMI is rescanned each
time. If it fails again, read the per-change `ErrorDetailList` — the errors are
specific and name the offending field.

---

## 2. Set the public seller display name to "Leanmcp"

This is a seller-profile setting. It is **not** in the change set, nothing in
this repo can set or read it, and it is the name buyers see on the listing.

1. Sign in to [AWS Partner Central](https://us-east-1.console.aws.amazon.com/partnercentral/home?region=us-east-1)
   with the seller account (`187692046617`).
2. In the left navigation pane, scroll to **AWS Marketplace settings**.
3. If no public profile exists, choose **Add public profile**. If one exists,
   open it to edit.
4. Fill in:
   - **Display name** — `Leanmcp` (max 40 characters; may differ from the legal
     entity name)
   - **Company logo** — upload; reuse
     `https://raw.githubusercontent.com/Leanmcp/.github/refs/heads/main/LOGO.png`
   - **URL to website** — your company site
   - **Company description** — max 600 characters
   - **Contact information** — use `contact@leanmcp.com`, matching the
     `SupportDescription` already in the listing
5. Choose **Submit**.

- [ ] Display name reads `Leanmcp`
- [ ] Support contact matches `contact@leanmcp.com`

You can update this at any time after registration.

---

## 3. Request public visibility

A successful change set leaves the product at **Limited**, visible only to
allowlisted AWS account IDs. Public visibility is a separate request that a
human at AWS reviews. There is no API for it.

**Prerequisite:** step 1 must show `SUCCEEDED` and the product must be `Limited`.
Also confirm the tax interview and bank account are complete — a paid offer
cannot go public without them.

1. Open the [AWS Marketplace Management Portal](https://us-east-1.console.aws.amazon.com/partnercentral/home)
   and sign in to the seller account.
2. Go to the [**Server products**](https://aws.amazon.com/marketplace/management/products/server)
   page and, on the **Current server product** tab, select **Pine Server**.
3. From the **Request changes** dropdown, choose **Update visibility**.
4. Choose **Submit change request**.
5. On the **Requests** tab, confirm **Request status** is **Under review**.

- [ ] Request submitted
- [ ] Request status is **Under review**
- [ ] Request status reaches **Succeeded** and visibility is **Public**

The AWS Marketplace Seller Operations team audits the product against their
policies before approving. Expect this to take days, not minutes, and expect
them to come back with required changes.

---

## 4. Prepare the pricing justification before step 3

Seller Operations reviews the listing against the buyer-visible value
proposition, and your pricing is the most likely thing they question.

The listing shows both numbers side by side:

| | t3.small |
| --- | --- |
| Software (yours) | $1.40/hr ≈ **$1,020/month** |
| EC2 infrastructure | ≈ **$15/month** |

That is roughly a 68× multiple on infrastructure, on an image whose tested
workload is a single-node SQLite capture server. Decide before you submit
whether you want to defend that number or revise it.

To revise: edit `hourly_prices_usd` in
`workspace/marketplace_publish/config.json`, re-render, and submit an
`UpdatePricingTerms` change set against the existing offer. Changing price on a
live public offer has buyer-notification rules attached, so it is much cheaper
to settle this **now**, while the product is still Limited.

- [ ] Price confirmed or revised
- [ ] Written justification ready for Seller Operations

---

## Not done, deliberately

- **`git push`.** `marketplace/`, `workspace/marketplace_publish/` and this file
  are untracked. Pushing puts listing copy and pricing on a remote. Say so
  explicitly if you want it committed and pushed.
- **Larger instance types.** Only `m5.xlarge` clears the $1.40 floor, so the
  proportional half of the pricing rule is barely visible. Adding `m5.2xlarge`
  (8 vCPU → $5.60) would make the ladder real. Add to both `instance_types` and
  `hourly_prices_usd`.

## Sources

- [Updating AMI-based product visibility](https://docs.aws.amazon.com/marketplace/latest/userguide/ami-update-visibility.html)
- [Register and create your seller profile](https://docs.aws.amazon.com/marketplace/latest/userguide/create-public-profile.html)
- [AMI-based product requirements](https://docs.aws.amazon.com/marketplace/latest/userguide/product-and-ami-policies.html)
