#!/usr/bin/env bash
# The reference SMART Health Links viewer that shl.spec.ts opens links in: shc-web-reader of The
# Commons Project (MIT), pinned at a commit and built as it is, with no patch. `serve <port>` builds it
# once (a few minutes; the build stays in node_modules/.cache, or under OWNHEALTH_SHL_VIEWER_DIR) and
# serves the static build on 127.0.0.1 without a request log. Playwright starts it as its second
# webServer. The viewer compares a file's contentType with "application/fhir+json" as a whole string,
# so the e2e also proves OwnHealth sends exactly that (ownhealth-ops shl.rs CONTENT_TYPE).
set -euo pipefail

repo=https://github.com/the-commons-project/shc-web-reader.git
commit=3824aa69e6874ab9695ddb9c8041d0e6708ce35b # 2026-09-11, reads JWE with raw DEFLATE (its PR 148)
here=$(cd "$(dirname "$0")" && pwd)
dir=${OWNHEALTH_SHL_VIEWER_DIR:-$here/../node_modules/.cache/shl-viewer}/$commit
port=${2:-8191}

build() {
  [ -f "$dir/build/index.html" ] && return 0
  rm -rf "$dir"
  mkdir -p "$dir/src"
  git -C "$dir/src" init -q
  git -C "$dir/src" fetch -q --depth 1 "$repo" "$commit"
  git -C "$dir/src" checkout -q FETCH_HEAD
  [ "$(git -C "$dir/src" rev-parse HEAD)" = "$commit" ] || { echo "the viewer is not at $commit" >&2; exit 1; }
  [ -z "$(git -C "$dir/src" status --porcelain)" ] || { echo "the viewer source is not the pinned commit as it is" >&2; exit 1; }
  # Its lockfile names one dependency by a git+ssh URL. A CI runner has no GitHub key, so that one
  # fetch goes over https, for this command only (never in a git config file). CI=false: the viewer's
  # own lint warnings are not ours to fail on.
  (
    cd "$dir/src"
    GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=ssh://git@github.com/ \
      npm ci --no-audit --no-fund --loglevel=error
    CI=false GENERATE_SOURCEMAP=false PUBLIC_URL=/ npm run build --silent
  )
  mv "$dir/src/build" "$dir/build"
}

case "${1:-}" in
  build) build ;;
  serve)
    build >&2
    exec python3 -I -c '
import functools, http.server, sys
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass
handler = functools.partial(Quiet, directory=sys.argv[2])
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), handler).serve_forever()
' "$port" "$dir/build"
    ;;
  *)
    echo "usage: shl-viewer.sh build | serve [port]" >&2
    exit 2
    ;;
esac
