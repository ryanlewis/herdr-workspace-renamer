#!/bin/sh
# Fake herdr CLI for the end-to-end tests. Answers each command from a canned
# reply the test wrote into $FAKE_HERDR_DIR, and logs `workspace rename` and
# `workspace report-metadata` arguments to calls.log and meta.log: each
# argument followed by \037, each call followed by \036.
dir=$FAKE_HERDR_DIR

log() {
	file=$1
	shift
	for arg; do printf '%s\037' "$arg"; done >>"$file"
	printf '\036' >>"$file"
}

case "$1 $2" in
"agent list" | "workspace list" | "tab list" | "pane list")
	cat "$dir/$1-$2.json"
	;;
"workspace get")
	if [ -f "$dir/get-$3.json" ]; then
		cat "$dir/get-$3.json"
	else
		echo "workspace not found" >&2
		exit 1
	fi
	;;
"workspace rename")
	shift 2
	log "$dir/calls.log" "$@"
	echo '{"id":"cli:workspace:rename","result":{"ok":true}}'
	;;
"workspace report-metadata")
	shift 2
	log "$dir/meta.log" "$@"
	# real herdr succeeds silently here: no stdout
	;;
*)
	echo "fake-herdr: unknown command $1 $2" >&2
	exit 1
	;;
esac
