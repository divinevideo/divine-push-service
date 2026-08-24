#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <event-name> <ref> <include-production> <sha> <repository> <ref-name>" >&2
  exit 2
fi

event_name="$1"
ref="$2"
include_production_input="$3"
sha="$4"
repository="$5"
ref_name="$6"

include_production=false
if [[ "$ref" == refs/tags/v* ]] ||
  [[ "$event_name" == "workflow_dispatch" && "$include_production_input" == "true" ]]; then
  include_production=true
fi

if [[ "$include_production" == "true" ]]; then
  environments='["poc","staging","production"]'
else
  environments='["poc","staging"]'
fi

payload="$(
  jq -cn \
    --argjson environments "$environments" \
    --arg image_tag "${sha:0:7}" \
    --arg repository "$repository" \
    --arg sha "$sha" \
    --arg ref_name "$ref_name" \
    --argjson include_production "$include_production" \
    '{
      event_type: "image-deploy",
      client_payload: (
        {
          application: "divine-push-service",
          environments: $environments,
          images: [{name: "divine-push-service", tag: $image_tag}],
          source_repo: $repository,
          source_sha: $sha,
          source_ref: $ref_name
        }
        + if $include_production then {auto_promote: true} else {} end
      )
    }'
)"

if ! jq -e '
  if (.client_payload.environments | index("production")) != null then
    .client_payload.auto_promote == true
  else
    true
  end
' >/dev/null <<<"$payload"; then
  echo "production deployment payload requires auto_promote: true" >&2
  exit 1
fi

printf '%s\n' "$payload"
