#!/bin/sh
# Run once, locally, as a repo admin (after `gh auth login`):
#
#     sh .github/repo-setup.sh
#
# It makes the GitHub repo match how this project is meant to be run:
#
#   * Squash-only merges, so main carries exactly one Conventional Commit per PR.
#     That commit subject IS the release note — `release.yml` assembles the notes
#     from the commit log since the previous tag (there is no CHANGELOG.md).
#   * A `main` ruleset that blocks deletion and force-push and requires the single
#     aggregate `CI` check to be green before a PR can merge. Branch protection
#     points at one job on purpose: a new gate is added to that job's `needs:`
#     list, never wired here (see docs/05-delivery/53-quality-gates.md).
#   * A `release-tags` ruleset that refuses to delete, move or force-push a tag,
#     and rejects any tag name that is not `vX.Y.Z` semver. Releases are cut only
#     by pushing a signed `vX.Y.Z` tag.
set -e
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)

gh api -X PATCH "repos/$repo" \
  -F allow_merge_commit=false \
  -F allow_rebase_merge=false \
  -F allow_squash_merge=true \
  -f squash_merge_commit_title=PR_TITLE \
  -f squash_merge_commit_message=PR_BODY \
  -F delete_branch_on_merge=true >/dev/null
echo "merge settings: squash-only, PR title as subject, branches auto-deleted"

for f in .github/rulesets/*.json; do
  name=$(sed -n 's/.*"name": "\([^"]*\)".*/\1/p' "$f" | head -n1)
  id=$(gh api "repos/$repo/rulesets" --jq ".[] | select(.name==\"$name\") | .id" | head -n1)
  if [ -n "$id" ]; then
    gh api -X PUT "repos/$repo/rulesets/$id" --input "$f" >/dev/null
    echo "ruleset updated: $name"
  else
    gh api -X POST "repos/$repo/rulesets" --input "$f" >/dev/null
    echo "ruleset created: $name"
  fi
done
echo "done: $repo"
