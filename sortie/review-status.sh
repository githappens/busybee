#!/usr/bin/env bash
# Report the mechanical review state of one pull-request head.
set -euo pipefail

verbose=false
while getopts ':v' option; do
  case "$option" in
    v) verbose=true ;;
    *) printf 'usage: %s [-v] <pr>\n' "${0##*/}" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))
if [ "$#" -ne 1 ] || ! [[ $1 =~ ^[0-9]+$ ]]; then
  printf 'usage: %s [-v] <pr>\n' "${0##*/}" >&2
  exit 2
fi
pr=$1

reviewer=${REVIEWER:-chatgpt-codex-connector[bot]}
gate=${GATE:-github-actions[bot]}
repo=${REPO:-${GH_REPO:-}}
if [ -z "$repo" ]; then
  repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# gh emits one JSON array per page. Slurp and add every listing so old PRs do
# not silently lose the reviews or replies beyond GitHub's first page.
api_array() {
  gh api "$1" --paginate | jq -s 'add // []' >"$2"
}

pr_json=$(gh api "repos/$repo/pulls/$pr")
head=$(jq -er .head.sha <<<"$pr_json")
author=$(jq -er .user.login <<<"$pr_json")
head_date=$(gh api "repos/$repo/commits/$head" --jq .commit.committer.date)
api_array "repos/$repo/pulls/$pr/reviews" "$tmp/reviews.json"
api_array "repos/$repo/pulls/$pr/comments" "$tmp/review-comments.json"
api_array "repos/$repo/issues/$pr/comments" "$tmp/issue-comments.json"
api_array "repos/$repo/issues/$pr/reactions" "$tmp/reactions.json"

