#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
script="$root/sortie/review-status.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/fixture"

cat >"$tmp/bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
fixture=${REVIEW_STATUS_FIXTURE:?}
if [ "$1" != api ]; then
  echo "unexpected gh command: $*" >&2
  exit 2
fi
endpoint=$2
case "$endpoint" in
  repos/example/busybee/pulls/1)
    cat "$fixture/pr.json"
    ;;
  repos/example/busybee/commits/head)
    printf '%s\n' '2020-01-01T00:00:00Z'
    ;;
  repos/example/busybee/pulls/1/reviews)
    [[ " $* " == *' --paginate '* ]] || { echo 'reviews call was not paginated' >&2; exit 3; }
    cat "$fixture/reviews.json"
    ;;
  repos/example/busybee/pulls/1/comments)
    [[ " $* " == *' --paginate '* ]] || { echo 'review-comments call was not paginated' >&2; exit 3; }
    cat "$fixture/review-comments.json"
    ;;
  repos/example/busybee/issues/1/comments)
    [[ " $* " == *' --paginate '* ]] || { echo 'issue-comments call was not paginated' >&2; exit 3; }
    cat "$fixture/issue-comments.json"
    ;;
  repos/example/busybee/issues/1/reactions)
    [[ " $* " == *' --paginate '* ]] || { echo 'reactions call was not paginated' >&2; exit 3; }
    cat "$fixture/reactions.json"
    ;;
  *)
    echo "unexpected endpoint: $endpoint" >&2
    exit 2
    ;;
esac
GH
chmod +x "$tmp/bin/gh"

fixture="$tmp/fixture"
printf '%s\n' '{"head":{"sha":"head"},"user":{"login":"author"}}' >"$fixture/pr.json"
reset_fixture() {
  printf '[]\n' >"$fixture/reviews.json"
  printf '[]\n' >"$fixture/review-comments.json"
  printf '[]\n' >"$fixture/issue-comments.json"
  printf '[]\n' >"$fixture/reactions.json"
}

run_status() {
  PATH="$tmp/bin:$PATH" REPO=example/busybee REVIEW_STATUS_FIXTURE="$fixture" \
    "$script" "$@"
}

expect_status() {
  local name=$1 expected=$2 actual
  actual=$(run_status 1)
  if [ "$actual" != "$expected" ]; then
    printf 'not ok - %s: expected %s, got %s\n' "$name" "$expected" "$actual" >&2
    exit 1
  fi
  printf 'ok - %s\n' "$name"
}

review() {
  local body=${1:-review}
  jq -nc --arg body "$body" '[{id:10,user:{login:"chatgpt-codex-connector[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:02:00Z",state:"COMMENTED",body:$body}]' \
    >"$fixture/reviews.json"
}
finding() {
  local priority=$1
  jq -nc --arg priority "$priority" '[{id:100,pull_request_review_id:10,in_reply_to_id:null,user:{login:"chatgpt-codex-connector[bot]"},created_at:"2020-01-01T00:02:00Z",body:("https://img.shields.io/badge/"+$priority+"-orange finding"),path:"src/lib.rs",line:7}]' \
    >"$fixture/review-comments.json"
}

reset_fixture
expect_status 'no reviewer signal is unknown' 'unknown:no-codex-signal'

reset_fixture
review
finding P1
expect_status 'P1 finding blocks' 'blocked:100'

reset_fixture
review
finding P2
expect_status 'unanswered P2 waits' 'waiting:100'

jq '. + [{id:101,pull_request_review_id:10,in_reply_to_id:100,user:{login:"author"},created_at:"2020-01-01T00:03:00Z",body:"Deferred with a reason."}]' \
  "$fixture/review-comments.json" >"$fixture/review-comments.next"
mv "$fixture/review-comments.next" "$fixture/review-comments.json"
expect_status 'answered P2 needs reply wording judge' 'unknown:judge-replies'

reset_fixture
review
finding P2
jq '. + [{id:101,pull_request_review_id:10,in_reply_to_id:100,user:{login:"author"},created_at:"2020-01-01T00:03:00Z",body:"Deferred with a reason."}]' \
  "$fixture/review-comments.json" >"$fixture/review-comments.next"
mv "$fixture/review-comments.next" "$fixture/review-comments.json"
jq -nc '[{id:20,user:{login:"github-actions[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:04:00Z",state:"APPROVED",body:"codex-gate"}]' \
  >"$fixture/reviews.json"
