#!/usr/bin/env bash
#
# scripts/release.sh - cut a release of the `alan` crate.
#
# 1. Shows the current version (crates/alan/Cargo.toml), the latest `v*` tag,
#    and the newest CHANGELOG release, then proposes the next patch version.
# 2. Bumps crates/alan/Cargo.toml, renames `## [Unreleased]` in CHANGELOG.md to
#    `## <version> - <date>` with a fresh empty `[Unreleased]` on top (per
#    AGENTS.md), and refreshes Cargo.lock with `cargo update -p alan`.
# 3. Runs the same checks CI runs: fmt, clippy, tests.
# 4. Shows the git commands (stage+commit, annotated tag, push) and asks once;
#    on confirm it runs them all, otherwise it prints them for later.
#
# Pushing the tag is what triggers .github/workflows/release.yml.
#
# Usage: scripts/release.sh [--dry-run]

set -euo pipefail

SEMVER_RE='^[0-9]+\.[0-9]+\.[0-9]+$'

DRY_RUN=0
case "${1:-}" in
"") ;;
--dry-run) DRY_RUN=1 ;;
-h | --help)
	echo "usage: scripts/release.sh [--dry-run]"
	exit 0
	;;
*)
	echo "error: unknown option '$1'" >&2
	echo "usage: scripts/release.sh [--dry-run]" >&2
	exit 1
	;;
esac

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
	echo "error: not inside a git repository" >&2
	exit 1
}
cd "$REPO_ROOT"

CARGO_TOML="crates/alan/Cargo.toml"
CHANGELOG="CHANGELOG.md"
LOCKFILE="Cargo.lock"

info() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

# Prints the prompt to stderr so it is visible even when stdin is piped.
# Accepts y/yes, case-insensitive; anything else (or EOF) is "no".
confirm() {
	local reply
	printf '%s' "$1" >&2
	read -r reply
	[[ "$reply" =~ ^[Yy]([Ee][Ss])?$ ]]
}

if [[ $DRY_RUN == 1 ]]; then
	info "dry-run: no files will be changed and no commands will run"
fi

# --- preflight ---------------------------------------------------------------

command -v cargo >/dev/null 2>&1 || die "cargo not found in PATH"
command -v perl >/dev/null 2>&1 || die "perl not found in PATH"
[[ -f $CARGO_TOML ]] || die "$CARGO_TOML not found"
[[ -f $CHANGELOG ]] || die "$CHANGELOG not found"
grep -q '^version = "' "$CARGO_TOML" || die "no version line found in $CARGO_TOML"
grep -q '^## \[Unreleased\]' "$CHANGELOG" || die "no '## [Unreleased]' heading found in $CHANGELOG"

branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" != "HEAD" ]] || die "detached HEAD; check out a branch first"
git remote get-url origin >/dev/null 2>&1 || warn "no 'origin' remote configured; push will fail"

if [[ -n "$(git status --porcelain)" ]]; then
	warn "working tree is not clean:"
	git status --short
	if [[ $DRY_RUN == 1 ]]; then
		warn "continuing (dry-run)"
	elif ! confirm "Continue with a dirty working tree? [y/N] "; then
		exit 1
	fi
fi

# --- versions ----------------------------------------------------------------

