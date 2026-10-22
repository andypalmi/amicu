# Google Cloud (T4 Spot VM)

vLLM serving Qwen3-0.6B (fp16) on one T4 Spot VM in `europe-west4`, deleted by Compute Engine after 2 hours. Replace `your-gcp-project-id`, run `gcloud auth application-default login`, export `CHAT_API_KEY` (a random key, for example `openssl rand -hex 32`) and run `amicu apply`. The endpoint is plain HTTP until TLS is supported, and the firewall rule allows only your public IP unless you set `allowed_source_cidrs`. See https://amicu.dev/docs/providers/gcp.
