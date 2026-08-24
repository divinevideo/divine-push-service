#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
builder="$script_dir/build-deploy-payload.sh"
sha="0123456789abcdef0123456789abcdef01234567"
repository="divinevideo/divine-push-service"

assert_payload() {
  local description="$1"
  local event_name="$2"
  local ref="$3"
  local include_production_input="$4"
  local ref_name="$5"
  local expected_environments="$6"
  local expected_auto_promote="$7"
  local payload

  payload="$($builder "$event_name" "$ref" "$include_production_input" "$sha" "$repository" "$ref_name")"

  jq -e \
    --argjson expected_environments "$expected_environments" \
    --argjson expected_auto_promote "$expected_auto_promote" \
    --arg sha "$sha" \
    --arg repository "$repository" '
      .event_type == "image-deploy"
      and .client_payload.application == "divine-push-service"
      and .client_payload.environments == $expected_environments
      and .client_payload.images == [{"name":"divine-push-service","tag":"0123456"}]
      and .client_payload.source_repo == $repository
      and .client_payload.source_sha == $sha
      and ((.client_payload.auto_promote // false) == $expected_auto_promote)
      and (
        if (.client_payload.environments | index("production")) != null then
          .client_payload.auto_promote == true
        else
          (.client_payload | has("auto_promote") | not)
        end
      )
    ' >/dev/null <<<"$payload"

  echo "ok - $description"
}

assert_payload \
  "push to main excludes production" \
  "push" "refs/heads/main" "false" "main" \
  '["poc","staging"]' "false"

assert_payload \
  "version tag authorizes production auto-promotion" \
  "push" "refs/tags/v1.2.3" "false" "v1.2.3" \
  '["poc","staging","production"]' "true"

assert_payload \
  "manual production opt-in authorizes auto-promotion" \
  "workflow_dispatch" "refs/heads/main" "true" "main" \
  '["poc","staging","production"]' "true"

assert_payload \
  "manual dispatch without opt-in excludes production" \
  "workflow_dispatch" "refs/heads/main" "false" "main" \
  '["poc","staging"]' "false"
