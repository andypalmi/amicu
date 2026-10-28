variable "project" {
  type        = string
  description = "Existing GCP project id (billing already linked)."
}

variable "region" {
  type        = string
  default     = "europe-west4"
  description = "Region for the regional GPU quota preferences."
}

variable "deployer_member" {
  type        = string
  description = "IAM member that runs amicu, for example user:you@example.com."
}

variable "contact_email" {
  type        = string
  description = "Contact email on quota requests."
}

variable "gpu_count" {
  type        = number
  default     = 1
  description = "Quota value requested for each GPU quota."
}

variable "request_quota" {
  type        = bool
  default     = true
  description = "Set to false to skip the quota preferences and the Cloud Quotas API."
}

provider "google" {
  project = var.project
  region  = var.region
}

locals {
  apis = concat(
    ["compute.googleapis.com", "run.googleapis.com", "secretmanager.googleapis.com"],
    var.request_quota ? ["cloudquotas.googleapis.com"] : [],
  )

  service_accounts = toset(["amicu-vm", "amicu-run"])

  quotas = var.request_quota ? {
    "amicu-gpus-all-regions" = {
      service = "compute.googleapis.com"
      quota   = "GPUS-ALL-REGIONS-per-project"
      dims    = {}
    }
    "amicu-t4-${var.region}" = {
      service = "compute.googleapis.com"
      quota   = "NVIDIA-T4-GPUS-per-project-region"
      dims    = { region = var.region }
    }
    "amicu-l4-${var.region}" = {
      service = "compute.googleapis.com"
      quota   = "NVIDIA-L4-GPUS-per-project-region"
      dims    = { region = var.region }
    }
    "amicu-run-l4-${var.region}" = {
      service = "run.googleapis.com"
      quota   = "NvidiaL4GpuAllocNoZonalRedundancyPerProjectRegion"
      dims    = { region = var.region }
    }
  } : {}
}

resource "google_project_service" "apis" {
  for_each           = toset(local.apis)
  project            = var.project
  service            = each.value
  disable_on_destroy = false
}

resource "google_service_account" "amicu" {
  for_each     = local.service_accounts
  project      = var.project
  account_id   = each.value
  display_name = "amicu ${trimprefix(each.value, "amicu-")}"

  depends_on = [google_project_service.apis]
}

resource "google_service_account_iam_member" "deployer_user" {
  for_each           = local.service_accounts
  service_account_id = google_service_account.amicu[each.value].name
  role               = "roles/iam.serviceAccountUser"
  member             = var.deployer_member
}

resource "google_cloud_quotas_quota_preference" "gpu" {
  for_each      = local.quotas
  parent        = "projects/${var.project}"
  name          = each.key
  service       = each.value.service
  quota_id      = each.value.quota
  contact_email = var.contact_email
  justification = "amicu GPU workloads"
  dimensions    = each.value.dims

  quota_config {
    preferred_value = var.gpu_count
  }

  depends_on = [google_project_service.apis]
}

output "gcp_block" {
  description = "Values for the gcp {} block in amicu.hcl."
  value = {
    project                   = var.project
    service_account           = google_service_account.amicu["amicu-vm"].email
    cloud_run_service_account = google_service_account.amicu["amicu-run"].email
  }
}
