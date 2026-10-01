#!/usr/bin/env bash
# Push the current version to App Store Connect: checks the release is
# tagged and consistent, then runs archive-ios.sh and archive-macos.sh
# with the same build number and uploads both. Wired up as "release" in
# Makefile.toml and "release-appstore" in paseo.json. On Linux the checks
# run here and the builds on the Mac build machine (scripts/on-mac.sh).
#
#   scripts/release-appstore.sh            # iPad + Mac, upload both
#   scripts/release-appstore.sh --ios      # iPad only
#   scripts/release-appstore.sh --macos    # Mac only
#   scripts/release-appstore.sh --export   # archive + export, no upload
#   scripts/release-appstore.sh --tag      # create and push v<version> first
#   scripts/release-appstore.sh --clean    # cargo clean afterwards
#
# Checks (skip with --skip-checks): clean working tree, Cargo.toml version
# equals MARKETING_VERSION in both project.yml files, tag v<version> exists
# and points at HEAD (--tag creates and pushes it, which also starts the
# Gitea Linux package build). The build number is the commit count, as in
# the archive scripts, computed once so both platforms share it.
#
# Env: KRABINK_TEAM, KRABINK_ASC_KEY_PATH/_ID/_ISSUER_ID and the other
# KRABINK_* variables of archive-ios.sh are passed through.
set -euo pipefail

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

ios=1
macos=1
destination=upload
tag=0
clean=0
checks=1
only=
for arg in "$@"; do
  case "$arg" in
    --ios) only=ios ;;
    --macos) only=macos ;;
    --export) destination=export ;;
    --tag) tag=1 ;;
    --clean) clean=1 ;;
    --skip-checks) checks=0 ;;
    -h | --help) usage; exit 0 ;;
    *)
      echo "error: unknown argument $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done
case "$only" in
  ios) macos=0 ;;
  macos) ios=0 ;;
esac

root=$(git rev-parse --show-toplevel)
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml)
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: can't read the workspace version in Cargo.toml" >&2
  exit 1
fi

if [[ "$checks" == 1 ]]; then
  if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: working tree is not clean; commit or stash first" >&2
    exit 1
  fi
  for f in ios/Krabink/project.yml macos/project.yml; do
    if ! grep -q "MARKETING_VERSION: \"$version\"" "$f"; then
      echo "error: $f MARKETING_VERSION is not $version (run cargo make bump)" >&2
      exit 1
    fi
  done

  head=$(git rev-parse HEAD)
  if git rev-parse -q --verify "refs/tags/v$version^{commit}" >/dev/null; then
    tagged=$(git rev-parse "v$version^{commit}")
    if [[ "$tagged" != "$head" ]]; then
      echo "error: tag v$version is at ${tagged:0:7}, HEAD is ${head:0:7}" >&2
      echo "       release from the tag, or bump the version for a new release" >&2
      exit 1
    fi
  elif [[ "$tag" == 1 ]]; then
    echo "==> tagging v$version at ${head:0:7}"
    git tag -a "v$version" -m "Release $version"
    git push origin "v$version"
  else
    echo "error: no tag v$version; rerun with --tag to create and push it" >&2
    exit 1
  fi
fi

export KRABINK_BUILD_NUMBER=${KRABINK_BUILD_NUMBER:-$(git rev-list --count HEAD)}
export KRABINK_EXPORT_DESTINATION=$destination

# Builds happen on the Mac; the checks above already ran, so the mirror
# (which has a throwaway git repo) must not repeat them.
if [[ "$(uname)" != "Darwin" ]]; then
  args=(--skip-checks)
  [[ "$ios" == 0 ]] && args+=(--macos)
  [[ "$macos" == 0 ]] && args+=(--ios)
  [[ "$destination" == export ]] && args+=(--export)
  [[ "$clean" == 1 ]] && args+=(--clean)
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "${args[@]}"
fi

logs=${TMPDIR:-/tmp}/krabink-release-$version
mkdir -p "$logs"
echo "==> releasing $version (build $KRABINK_BUILD_NUMBER, $destination); logs in $logs"

done_platforms=()
if [[ "$ios" == 1 ]]; then
  scripts/archive-ios.sh 2>&1 | tee "$logs/ios.log"
  done_platforms+=(iPad)
fi
if [[ "$macos" == 1 ]]; then
  scripts/archive-macos.sh 2>&1 | tee "$logs/macos.log"
  done_platforms+=(Mac)
fi

if [[ "$clean" == 1 ]]; then
  echo "==> cargo clean"
  cargo clean
fi

echo
echo "==> $version build $KRABINK_BUILD_NUMBER: ${done_platforms[*]} ${destination}ed"
if [[ "$destination" == upload ]]; then
  echo "    next, in App Store Connect once processing finishes (5-30 min):"
  echo "    attach build $KRABINK_BUILD_NUMBER to version $version, fill What's New, submit."
fi
