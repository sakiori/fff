#!/bin/sh
# Fails while the group's compute minutes cannot run publish-npm, the one job on GitLab.com's
# shared runners, so release-tag cuts no release whose npm job would stop on ci_quota_exceeded.
# GitLab shows a namespace's usage to its Owners only: COMPUTE_QUOTA_TOKEN is a read_api token
# of an account that is an Owner of the group (CUSTOM.md, Project settings).
#
# usage: .gitlab/scripts/check-compute-quota.sh <minutes>   (with jq on the PATH)
set -eu

need=${1:?usage: check-compute-quota.sh <minutes>}
: "${CI_API_V4_URL:?}" "${CI_PROJECT_ROOT_NAMESPACE:?}"
: "${COMPUTE_QUOTA_TOKEN:?COMPUTE_QUOTA_TOKEN is not set; CUSTOM.md, Project settings, describes it}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

get() {
  status=$(curl --silent --show-error --connect-timeout 10 --max-time 60 --retry 4 --header "PRIVATE-TOKEN: $COMPUTE_QUOTA_TOKEN" \
    --output "$tmp/$1.json" --write-out '%{http_code}' "$CI_API_V4_URL/$1/$CI_PROJECT_ROOT_NAMESPACE") ||
    { echo "GitLab did not answer for the $1 of $CI_PROJECT_ROOT_NAMESPACE" >&2; exit 1; }
  case "$status" in
    200) ;;
    401) echo "GitLab refused COMPUTE_QUOTA_TOKEN; replace it if it expired (CUSTOM.md, Project settings)" >&2; exit 1 ;;
    *) echo "GitLab answered $status for the $1 of $CI_PROJECT_ROOT_NAMESPACE" >&2; exit 1 ;;
  esac
}

get namespaces
get groups

used=$(jq -er '.ci_minutes_usage.total_minutes_used | floor' "$tmp/namespaces.json") ||
  { echo "GitLab shows no compute usage of $CI_PROJECT_ROOT_NAMESPACE; the answer is not JSON, or COMPUTE_QUOTA_TOKEN does not belong to an Owner" >&2; exit 1; }
monthly=$(jq -er '.shared_runners_minutes_limit | floor' "$tmp/groups.json") ||
  { echo "GitLab shows no monthly compute quota of $CI_PROJECT_ROOT_NAMESPACE" >&2; exit 1; }
purchased=$(jq -r '.extra_shared_runners_minutes_limit // 0 | floor' "$tmp/groups.json")

# A monthly quota of 0 is no limit.
if [ "$monthly" -eq 0 ]; then
  echo "$CI_PROJECT_ROOT_NAMESPACE has no compute quota; $used minutes used this month"
  exit 0
fi

remaining=$((monthly + purchased - used))
echo "$CI_PROJECT_ROOT_NAMESPACE: $remaining compute minutes remain ($monthly monthly and $purchased purchased, $used used); publish-npm needs $need"
if [ "$remaining" -lt "$need" ]; then
  echo "no release is cut: buy compute minutes for the group or wait for the monthly quota to reset, then run the schedule by hand" >&2
  exit 1
fi