expect_status 'answered P2 after gate judged replies is approvable' 'approvable'

reset_fixture
jq -nc '[{id:200,user:{login:"chatgpt-codex-connector[bot]"},content:"+1",created_at:"2020-01-01T00:05:00Z"}]' \
  >"$fixture/reactions.json"
expect_status 'quiescent thumbs-up is approvable' 'approvable'

reset_fixture
jq -nc '[{id:201,user:{login:"chatgpt-codex-connector[bot]"},content:"eyes",created_at:"2020-01-01T00:05:00Z"}]' \
  >"$fixture/reactions.json"
expect_status 'eyes means review in flight' 'unknown:review-in-flight'

reset_fixture
jq -nc '[{id:300,user:{login:"chatgpt-codex-connector[bot]"},created_at:"2020-01-01T00:05:00Z",body:"Review complete."}]' \
  >"$fixture/issue-comments.json"
expect_status 'freeform reviewer comment needs classification' 'unknown:classify-comment-300'

reset_fixture
review
finding P1
jq -nc '[{id:301,user:{login:"author"},created_at:"2020-01-01T00:04:00Z",body:"@codex review\nPlease reconsider thread 100."}]' \
  >"$fixture/issue-comments.json"
expect_status 'one re-review request is remembered' 'unknown:re-review-requested'

reset_fixture
review 'No findings.'
expect_status 'review with zero inline findings is approvable' 'approvable'

# Verbose evidence goes to stderr without changing the one-line stdout contract.
reset_fixture
review
finding P2
stdout=$(run_status -v 1 2>"$tmp/evidence")
test "$stdout" = 'waiting:100'
grep -q 'finding: id=100 priority=P2' "$tmp/evidence"
grep -q 'verdict: waiting:100' "$tmp/evidence"
printf 'ok - verbose evidence\n'

reset_fixture
jq -nc '[{id:20,user:{login:"github-actions[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:01:00Z",state:"APPROVED",body:"codex-gate"}]' \
  >"$fixture/reviews.json"
jq -nc '[{id:10,user:{login:"chatgpt-codex-connector[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:02:00Z",state:"COMMENTED",body:"findings"}]' \
  >"$fixture/reviews.next"
jq -s 'add' "$fixture/reviews.json" "$fixture/reviews.next" >"$fixture/reviews.json"
finding P2
jq '. + [{id:101,pull_request_review_id:10,in_reply_to_id:100,user:{login:"author"},created_at:"2020-01-01T00:03:00Z",body:"will fix"}]' \
  "$fixture/review-comments.json" >"$fixture/review-comments.next"
mv "$fixture/review-comments.next" "$fixture/review-comments.json"
expect_status 'stale gate approval after new findings needs judge' 'unknown:judge-replies'

reset_fixture
jq -nc '[{id:10,user:{login:"chatgpt-codex-connector[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:01:00Z",state:"COMMENTED",body:"P1 review"}]' \
  >"$fixture/reviews.json"
jq -nc '[{id:100,pull_request_review_id:10,in_reply_to_id:null,user:{login:"chatgpt-codex-connector[bot]"},created_at:"2020-01-01T00:01:00Z",body:"https://img.shields.io/badge/P1-orange finding",path:"src/lib.rs",line:7}]' \
  >"$fixture/review-comments.json"
jq -nc '[{id:200,user:{login:"chatgpt-codex-connector[bot]"},content:"+1",created_at:"2020-01-01T00:02:00Z"}]' \
  >"$fixture/reactions.json"
jq -nc '[{id:11,user:{login:"chatgpt-codex-connector[bot]"},commit_id:"head",submitted_at:"2020-01-01T00:03:00Z",state:"COMMENTED",body:"P2 review"}]' \
  >"$fixture/reviews.next"
jq -s 'add' "$fixture/reviews.json" "$fixture/reviews.next" >"$fixture/reviews.json"
jq '. + [{id:101,pull_request_review_id:11,in_reply_to_id:null,user:{login:"chatgpt-codex-connector[bot]"},created_at:"2020-01-01T00:03:00Z",body:"https://img.shields.io/badge/P2-orange finding",path:"src/lib.rs",line:8}]' \
  "$fixture/review-comments.json" >"$fixture/review-comments.next"
mv "$fixture/review-comments.next" "$fixture/review-comments.json"
expect_status 'superseded P1 does not resurrect after clean signal' 'waiting:101'
