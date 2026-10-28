# GCP project setup for amicu

Two equivalent ways to prepare an existing GCP project (billing already linked) for amicu's `gcp` provider. See the GCP provider docs (`web/apps/web/content/docs/providers/gcp.mdx`) for what each piece is for.

## Script

```sh
./setup.sh my-project-id [region] [--skip-quota] [--keep-default-firewall] [--gpu-count N]
```

It enables the Compute Engine, Cloud Run, Secret Manager and Cloud Quotas APIs, creates the `amicu-vm` and `amicu-run` service accounts, grants the active gcloud user `roles/iam.serviceAccountUser` on both, requests GPU quota through the Cloud Quotas REST API, deletes the `default-allow-ssh` and `default-allow-rdp` firewall rules (open to 0.0.0.0/0), and prints the `gcp {}` values to paste. It does not create the project or link billing; it prints the commands when either is missing. It is safe to run again. Afterwards run `gcloud auth application-default login`.

## Terraform

```sh
terraform init
terraform apply -var project=my-project-id -var deployer_member=user:you@example.com -var contact_email=you@example.com
```

It manages the API enablement (`disable_on_destroy = false`), the two service accounts, additive `google_service_account_iam_member` grants and the quota preferences. It does not touch firewall rules; delete the default ones separately if you want to. The `gcp_block` output holds the values for `amicu.hcl`.

## Notes

- amicu owns the resources it creates: its labelled VMs, firewall rules, Cloud Run services and secrets. Terraform must not manage them, or the two will fight over them.
- Use only additive IAM resources (`_member`), never `_binding` or `_policy`, which would remove grants made by amicu or others.
- `google_cloud_quotas_quota_preference` fails if a preference for the same quota and dimensions already exists (created in the console, by hand or by `setup.sh`). Import it first, for example `terraform import 'google_cloud_quotas_quota_preference.gpu["amicu-gpus-all-regions"]' projects/my-project-id/locations/global/quotaPreferences/<existing-preference-id>`, or set `-var request_quota=false`. `setup.sh` skips quotas that already have a preference and raises ones whose requested value is lower.
- Quota requests can need manual approval and take from minutes to days. New projects start with `GPUS-ALL-REGIONS-per-project` at 0.
- Keep GPU workloads out of projects that have a billing kill switch (a budget that disables billing): the kill switch would take the project down mid-run and block amicu from cleaning up.
