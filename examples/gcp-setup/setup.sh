#!/usr/bin/env bash
# Prepares an existing GCP project for amicu. Idempotent: safe to run again.
# Does not create the project or link billing, and never creates VMs or Cloud Run services.
set -euo pipefail

usage() {
  cat <<USAGE
Usage: $0 <project-id> [region] [--skip-quota] [--keep-default-firewall] [--gpu-count N]

  project-id               existing GCP project with billing linked (required)
  region                   default: europe-west4
  --skip-quota             do not request GPU quota
  --keep-default-firewall  keep default-allow-ssh and default-allow-rdp (open to 0.0.0.0/0)
  --gpu-count N            quota value to request per GPU quota (default: 1)
USAGE
}

PROJECT=""
REGION=""
SKIP_QUOTA=0
KEEP_FW=0
GPU_COUNT=1

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --skip-quota) SKIP_QUOTA=1 ;;
    --keep-default-firewall) KEEP_FW=1 ;;
    --gpu-count) shift; GPU_COUNT="${1:?--gpu-count needs a value}" ;;
    -*) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [ -z "$PROJECT" ]; then PROJECT="$1"
      elif [ -z "$REGION" ]; then REGION="$1"
      else echo "unexpected argument: $1" >&2; usage >&2; exit 2; fi ;;
  esac
  shift
done

[ -n "$PROJECT" ] || { usage >&2; exit 2; }
REGION="${REGION:-europe-west4}"
case "$GPU_COUNT" in ''|*[!0-9]*) echo "--gpu-count must be a number" >&2; exit 2 ;; esac

command -v gcloud >/dev/null || { echo "gcloud is not installed" >&2; exit 1; }

log() { printf '==> %s\n' "$*"; }

ACCOUNT="$(gcloud config get-value account 2>/dev/null || true)"
[ -n "$ACCOUNT" ] || { echo "No active gcloud account. Run: gcloud auth login" >&2; exit 1; }

if ! gcloud projects describe "$PROJECT" >/dev/null 2>&1; then
  cat >&2 <<MSG
Project $PROJECT does not exist or $ACCOUNT cannot see it. Create it and link billing first:

  gcloud projects create $PROJECT
  gcloud billing accounts list
  gcloud billing projects link $PROJECT --billing-account <BILLING_ACCOUNT_ID>
MSG
  exit 1
fi

if [ "$(gcloud billing projects describe "$PROJECT" --format='value(billingEnabled)' 2>/dev/null || true)" != "True" ]; then
  cat >&2 <<MSG
Billing is not enabled on $PROJECT. Link it first:

  gcloud billing accounts list
  gcloud billing projects link $PROJECT --billing-account <BILLING_ACCOUNT_ID>
MSG
  exit 1
fi

APIS="compute.googleapis.com run.googleapis.com secretmanager.googleapis.com"
[ "$SKIP_QUOTA" -eq 1 ] || APIS="$APIS cloudquotas.googleapis.com"
log "Enabling APIs: $APIS"
# shellcheck disable=SC2086
gcloud services enable $APIS --project "$PROJECT"

for sa in amicu-vm amicu-run; do
  email="$sa@$PROJECT.iam.gserviceaccount.com"
  if gcloud iam service-accounts describe "$email" --project "$PROJECT" >/dev/null 2>&1; then
    log "Service account $email exists"
  else
    log "Creating service account $email"
    gcloud iam service-accounts create "$sa" --project "$PROJECT" \
      --display-name "amicu ${sa#amicu-}"
  fi
done

# Owners already have serviceAccountUser; the additive per-account binding is harmless for them.
for sa in amicu-vm amicu-run; do
  email="$sa@$PROJECT.iam.gserviceaccount.com"
  log "Granting roles/iam.serviceAccountUser on $email to user:$ACCOUNT"
  gcloud iam service-accounts add-iam-policy-binding "$email" --project "$PROJECT" \
    --member "user:$ACCOUNT" --role roles/iam.serviceAccountUser --condition=None >/dev/null
done

request_quota() {
  # args: preference-id service quotaId dimensions-json
  local id="$1" service="$2" quota_id="$3" dims="$4" token body code out
  token="$(gcloud auth print-access-token)"
  body="$(printf '{"service":"%s","quotaId":"%s","quotaConfig":{"preferredValue":"%s"},"dimensions":%s,"justification":"amicu GPU workloads","contactEmail":"%s"}' \
    "$service" "$quota_id" "$GPU_COUNT" "$dims" "$ACCOUNT")"
  out="$(mktemp)"
  code="$(curl -sS -o "$out" -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $token" \
    -H "x-goog-user-project: $PROJECT" \
    -H "Content-Type: application/json" \
    -d "$body" \
    "https://cloudquotas.googleapis.com/v1/projects/$PROJECT/locations/global/quotaPreferences?quotaPreferenceId=$id")" || code=000
  case "$code" in
    200) log "Quota requested: $quota_id ($GPU_COUNT)" ;;
    409) log "Quota preference $id already exists (change the value in the console)" ;;
    *) echo "warning: quota request $quota_id failed (HTTP $code); request it in the console instead" >&2
       head -c 400 "$out" >&2; echo >&2 ;;
  esac
  rm -f "$out"
}

if [ "$SKIP_QUOTA" -eq 0 ]; then
  log "Requesting GPU quota in $REGION (value $GPU_COUNT)"
  rdims="{\"region\":\"$REGION\"}"
  request_quota amicu-gpus-all-regions compute.googleapis.com GPUS-ALL-REGIONS-per-project '{}'
  request_quota "amicu-t4-$REGION" compute.googleapis.com NVIDIA-T4-GPUS-per-project-region "$rdims"
  request_quota "amicu-l4-$REGION" compute.googleapis.com NVIDIA-L4-GPUS-per-project-region "$rdims"
  request_quota "amicu-run-l4-$REGION" run.googleapis.com NvidiaL4GpuAllocNoZonalRedundancyPerProjectRegion "$rdims"
  echo "Requests may need manual approval and can take from minutes to days."
fi

if [ "$KEEP_FW" -eq 0 ]; then
  for rule in default-allow-ssh default-allow-rdp; do
    if gcloud compute firewall-rules describe "$rule" --project "$PROJECT" >/dev/null 2>&1; then
      log "Deleting firewall rule $rule"
      gcloud compute firewall-rules delete "$rule" --project "$PROJECT" --quiet
    fi
  done
fi

cat <<OUT

Done. Paste into your amicu config:

  gcp {
    project         = "$PROJECT"
    service_account = "amicu-vm@$PROJECT.iam.gserviceaccount.com"
  }

Cloud Run deployments use amicu-run@$PROJECT.iam.gserviceaccount.com.

Then log in with Application Default Credentials:

  gcloud auth application-default login
OUT