current_version="$(perl -ne 'if (/^\[package\]/) { $pkg = 1 } elsif ($pkg && /^version\s*=\s*"([^"]+)"/) { print $1; exit }' "$CARGO_TOML")"
[[ -n "$current_version" ]] || die "could not read version from $CARGO_TOML"

latest_tag="$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n 1)"
tag_version=""
[[ -n "$latest_tag" ]] && tag_version="${latest_tag#v}"

changelog_version="$(grep -E '^## [0-9]+\.[0-9]+\.[0-9]+' "$CHANGELOG" | head -n 1 | awk '{print $2}' || true)"

next_version=""
if [[ "$current_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
	next_version="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"
fi

info "Current version:  $current_version  ($CARGO_TOML)"
info "Previous release: ${latest_tag:-<none>}"
info "Last changelog:   ${changelog_version:-<none>}"
info "Next patch:       ${next_version:-<cannot compute>}"

if [[ -n "$tag_version" && "$tag_version" != "$current_version" ]]; then
	warn "latest tag $latest_tag does not match $CARGO_TOML version $current_version"
fi
if [[ -n "$changelog_version" && "$changelog_version" != "$current_version" ]]; then
	warn "latest CHANGELOG release $changelog_version does not match $CARGO_TOML version $current_version"
fi

unreleased_bullets="$(awk '/^## \[Unreleased\]/{keep = 1; next} keep && /^## /{keep = 0} keep && /^- /{n++} END {print n + 0}' "$CHANGELOG")"
if [[ "$unreleased_bullets" -eq 0 ]]; then
	warn "'## [Unreleased]' has no bullet entries; release notes will be empty"
	if [[ $DRY_RUN != 1 ]] && ! confirm "Release anyway? [y/N] "; then
		exit 1
	fi
fi

# --- pick the version --------------------------------------------------------

echo
if [[ -n "$next_version" ]]; then
	printf 'Version to release [%s]: ' "$next_version" >&2
else
	printf 'Version to release: ' >&2
fi
read -r version_reply
new_version="${version_reply:-$next_version}"
[[ -n "$new_version" ]] || die "no version entered and next patch could not be computed"
[[ "$new_version" =~ $SEMVER_RE ]] || die "'$new_version' is not X.Y.Z"
[[ "$new_version" != "$current_version" ]] || die "$new_version is already the current version"
if git rev-parse -q --verify "refs/tags/v$new_version" >/dev/null 2>&1; then
	die "tag v$new_version already exists"
fi
if [[ "$(printf '%s\n%s\n' "$new_version" "$current_version" | sort -V | head -n 1)" == "$new_version" ]]; then
	warn "$new_version is lower than the current version $current_version"
	if [[ $DRY_RUN != 1 ]] && ! confirm "Continue anyway? [y/N] "; then
		exit 1
	fi
fi

# --- planned changes ---------------------------------------------------------

date_today="$(date +%Y-%m-%d)"

echo
info "Planned changes:"
echo "  1. $CARGO_TOML: version = \"$current_version\" -> version = \"$new_version\""
echo "  2. $CHANGELOG: rename '## [Unreleased]' to '## $new_version - $date_today' (fresh empty '[Unreleased]' on top)"
echo "  3. $LOCKFILE: cargo update -p alan"
echo "  4. cargo fmt --all -- --check"
echo "  5. cargo clippy --workspace --all-targets --locked -- -D warnings"
echo "  6. cargo test --workspace --locked"
echo "Git commands (run together after one y/n confirm):"
echo "  git add -- $LOCKFILE $CHANGELOG $CARGO_TOML"
echo "  git commit -m \"release v$new_version\""
echo "  git tag -a v$new_version -m \"release v$new_version\""
echo "  git push origin $branch v$new_version"

if [[ $DRY_RUN == 1 ]]; then
	echo
	info "dry-run complete; nothing was changed"
	exit 0
fi

# --- apply the bump ----------------------------------------------------------

info "Updating $CARGO_TOML"
NEW_VERSION="$new_version" perl -pi -e \
	'if (!$done && s/^version = "[^"]+"/version = "$ENV{NEW_VERSION}"/) { $done = 1 }' \
	"$CARGO_TOML"
grep -Fq "version = \"$new_version\"" "$CARGO_TOML" || die "failed to bump $CARGO_TOML"

info "Updating $CHANGELOG"
NEW_VERSION="$new_version" RELEASE_DATE="$date_today" perl -pi -e \
	'if (!$done && /^## \[Unreleased\]\s*$/) { $_ = "## [Unreleased]\n\n## $ENV{NEW_VERSION} - $ENV{RELEASE_DATE}\n"; $done = 1 }' \
	"$CHANGELOG"
grep -Fq "## $new_version - $date_today" "$CHANGELOG" || die "failed to update $CHANGELOG"
grep -q '^## \[Unreleased\]' "$CHANGELOG" || die "lost the '## [Unreleased]' heading in $CHANGELOG"

info "Refreshing $LOCKFILE"
cargo update -p alan

echo
git --no-pager diff --stat -- "$CARGO_TOML" "$CHANGELOG" "$LOCKFILE"
git --no-pager diff -- "$CARGO_TOML" "$CHANGELOG"

# --- ci checks ---------------------------------------------------------------

echo
info "Running CI checks (fmt, clippy, test) - this can take a while"
printf '$ cargo fmt --all -- --check\n'
cargo fmt --all -- --check || die "cargo fmt failed - fix it, then re-run scripts/release.sh"
printf '$ cargo clippy --workspace --all-targets --locked -- -D warnings\n'
cargo clippy --workspace --all-targets --locked -- -D warnings || die "clippy failed - fix it, then re-run scripts/release.sh"
printf '$ cargo test --workspace --locked\n'
cargo test --workspace --locked || die "tests failed - fix them, then re-run scripts/release.sh"

# --- git steps, one confirm for the whole sequence ---------------------------

echo
echo "  git add -- $LOCKFILE $CHANGELOG $CARGO_TOML"
echo "  git commit -m \"release v$new_version\""
echo "  git tag -a v$new_version -m \"release v$new_version\""
echo "  git push origin $branch v$new_version"
if confirm "Run these git commands? (commit, tag, push; push starts the release build) [y/N] "; then
	git add -- "$LOCKFILE" "$CHANGELOG" "$CARGO_TOML"
	git commit -m "release v$new_version"
	git tag -a "v$new_version" -m "release v$new_version"
	git push origin "$branch" "v$new_version"
	info "Released v$new_version; .github/workflows/release.yml is building the artifacts."
else
	info "Release prepared for v$new_version. Commands still to run:"
	warn "the version bump is still uncommitted in the working tree"
	echo "  git add -- $LOCKFILE $CHANGELOG $CARGO_TOML"
	echo "  git commit -m \"release v$new_version\""
	echo "  git tag -a v$new_version -m \"release v$new_version\""
	echo "  git push origin $branch v$new_version"
fi