jq -n \
  --arg reviewer "$reviewer" --arg author "$author" --arg head "$head" \
  --slurpfile reviews "$tmp/reviews.json" \
  --slurpfile comments "$tmp/review-comments.json" '
    [ $reviews[0][] | select(.user.login == $reviewer and .commit_id == $head) ] as $head_reviews
    | ($head_reviews | map({key: (.id | tostring), value: .submitted_at}) | from_entries) as $times
    | [ $comments[0][]
        | select(
            .user.login == $reviewer
            and .in_reply_to_id == null
            and ($times[.pull_request_review_id | tostring] != null)
          )
        | . as $finding
        | . + {
            priority: (([.body // "" | scan("badge/P[0-9]")] | first) // "badge/unbadged" | split("/")[-1]),
            review_at: $times[.pull_request_review_id | tostring],
            author_replies: [ $comments[0][]
              | select(.in_reply_to_id == $finding.id and .user.login == $author)
              | {id, created_at, body}
            ]
          }
      ] | sort_by(.created_at, .id)
  ' >"$tmp/findings.json"

latest_finding_at=$(jq -r '[.[].review_at] | max // empty' "$tmp/findings.json")
latest_codex_review_at=$(jq -r --arg reviewer "$reviewer" --arg head "$head" '
  [.[] | select(.user.login == $reviewer and .commit_id == $head) | .submitted_at] | max // empty
' "$tmp/reviews.json")
latest_zero_review_at=$(jq -r \
  --arg reviewer "$reviewer" --arg head "$head" \
  --slurpfile findings "$tmp/findings.json" '
    ($findings[0] | map(.pull_request_review_id) | unique) as $with_findings
    | [.[]
       | select(.user.login == $reviewer and .commit_id == $head)
       | select(.id as $id | $with_findings | index($id) == null)
       | .submitted_at]
    | max // empty
  ' "$tmp/reviews.json")

latest_gate_at=$(jq -r --arg gate "$gate" --arg head "$head" '
  [.[] | select(.user.login == $gate and .commit_id == $head) | .submitted_at] | max // empty
' "$tmp/reviews.json")
latest_gate_state=$(jq -r --arg gate "$gate" --arg head "$head" '
  [.[] | select(.user.login == $gate and .commit_id == $head)]
  | sort_by(.submitted_at) | last | .state // empty
' "$tmp/reviews.json")

jq --arg reviewer "$reviewer" --arg head_date "$head_date" '
  [.[] | select(.user.login == $reviewer and .created_at > $head_date)] | sort_by(.created_at)
' "$tmp/issue-comments.json" >"$tmp/codex-issue-comments.json"
jq --arg author "$author" --arg head_date "$head_date" '
  [.[]
   | select(.user.login == $author and .created_at > $head_date)
   | select((.body // "") | test("@codex[[:space:]]+review"; "i"))]
  | sort_by(.created_at)
' "$tmp/issue-comments.json" >"$tmp/rereview-requests.json"

eyes=$(jq --arg reviewer "$reviewer" '
  [.[] | select(.user.login == $reviewer and .content == "eyes")] | length
' "$tmp/reactions.json")
latest_reaction_at=$(jq -r --arg reviewer "$reviewer" --arg head_date "$head_date" '
  [.[] | select(.user.login == $reviewer and .content == "+1" and .created_at > $head_date) | .created_at]
  | max // empty
' "$tmp/reactions.json")
latest_comment_at=$(jq -r '[.[].created_at] | max // empty' "$tmp/codex-issue-comments.json")
latest_rereview_at=$(jq -r '[.[].created_at] | max // empty' "$tmp/rereview-requests.json")

iso_epoch() {
  date -u -d "$1" +%s 2>/dev/null \
    || date -j -u -f '%Y-%m-%dT%H:%M:%SZ' "$1" +%s
}
head_epoch=$(iso_epoch "$head_date")
age=$(( $(date -u +%s) - head_epoch ))

newer_than() {
  [ -n "$1" ] && { [ -z "$2" ] || [[ $1 > $2 ]]; }
}
latest_codex_activity=$(printf '%s\n%s\n%s\n' \
  "$latest_codex_review_at" "$latest_comment_at" "$latest_reaction_at" | sort | tail -1)

# A gate review newer than Codex's activity means that activity has already
# been classified. Only later unpinned signals need another model judgement.
activity_cutoff=$head_date
if newer_than "$latest_gate_at" "$activity_cutoff"; then
  activity_cutoff=$latest_gate_at
fi
new_comment_ids=$(jq -r --arg cutoff "$activity_cutoff" '
  [.[] | select(.created_at > $cutoff) | (.id | tostring)] | join(",")
' "$tmp/codex-issue-comments.json")
new_comment_at=$(jq -r --arg cutoff "$activity_cutoff" '
  [.[] | select(.created_at > $cutoff) | .created_at] | max // empty
' "$tmp/codex-issue-comments.json")

status=
if [ "$eyes" -gt 0 ]; then
  status=unknown:review-in-flight
elif { newer_than "$latest_reaction_at" "$latest_gate_at" || newer_than "$new_comment_at" "$latest_gate_at"; } \
  && [ "$age" -lt 480 ]; then
  status=unknown:head-not-quiescent
elif newer_than "$latest_reaction_at" "$latest_finding_at" \
  && newer_than "$latest_reaction_at" "$latest_gate_at"; then
  status=approvable
elif newer_than "$latest_zero_review_at" "$latest_finding_at" \
  && newer_than "$latest_zero_review_at" "$latest_gate_at"; then
  status=approvable
elif [ -n "$new_comment_ids" ] && newer_than "$new_comment_at" "$latest_finding_at"; then
  status="unknown:classify-comment-$new_comment_ids"
elif newer_than "$latest_rereview_at" "$latest_codex_activity"; then
  status=unknown:re-review-requested
elif [ "$latest_gate_state" = APPROVED ] && ! newer_than "$latest_codex_activity" "$latest_gate_at"; then
  status=approvable
elif [ -s "$tmp/findings.json" ] && [ "$(jq length "$tmp/findings.json")" -gt 0 ]; then
  blocked=$(jq -r '[.[] | select(.priority != "P2" and .priority != "P3") | (.id | tostring)] | join(",")' "$tmp/findings.json")
  waiting=$(jq -r '[.[] | select((.priority == "P2" or .priority == "P3") and (.author_replies | length == 0)) | (.id | tostring)] | join(",")' "$tmp/findings.json")
  if [ -n "$blocked" ]; then
    status="blocked:$blocked"
  elif [ -n "$waiting" ]; then
    status="waiting:$waiting"
  else
    status=approvable
  fi
elif [ -n "$new_comment_ids" ]; then
  status="unknown:classify-comment-$new_comment_ids"
elif newer_than "$latest_rereview_at" "$head_date"; then
  status=unknown:re-review-requested
else
  status=unknown:no-codex-signal
fi

if $verbose; then
  {
    printf 'review-status: pr=#%s head=%s head_date=%s age_seconds=%s author=%s\n' \
      "$pr" "$head" "$head_date" "$age" "$author"
    jq -r '
      .[]
      | "finding: id=\(.id) priority=\(.priority) review=\(.pull_request_review_id) at=\(.review_at) reply=\(if (.author_replies | length) > 0 then "yes" else "no" end) path=\(.path):\(.line // .original_line // "?")\n  body: \((.body // "") | gsub("\\n"; " "))",
        (.author_replies[] | "  author-reply: id=\(.id) at=\(.created_at) body=\((.body // "") | gsub("\\n"; " "))")
    ' "$tmp/findings.json"
    jq -r '.[] | "codex-comment: id=\(.id) at=\(.created_at) body=\((.body // "") | gsub("\\n"; " "))"' \
      "$tmp/codex-issue-comments.json"
    jq -r '.[] | "re-review-request: id=\(.id) at=\(.created_at) body=\((.body // "") | gsub("\\n"; " "))"' \
      "$tmp/rereview-requests.json"
    printf 'signals: eyes=%s thumbs_up_at=%s zero_findings_review_at=%s gate=%s@%s re_review_requested_at=%s\n' \
      "$eyes" "${latest_reaction_at:-none}" "${latest_zero_review_at:-none}" \
      "${latest_gate_state:-none}" "${latest_gate_at:-none}" "${latest_rereview_at:-none}"
    printf 'verdict: %s\n' "$status"
  } >&2
fi

printf '%s\n' "$status"
