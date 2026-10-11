#!/bin/sh
# Updates openclaw-rs to the latest release: the same as install.sh --update
# (checksum checked, the previous binary kept as openclaw-rs.old, a running
# OpenRC service restarted when run as root).
#
#   curl -fsSL https://raw.githubusercontent.com/chappie220/openclaw/main/scripts/update.sh | doas sh
#   sh update.sh --version 0.2.0      # a given release, also to go back
#
# Takes the same options as install.sh (--dir, --version, --force).
# `openclaw-rs update` does the same from the binary itself; this script
# also works when the installed binary is too old to have it, or broken.

set -eu

# Run next to install.sh (a checkout), or download it.
case "$0" in
*update.sh)
	here=$(cd "$(dirname "$0")" && pwd)
	if [ -f "$here/install.sh" ]; then
		exec sh "$here/install.sh" --update "$@"
	fi
	;;
esac

repo="${OPENCLAW_REPO:-chappie220/openclaw}"
url="${OPENCLAW_INSTALL_URL:-https://raw.githubusercontent.com/$repo/main/scripts/install.sh}"
tmp=$(mktemp "${TMPDIR:-/tmp}/openclaw-install.XXXXXX")
trap 'rm -f "$tmp"' EXIT INT TERM
if command -v curl >/dev/null 2>&1; then
	curl -fsSL --retry 3 -o "$tmp" "$url"
elif command -v wget >/dev/null 2>&1; then
	wget -q -O "$tmp" "$url"
else
	echo "error: needs curl or wget (doas apk add curl)" >&2
	exit 1
fi
sh "$tmp" --update "$@"
