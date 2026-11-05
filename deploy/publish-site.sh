#!/usr/bin/env bash
# Publish site/ to Cloudflare Pages (project fortis-rest → https://fortistechlabs.com).
# Linux/macOS twin of publish-site.ps1. See publish-site.md.
#
#   deploy/publish-site.sh            production deploy of the committed site/
#   deploy/publish-site.sh --preview  throwaway preview with its own URL
#
# Auth: CLOUDFLARE_API_TOKEN (Account → Cloudflare Pages → Edit). Taken from the
# environment if set, otherwise asked for (not echoed, never stored).
set -euo pipefail

PROJECT=fortis-rest
# Not a secret: it names the account; the token is what authorises.
ACCOUNT_ID=c322bb8fac729ef05d2f522200d8bdb6
BRANCH=main
[ "${1:-}" = "--preview" ] && BRANCH="preview-$(date +%Y%m%d-%H%M%S)"

cd "$(dirname "$0")/.."
if [ -n "$(git status --porcelain -- site)" ]; then
    echo "site/ has uncommitted changes:"; git status --short -- site
    read -r -p "Publish them anyway? Type yes: " ok
    [ "$ok" = "yes" ] || exit 1
    DIRTY=true
else
    DIRTY=false
fi

TOKEN="${CLOUDFLARE_API_TOKEN:-}"
if [ -z "$TOKEN" ]; then
    read -rs -p "Cloudflare API token: " TOKEN; echo
fi

echo "Publishing site/ → $PROJECT ($BRANCH) at $(git rev-parse --short HEAD)"
CLOUDFLARE_ACCOUNT_ID="$ACCOUNT_ID" CLOUDFLARE_API_TOKEN="$TOKEN" \
    npx --yes wrangler@4 pages deploy site \
    --project-name "$PROJECT" --branch "$BRANCH" \
    --commit-hash "$(git rev-parse HEAD)" \
    --commit-message "$(git log -1 --format=%s)" \
    --commit-dirty "$DIRTY"
unset TOKEN

if [ "$BRANCH" = main ]; then
    echo
    echo "Live version.json (Cloudflare may take a minute to update):"
    curl -s https://fortistechlabs.com/version.json | grep -E '"versionName"|"sha256"' || true
fi
