#!/usr/bin/env bash
# Release check 13.8 for issue #17 (see the plan, Task 13): a keg installed
# from another non-core tap is moved to brewsoak's own tap by brew's upgrade.
#
# This drives REAL brew with raw commands and changes real state (a throwaway
# tap, one installed formula, trust entries). Usage:
#   release-check-17.sh --run        run the check, clean up, compare state
#   release-check-17.sh --cleanup    remove leftovers of an interrupted run
#   release-check-17.sh --self-test  test this script against a fake brew
#
# Exit codes: 0 pass (or skipped because hello is installed), 1 the check or
# the cleanup failed, 2 a precondition is missing.
#
# Second run: a finished run leaves no state, so it behaves like the first.
# Overlapping runs: a lock directory refuses the second one.
# Leftovers of a failed run: --run refuses to start and names --cleanup, which
# acts only on what the marker file of the failed run recorded. The marker and
# the lock live in ${XDG_STATE_HOME:-$HOME/.local/state}/brewsoak, so a changed
# TMPDIR or a temp purge does not lose the marker. Both survive a reboot. A
# SIGKILL or a power loss leaves the lock in place until it is removed with the
# command the die message names (rm -r <lock>).
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
SOAK_TAP="ericfitz/brewsoak"
SPIKE_TAP="spike17c/t3"
PKG="hello"

die() {
	echo "release-check-17: $*" >&2
	exit 2
}

# Resolve tools to absolute paths, failing fast with what to install.
init_tools() {
	export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1
	BREW="$(command -v brew || true)"
	[[ "$BREW" == /* ]] || die "brew not found on PATH; install Homebrew first."
	JQ="$(command -v jq || true)"
	[[ "$JQ" == /* ]] || die "jq not found on PATH; run: brew install jq"
	SHASUM="$(command -v shasum || true)"
	[[ "$SHASUM" == /* ]] || die "shasum not found on PATH."
	REPO="$("$BREW" --repository)"
	CELLAR="$("$BREW" --cellar)"
	CASKROOM="$("$BREW" --caskroom)"
	[[ "$REPO" == /* && "$CELLAR" == /* && "$CASKROOM" == /* ]] ||
		die "brew did not print absolute --repository, --cellar and --caskroom paths."
	TAP="$REPO/Library/Taps/ericfitz/homebrew-brewsoak"
	SPIKE="$REPO/Library/Taps/spike17c/homebrew-t3"
	local state
	if [[ -n "${XDG_STATE_HOME:-}" ]]; then
		state="$XDG_STATE_HOME/brewsoak"
	elif [[ -n "${HOME:-}" ]]; then
		state="$HOME/.local/state/brewsoak"
	else
		state=""
	fi
	[[ "$state" == /* ]] || die "set XDG_STATE_HOME or HOME to an absolute path; the marker needs a durable directory."
	mkdir -p -- "$state" || die "cannot create $state for the marker and lock."
	MARKER="$state/release-check-17.marker"
	LOCK="$state/release-check-17.lock"
}

receipts() { # <label> <root>
	local label="$1" root="$2" f
	[[ -d "$root" ]] || return 0
	(cd "$root" && find . -maxdepth 3 -name INSTALL_RECEIPT.json | LC_ALL=C sort) |
		while IFS= read -r f; do
			printf '%s/%s  %s\n' "$label" "${f#./}" "$("$SHASUM" -a 256 <"$root/$f" | cut -d' ' -f1)"
		done
}

# The state this check must leave as it found it.
snapshot() {
	echo "## taps"
	"$BREW" tap | LC_ALL=C sort
	echo "## formulae"
	"$BREW" list --formula | LC_ALL=C sort
	echo "## casks"
	"$BREW" list --cask | LC_ALL=C sort
	echo "## trust"
	"$BREW" trust --json=v1 | "$JQ" -S .
	echo "## receipts"
	receipts cellar "$CELLAR"
	receipts caskroom "$CASKROOM"
}

indent() { # <text>: print each line indented two spaces
	local l
	while IFS= read -r l; do printf '  %s\n' "$l"; done <<<"$1"
}

# trust_query <taps|formulae> <entry>: exact match in brew's trust list.
# Returns 0 listed, 1 not listed, 2 brew or jq could not answer.
trust_query() {
	local json rc=0
	TRUST_WHY="brew trust --json=v1 failed"
	json="$("$BREW" trust --json=v1 2>/dev/null)" || return 2
	TRUST_WHY="jq could not read the output of brew trust --json=v1"
	# shellcheck disable=SC2016 # $k and $v are jq variables
	"$JQ" -e --arg k "$1" --arg v "$2" '.[$k] | index($v) != null' <<<"$json" >/dev/null 2>&1 || rc=$?
	case "$rc" in 0 | 1) return "$rc" ;; *) return 2 ;; esac
}

# trust_has <taps|formulae> <entry>: 0 listed, 1 not listed. When brew cannot
# answer, die: "not listed" and "could not ask" must not look alike.
trust_has() {
	local rc=0
	trust_query "$@" || rc=$?
	if ((rc == 2)); then die "could not read brew's trust list: $TRUST_WHY"; fi
	return "$rc"
}

preconditions() {
	[[ -d "$TAP/Formula" ]] ||
		die "brewsoak's tap is missing at $TAP; run 'brewsoak upgrade ast-grep' (Task 13.1) first."
	trust_has taps "$SOAK_TAP" ||
		die "$SOAK_TAP is not trusted; run 'brew trust $SOAK_TAP' (Task 13.2) first."
	if [[ -e "$MARKER" || -e "$SPIKE" || -e "$TAP/Formula/$PKG.rb" ]]; then
		die "leftovers of an earlier run exist ($MARKER, $SPIKE or $TAP/Formula/$PKG.rb), or $PKG is installed through brewsoak's tap and this check needs it absent (Task 13.8); for leftovers run: bash $SELF --cleanup"
	fi
	if trust_has taps "$SPIKE_TAP" || trust_has formulae "$SOAK_TAP/$PKG"; then
		die "brew already trusts $SPIKE_TAP or $SOAK_TAP/$PKG (the second is also there when $PKG is installed through brewsoak's tap, which this check needs absent, Task 13.8); this check would untrust it at the end. Remove the entry yourself ('brew untrust --tap $SPIKE_TAP' or 'brew untrust --formula $SOAK_TAP/$PKG') if it is a leftover, then run again."
	fi
}

hello_installed() {
	local list # captured first: grep -q would SIGPIPE brew and pipefail would report 141
	list="$("$BREW" list --formula)" || return 1
	grep -qx "$PKG" <<<"$list"
}

CREATED_TAP=0 TRUSTED_SPIKE=0 INSTALLED=0 WROTE_FILE=0 TRUSTED_PKG=0
HAVE_LOCK=0 WORK="" LOG="" DID_RUN=0

# begin <step>: record in the marker, then set the flag, before the step runs,
# so a --cleanup after any interruption undoes exactly what this script began.
begin() {
	echo "$1" >>"$MARKER"
	case "$1" in
	tap) CREATED_TAP=1 ;;
	spike-trust) TRUSTED_SPIKE=1 ;;
	installed) INSTALLED=1 ;;
	file) WROTE_FILE=1 ;;
	pkg-trust) TRUSTED_PKG=1 ;;
	*) die "internal error: unknown step $1" ;;
	esac
}

load_marker() {
	local k
	[[ -f "$MARKER" ]] || return 0
	while IFS= read -r k; do
		case "$k" in
		tap) CREATED_TAP=1 ;;
		spike-trust) TRUSTED_SPIKE=1 ;;
		installed) INSTALLED=1 ;;
		file) WROTE_FILE=1 ;;
		pkg-trust) TRUSTED_PKG=1 ;;
		esac
	done <"$MARKER"
}

# What this check can leave behind, one item per line (empty when clean).
leftovers() {
	if hello_installed; then echo "formula $PKG is installed (brew uninstall $PKG)"; fi
	if [[ -e "$TAP/Formula/$PKG.rb" ]]; then echo "file $TAP/Formula/$PKG.rb"; fi
	if [[ -e "$SPIKE" ]]; then echo "tap directory $SPIKE (brew untap $SPIKE_TAP)"; fi
	if trust_has taps "$SPIKE_TAP"; then echo "trust entry for tap $SPIKE_TAP (brew untrust --tap $SPIKE_TAP)"; fi
	if trust_has formulae "$SOAK_TAP/$PKG"; then echo "trust entry for formula $SOAK_TAP/$PKG (brew untrust --formula $SOAK_TAP/$PKG)"; fi
	return 0
}

cleanup_steps() {
	local ok=0
	step_c() {
		if ! "$@" >>"$LOG" 2>&1; then
			echo "cleanup warning: '$*' failed (see $LOG)" >&2
			ok=1
		fi
	}
	if ((INSTALLED)) && hello_installed; then step_c "$BREW" uninstall "$PKG"; fi
	if ((WROTE_FILE)) && [[ -e "$TAP/Formula/$PKG.rb" ]]; then step_c rm -f "$TAP/Formula/$PKG.rb"; fi
	if ((CREATED_TAP)) && [[ -e "$SPIKE" ]]; then step_c "$BREW" untap "$SPIKE_TAP"; fi
	# Untrust only an entry that is listed. When brew cannot list the trust
	# store, run the untrust anyway: cleanup is never skipped on a doubt.
	local q=0
	if ((TRUSTED_SPIKE)); then
		q=0
		trust_query taps "$SPIKE_TAP" || q=$?
		if ((q != 1)); then step_c "$BREW" untrust --tap "$SPIKE_TAP"; fi
	fi
	if ((TRUSTED_PKG)); then
		q=0
		trust_query formulae "$SOAK_TAP/$PKG" || q=$?
		if ((q != 1)); then step_c "$BREW" untrust --formula "$SOAK_TAP/$PKG"; fi
	fi
	return "$ok"
}

release_lock() {
	if ((HAVE_LOCK)); then
		rm -r -- "$LOCK" 2>/dev/null
		HAVE_LOCK=0
	fi
	return 0
}

finish() {
	local rc=$?
	trap - EXIT INT TERM HUP
	set +e
	if ((DID_RUN)); then
		cleanup_steps
		local crc=$?
		snapshot >"$WORK/after" 2>>"$LOG"
		if diff "$WORK/before" "$WORK/after" >"$WORK/diff"; then
			echo "state: unchanged (formulae, casks, taps, trust store, receipts)"
		else
			echo "state: CHANGED, the check left residue:"
			cat "$WORK/diff"
			rc=1
		fi
		((crc == 0)) || rc=1
		local left
		left="$(leftovers)" || { rc=1; left="could not check for leftovers (see the message above)"; }
		if [[ -n "$left" ]]; then
			rc=1
			echo "leftovers:" >&2
			indent "$left" >&2
			echo "retry the cleanup with: bash $SELF --cleanup" >&2
		fi
		if ((rc == 0)); then
			echo "release-check-17: PASS"
			rm -r -- "$WORK"
			rm -f -- "$MARKER"
		else
			echo "release-check-17: FAIL; log and snapshots in $WORK; marker $MARKER kept for --cleanup" >&2
		fi
	fi
	release_lock
	exit "$rc"
}

fail() {
	echo "FAIL: $*" >&2
	[[ -s "$LOG" ]] && tail -n 15 "$LOG" >&2
	exit 1
}

run_step() { # <description> <command...>
	local what="$1"
	shift
	echo "step: $what"
	"$@" >>"$LOG" 2>&1 || fail "$what ('$*' exited non-zero)"
}

take_lock() {
	mkdir "$LOCK" 2>/dev/null ||
		die "another run holds $LOCK; if none is running, remove it with: rm -r $LOCK"
	HAVE_LOCK=1
}

setup_work() {
	WORK="$(mktemp -d "${TMPDIR:-/tmp}/release-check-17.XXXXXX")"
	LOG="$WORK/brew.log"
	: >"$LOG"
}

add_revision() { # <src> <dst>: insert "revision 1" after the first top-level sha256 line
	awk '{print} /^  sha256 / && !done {print "  revision 1"; done=1}' "$1" >"$2"
	grep -qx '  revision 1' "$2"
}

do_run() {
	init_tools
	preconditions
	if hello_installed; then
		echo "SKIP: $PKG is installed; this check needs it absent (Task 13.8). Nothing was changed."
		return 0
	fi
	take_lock
	setup_work
	trap finish EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM
	trap 'exit 129' HUP
	snapshot >"$WORK/before"
	: >"$MARKER"
	DID_RUN=1
	echo "snapshot: $(wc -l <"$WORK/before" | tr -d ' ') lines saved in $WORK/before"

	begin tap
	run_step "create the throwaway tap directory" mkdir -p "$SPIKE/Formula"
	# shellcheck disable=SC2016 # expanded by the inner bash
	run_step "copy $PKG into it" bash -c '"$1" cat "$2" >"$3"' _ "$BREW" "$PKG" "$SPIKE/Formula/$PKG.rb"
	[[ -s "$SPIKE/Formula/$PKG.rb" ]] || fail "brew cat $PKG printed nothing"
	begin spike-trust
	run_step "trust the throwaway tap" "$BREW" trust "$SPIKE_TAP"
	begin installed
	run_step "install $PKG from the throwaway tap" "$BREW" install --formula "$SPIKE_TAP/$PKG"
	begin file
	run_step "write $PKG with 'revision 1' into brewsoak's tap" add_revision "$SPIKE/Formula/$PKG.rb" "$TAP/Formula/$PKG.rb"
	begin pkg-trust
	run_step "upgrade $PKG through brewsoak's tap" "$BREW" upgrade --formula "$SOAK_TAP/$PKG"

	local receipt tap
	# Captured whole, then cut at the first newline: a pipe into `head` can die
	# of SIGPIPE under pipefail.
	receipt="$(find "$CELLAR/$PKG" -maxdepth 2 -path '*_1/INSTALL_RECEIPT.json')"
	receipt="${receipt%%$'\n'*}"
	[[ -n "$receipt" ]] || fail "no $PKG keg with a _1 revision exists after the upgrade"
	tap="$("$JQ" -r .source.tap "$receipt")"
	[[ "$tap" == "$SOAK_TAP" ]] || fail "the _1 receipt names '$tap', expected '$SOAK_TAP'"
	echo "check: $PKG upgraded to the _1 revision; receipt names $SOAK_TAP"
}

do_cleanup() {
	init_tools
	take_lock
	setup_work
	load_marker
	local left
	if [[ ! -f "$MARKER" ]]; then
		left="$(leftovers)" || { release_lock; rm -r -- "$WORK"; exit 2; }
		if [[ -z "$left" ]]; then
			echo "cleanup: nothing to do (no marker, no leftovers)"
			release_lock
			rm -r -- "$WORK"
			return 0
		fi
		echo "cleanup: no marker records what a run created, so nothing is removed automatically. Possible leftovers:" >&2
		indent "$left" >&2
		echo "Remove them by hand only if you are sure this check created them." >&2
		release_lock
		rm -r -- "$WORK"
		exit 1
	fi
	cleanup_steps || true
	left="$(leftovers)" || { release_lock; exit 2; }
	if [[ -n "$left" ]]; then
		echo "cleanup: leftovers remain (log: $LOG):" >&2
		indent "$left" >&2
		echo "retry with: bash $SELF --cleanup" >&2
		release_lock
		exit 1
	fi
	echo "cleanup: done"
	release_lock
	rm -f -- "$MARKER"
	rm -r -- "$WORK"
}

# ---------------------------------------------------------------- self-test

write_fake_brew() { # <dir>
	cat >"$1/brew" <<'STUB'
#!/usr/bin/env bash
# Fake brew for release-check-17.sh --self-test. State lives under $FAKE_ROOT.
set -euo pipefail
R="$FAKE_ROOT"
[[ "${HOMEBREW_NO_AUTO_UPDATE:-}" == 1 && "${HOMEBREW_NO_INSTALL_CLEANUP:-}" == 1 && "${HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK:-}" == 1 ]] || { echo "fake brew: HOMEBREW_NO_* not set" >&2; exit 97; }
CELLAR="$R/cellar"; CASKROOM="$R/caskroom"; TAPS="$R/repo/Library/Taps"
TRUST="$R/trust.json"
echo "$*" >>"$R/calls"
[[ "${FAKE_FAIL:-}" == "${1:-}" ]] && { echo "Error: fake $1 failure" >&2; exit 1; }
receipt() { mkdir -p "$CELLAR/$1/$2"; printf '{"source":{"tap":"%s"}}\n' "$3" >"$CELLAR/$1/$2/INSTALL_RECEIPT.json"; }
untrust() { jq --arg k "$1" --arg v "$2" '.[$k] -= [$v]' "$TRUST" >"$TRUST.new" && mv "$TRUST.new" "$TRUST"; }
case "$1" in
--repository) echo "$R/repo" ;;
--cellar) echo "$CELLAR" ;;
--caskroom) echo "$CASKROOM" ;;
tap) for d in "$TAPS"/*/homebrew-*; do [[ -d "$d" ]] && { u="${d%/*}"; echo "${u##*/}/${d##*/homebrew-}"; }; done; true ;;
list) if [[ -n "${FAKE_BIG:-}" && "$2" == --formula ]]; then echo hello; seq 1 20000; elif [[ "$2" == --formula ]]; then ls "$CELLAR"; else ls "$CASKROOM"; fi ;;
cat) printf 'class Hello < Formula\n  url "https://x/hello.tgz"\n  sha256 "abc"\n  bottle do\n    sha256 cellar: :any, arm64: "def"\n  end\nend\n' ;;
trust) if [[ "$2" == --json=v1 ]]; then [[ ! -e "$R/trust-json-broken" ]] || { echo "Error: fake trust --json failure" >&2; exit 1; }; cat "$TRUST"; else jq --arg v "$2" '.taps += [$v]' "$TRUST" >"$TRUST.new" && mv "$TRUST.new" "$TRUST"; fi ;;
untrust) if [[ "$2" == --tap ]]; then untrust taps "$3"; else untrust formulae "$3"; fi ;;
install) receipt hello 2.12 spike17c/t3 ;;
upgrade)
	[[ -f "$TAPS/ericfitz/homebrew-brewsoak/Formula/hello.rb" ]] || { echo "Error: no such formula" >&2; exit 1; }
	if [[ "${FAKE_HANG:-}" == upgrade ]]; then : >"$R/hang"; sleep 1; fi
	[[ -z "${FAKE_BREAK_TRUST_JSON:-}" ]] || : >"$R/trust-json-broken"
	rm -rf "$CELLAR/hello"; receipt hello 2.12_1 "${FAKE_RECEIPT_TAP:-ericfitz/brewsoak}"
	jq '.formulae += ["ericfitz/brewsoak/hello"]' "$TRUST" >"$TRUST.new" && mv "$TRUST.new" "$TRUST" ;;
uninstall) rm -rf "$CELLAR/$2" ;;
untap) rm -rf "$TAPS/${2%%/*}/homebrew-${2##*/}" ;;
*) echo "fake brew: unsupported: $*" >&2; exit 64 ;;
esac
STUB
	chmod +x "$1/brew"
}

reset_fake() { # <root> <home>
	rm -rf "${1:?}" "${2:?}"
	mkdir -p "$1/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula" "$1/repo/Library/Taps/homebrew/homebrew-core" \
		"$1/cellar/wget/1.0" "$1/caskroom/firefox/.metadata" "$2" "$1/tmp"
	printf '{"source":{"tap":"homebrew/core"}}\n' >"$1/cellar/wget/1.0/INSTALL_RECEIPT.json"
	printf '{"source":{"tap":"homebrew/cask"}}\n' >"$1/caskroom/firefox/.metadata/INSTALL_RECEIPT.json"
	printf '{"casks":[],"commands":[],"formulae":[],"taps":["ericfitz/brewsoak"]}\n' >"$1/trust.json"
	: >"$1/calls"
}

# Recorded from a first run of snapshot() against the fake state above.
expected_snapshot() {
	cat <<'EOF'
## taps
ericfitz/brewsoak
homebrew/core
## formulae
wget
## casks
firefox
## trust
{
  "casks": [],
  "commands": [],
  "formulae": [],
  "taps": [
    "ericfitz/brewsoak"
  ]
}
## receipts
cellar/wget/1.0/INSTALL_RECEIPT.json  @@WGET@@
caskroom/firefox/.metadata/INSTALL_RECEIPT.json  @@FIREFOX@@
EOF
}

# Recorded: what `diff before after` prints for a run whose untrust fails
# (trust store keeps the throwaway tap and the hello entry).
expected_leak_diff() {
	cat <<'EOF'
12c12,14
<   "formulae": [],
---
>   "formulae": [
>     "ericfitz/brewsoak/hello"
>   ],
14c16,17
<     "ericfitz/brewsoak"
---
>     "ericfitz/brewsoak",
>     "spike17c/t3"
EOF
}

self_test() {
	local total=0 failed=0 names=()
	T="$(mktemp -d "${TMPDIR:-/tmp}/release-check-17-selftest.XXXXXX")"
	trap 'rm -r -- "$T"' EXIT
	mkdir -p "$T/bin" "$T/tools"
	write_fake_brew "$T/bin"
	local JQBIN SHABIN
	JQBIN="$(command -v jq || true)"
	[[ "$JQBIN" == /* ]] || die "jq not found on PATH; run: brew install jq"
	SHABIN="$(command -v shasum || true)"
	[[ "$SHABIN" == /* ]] || die "shasum not found on PATH."
	local t p
	for t in seq bash jq shasum dirname basename mktemp rm mkdir cat sed awk sort find diff grep cut head tail wc tr ls mv sleep env; do
		p="$(command -v "$t" || true)"
		if [[ "$p" == /* ]]; then ln -s "$p" "$T/tools/$t"; fi
	done

	local ROOT="$T/fake" HOME_="$T/home" out rc
	local STATE="$T/fake/state/brewsoak"
	local wget_sum firefox_sum
	wget_sum="$(printf '{"source":{"tap":"homebrew/core"}}\n' | "$SHABIN" -a 256 | cut -d' ' -f1)"
	firefox_sum="$(printf '{"source":{"tap":"homebrew/cask"}}\n' | "$SHABIN" -a 256 | cut -d' ' -f1)"
	expected_snapshot | sed "s/@@WGET@@/$wget_sum/; s/@@FIREFOX@@/$firefox_sum/" >"$T/expected.snap"

	# in_env <PATH> [VAR=value...]: environment for one run against the fake.
	in_env() {
		local path="$1"
		shift
		env -i PATH="$path" HOME="$HOME_" XDG_STATE_HOME="$ROOT/state" FAKE_ROOT="$ROOT" TMPDIR="$ROOT/tmp" "$@"
	}
	# run_case <PATH> [VAR=value...] -- <args>: sets out and rc.
	run_case() {
		local path="$1"
		shift
		rc=0
		out="$(in_env "$path" "$@" 2>&1)" || rc=$?
	}
	check() { # <case> <0 ok | non-zero failure> <detail>
		total=$((total + 1))
		if [[ "$2" != 0 ]]; then
			failed=$((failed + 1))
			names+=("$1")
			echo "  FAIL $1: $3" >>"$T/failures.txt"
		fi
	}
	want_rc() { # <case> <wanted>
		check "$1" "$([[ $rc == "$2" ]] && echo 0 || echo 1)" "exit $rc, want $2: $out"
	}
	has() { check "$1" "$(grep -qF -- "$2" <<<"$out" && echo 0 || echo 1)" "missing '$2' in: $out"; }
	untouched() { # <case>: the fake state equals the recorded snapshot
		in_env "$T/bin:$T/tools" bash "$SELF" --snapshot >"$T/now.snap" 2>&1 || true
		check "$1" "$(diff "$T/expected.snap" "$T/now.snap" >/dev/null && echo 0 || echo 1)" "state differs from the recorded snapshot"
	}
	P="$T/bin:$T/tools"
	: >"$T/failures.txt"

	# 0. the snapshot of the fake state matches the recorded one
	reset_fake "$ROOT" "$HOME_"
	untouched snapshot-matches-recorded

	# 1. happy path: pass, state restored, every cleanup command issued
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" bash "$SELF" --run
	want_rc happy-exit 0
	has happy-pass "release-check-17: PASS"
	has happy-state "state: unchanged"
	untouched happy-restored
	for c in "uninstall hello" "untap spike17c/t3" "untrust --tap spike17c/t3" "untrust --formula ericfitz/brewsoak/hello"; do
		check "happy-cleanup-$c" "$(grep -qxF "$c" "$ROOT/calls" && echo 0 || echo 1)" "fake brew never saw '$c'"
	done
	check happy-lock-released "$([[ ! -e "$STATE/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left behind"

	# 2. second run behaves like the first
	run_case "$P" bash "$SELF" --run
	want_rc second-run 0

	# 3. the upgrade fails: nonzero, cleanup still ran, state restored
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_FAIL=upgrade bash "$SELF" --run
	want_rc upgrade-fails-exit 1
	has upgrade-fails-msg "FAIL: upgrade hello through brewsoak's tap"
	untouched upgrade-fails-restored

	# 4. wrong receipt tap: fails with the receipt message, state restored
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_RECEIPT_TAP=homebrew/core bash "$SELF" --run
	want_rc wrong-receipt-exit 1
	has wrong-receipt-msg "the _1 receipt names 'homebrew/core'"
	untouched wrong-receipt-restored

	# 5. residue is detected: untrust fails, the diff is reported and recorded
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_FAIL=untrust bash "$SELF" --run
	want_rc residue-exit 1
	has residue-msg "state: CHANGED"
	printf '%s\n' "$out" | sed -n '/^state: CHANGED/,/^leftovers:/p' | sed '1d;$d' >"$T/leak.got"
	has residue-leftovers "trust entry for tap spike17c/t3 (brew untrust --tap spike17c/t3)"
	has residue-retry "retry the cleanup with: bash $SELF --cleanup"
	expected_leak_diff >"$T/leak.want"
	check residue-diff "$(diff "$T/leak.want" "$T/leak.got" >"$T/leak.diff" && echo 0 || echo 1)" "$(cat "$T/leak.diff")"

	# 6. hello already installed: skipped, nothing touched
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/cellar/hello/2.12"
	run_case "$P" bash "$SELF" --run
	want_rc skip-exit 0
	has skip-msg "SKIP: hello is installed"
	check skip-no-mutation "$(grep -qE '^(trust [a-z]|install|upgrade|uninstall|untap|untrust)' "$ROOT/calls" && echo 1 || echo 0)" "$(cat "$ROOT/calls")"

	# 7. a run whose cleanup fails keeps its marker, lists the leftovers and the
	# retry command; --run then refuses; --cleanup finishes the job (twice is safe)
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_FAIL=uninstall bash "$SELF" --run
	want_rc cleanup-fails-exit 1
	has cleanup-fails-leftover "formula hello is installed (brew uninstall hello)"
	has cleanup-fails-retry "retry the cleanup with: bash $SELF --cleanup"
	check cleanup-fails-marker "$([[ -s "$STATE/release-check-17.marker" ]] && echo 0 || echo 1)" "marker missing"
	check marker-not-in-tmpdir "$([[ ! -e "$ROOT/tmp/release-check-17.marker" ]] && echo 0 || echo 1)" "marker written under TMPDIR"
	run_case "$P" bash "$SELF" --run
	want_rc leftovers-exit 2
	has leftovers-msg "--cleanup"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-exit 0
	untouched cleanup-restores
	check cleanup-marker-removed "$([[ ! -e "$STATE/release-check-17.marker" ]] && echo 0 || echo 1)" "marker remains"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-idempotent 0
	has cleanup-idempotent-msg "nothing to do"

	# 7b. --cleanup with no marker removes nothing, even a user-installed hello
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/cellar/hello/2.12"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-nomarker-exit 1
	has cleanup-nomarker-msg "no marker"
	check cleanup-nomarker-no-workdir "$(compgen -G "$ROOT/tmp/release-check-17.??????" >/dev/null && echo 1 || echo 0)" "work directory left: $(ls "$ROOT/tmp")"
	check cleanup-nomarker-kept "$([[ -d "$ROOT/cellar/hello/2.12" ]] && ! grep -qE '^(uninstall|untap|untrust)' "$ROOT/calls" && echo 0 || echo 1)" "removed something: $(cat "$ROOT/calls")"
	# ... and a --run's marker alone controls what --cleanup removes: a marker
	# that records only the file leaves a user's hello and trust entries alone
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/cellar/hello/2.12"
	: >"$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/hello.rb"
	mkdir -p "$STATE"
	echo file >"$STATE/release-check-17.marker"
	run_case "$P" bash "$SELF" --cleanup
	check cleanup-marker-scope "$([[ -d "$ROOT/cellar/hello/2.12" && ! -e "$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/hello.rb" ]] && ! grep -qE '^(uninstall|untap|untrust)' "$ROOT/calls" && echo 0 || echo 1)" "calls: $(cat "$ROOT/calls")"

	# 7c. guarded untrust: an entry that is not listed is not untrusted; when
	# brew cannot list the trust store the untrust runs anyway
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$STATE"
	printf 'spike-trust\npkg-trust\n' >"$STATE/release-check-17.marker"
	run_case "$P" bash "$SELF" --cleanup
	want_rc untrust-absent-exit 0
	check untrust-absent-skipped "$(grep -q '^untrust' "$ROOT/calls" && echo 1 || echo 0)" "untrust ran for an absent entry: $(cat "$ROOT/calls")"
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$STATE"
	printf 'spike-trust\npkg-trust\n' >"$STATE/release-check-17.marker"
	echo '{"casks":[],"commands":[],"formulae":["ericfitz/brewsoak/hello"],"taps":["ericfitz/brewsoak","spike17c/t3"]}' >"$ROOT/trust.json"
	run_case "$P" bash "$SELF" --cleanup
	want_rc untrust-present-exit 0
	for c in "untrust --tap spike17c/t3" "untrust --formula ericfitz/brewsoak/hello"; do
		check "untrust-present-$c" "$(grep -qxF "$c" "$ROOT/calls" && echo 0 || echo 1)" "fake brew never saw '$c'"
	done
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_BREAK_TRUST_JSON=1 bash "$SELF" --run
	for c in "untrust --tap spike17c/t3" "untrust --formula ericfitz/brewsoak/hello"; do
		check "untrust-fallback-$c" "$(grep -qxF "$c" "$ROOT/calls" && echo 0 || echo 1)" "fake brew never saw '$c' when trust --json failed"
	done

	# 7d. hello installed through brewsoak's tap: --run exits 2 and says so
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/cellar/hello/2.12_1"
	: >"$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/hello.rb"
	run_case "$P" bash "$SELF" --run
	want_rc anchored-hello-exit 2
	has anchored-hello-message "or hello is installed through brewsoak's tap and this check needs it absent (Task 13.8)"

	# 8. preconditions: tap missing, tap not trusted (exact match), pre-existing trust entries
	reset_fake "$ROOT" "$HOME_"
	rm -r -- "$ROOT/repo/Library/Taps/ericfitz"
	run_case "$P" bash "$SELF" --run
	want_rc no-tap-exit 2
	has no-tap-msg "Task 13.1"
	reset_fake "$ROOT" "$HOME_"
	echo '{"casks":[],"commands":[],"formulae":[],"taps":[]}' >"$ROOT/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc untrusted-exit 2
	has untrusted-msg "is not trusted; run 'brew trust ericfitz/brewsoak'"
	reset_fake "$ROOT" "$HOME_"
	echo '{"casks":[],"commands":[],"formulae":["ericfitz/brewsoak/hello"],"taps":["ericfitz/brewsoak-other"]}' >"$ROOT/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc substring-trust-exit 2
	has substring-trust-msg "is not trusted"
	reset_fake "$ROOT" "$HOME_"
	echo '{"casks":[],"commands":[],"formulae":[],"taps":["ericfitz/brewsoak","spike17c/t3"]}' >"$ROOT/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc preexisting-spike-trust-exit 2
	has preexisting-spike-trust-msg "already trusts"
	reset_fake "$ROOT" "$HOME_"
	echo '{"casks":[],"commands":[],"formulae":["ericfitz/brewsoak/hello"],"taps":["ericfitz/brewsoak"]}' >"$ROOT/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc preexisting-pkg-trust-exit 2
	has preexisting-pkg-trust-msg "already trusts"
	has preexisting-pkg-trust-reworded "(the second is also there when $PKG is installed through brewsoak's tap, which this check needs absent, Task 13.8)"
	check preexisting-trust-untouched "$(grep -qE '^(install|upgrade|uninstall|untap|untrust)' "$ROOT/calls" && echo 1 || echo 0)" "$(cat "$ROOT/calls")"

	# 8a. brew cannot answer the trust query: not "is not trusted"
	reset_fake "$ROOT" "$HOME_"
	: >"$ROOT/trust-json-broken"
	run_case "$P" bash "$SELF" --run
	want_rc trust-unreadable-pre-exit 2
	has trust-unreadable-pre-msg "could not read brew's trust list: brew trust --json=v1 failed"
	check trust-unreadable-pre-not-untrusted "$(grep -qF 'is not trusted' <<<"$out" && echo 1 || echo 0)" "$out"
	check trust-unreadable-pre-no-lock "$([[ ! -e "$STATE/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left"
	reset_fake "$ROOT" "$HOME_"
	: >"$ROOT/trust-json-broken"
	run_case "$P" bash "$SELF" --cleanup
	want_rc trust-unreadable-left-exit 2
	has trust-unreadable-left-msg "could not read brew's trust list: brew trust --json=v1 failed"
	check trust-unreadable-left-no-lock "$([[ ! -e "$STATE/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left"

	# 8b. hello installed, with a long `brew list` that prints hello first
	# (grep -q under pipefail would SIGPIPE brew and report "not installed")
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_BIG=1 bash "$SELF" --run
	want_rc big-list-exit 0
	has big-list-skip "SKIP: hello is installed"

	# 9. missing tools
	reset_fake "$ROOT" "$HOME_"
	run_case "$T/tools" bash "$SELF" --run
	want_rc no-brew-exit 2
	has no-brew-msg "brew not found"
	mkdir -p "$T/tools-nojq"
	for p in "$T/tools"/*; do
		if [[ "${p##*/}" != jq ]]; then ln -s "$(readlink "$p")" "$T/tools-nojq/${p##*/}"; fi
	done
	ln -s "$T/bin/brew" "$T/tools-nojq/brew"
	run_case "$T/tools-nojq" bash "$SELF" --run
	want_rc no-jq-exit 2
	has no-jq-msg "brew install jq"

	# 10. a held lock refuses a second run and is not removed by it
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$STATE/release-check-17.lock"
	run_case "$P" bash "$SELF" --run
	want_rc lock-exit 2
	has lock-msg "another run holds"
	check lock-kept "$([[ -d "$STATE/release-check-17.lock" ]] && echo 0 || echo 1)" "the other run's lock was removed"
	rm -r -- "$STATE/release-check-17.lock"

	# 11. SIGTERM during the upgrade: the trap still cleans up and restores
	reset_fake "$ROOT" "$HOME_"
	in_env "$P" FAKE_HANG=upgrade bash "$SELF" --run >"$T/term.out" 2>&1 &
	local pid=$!
	for _ in $(seq 1 100); do
		[[ -e "$ROOT/hang" ]] && break
		sleep 0.1
	done
	kill -TERM "$pid" 2>/dev/null || true
	rc=0
	wait "$pid" || rc=$?
	out="$(cat "$T/term.out")"
	want_rc term-exit 143
	untouched term-restored
	check term-lock "$([[ ! -e "$STATE/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left behind"

	# 11b. SIGHUP behaves the same
	reset_fake "$ROOT" "$HOME_"
	in_env "$P" FAKE_HANG=upgrade bash "$SELF" --run >"$T/hup.out" 2>&1 &
	pid=$!
	for _ in $(seq 1 100); do
		[[ -e "$ROOT/hang" ]] && break
		sleep 0.1
	done
	kill -HUP "$pid" 2>/dev/null || true
	rc=0
	wait "$pid" || rc=$?
	out="$(cat "$T/hup.out")"
	want_rc hup-exit 129
	untouched hup-restored

	# 12. the revision insert needs a top-level sha256 line
	printf 'class X\n  bottle do\n    sha256 cellar: :any, a: "b"\n  end\nend\n' >"$T/nosha.rb"
	check revision-needs-sha "$(add_revision "$T/nosha.rb" "$T/nosha.out" && echo 1 || echo 0)" "accepted a file with no top-level sha256"

	echo "self-test: $total checks, $((total - failed)) passed, $failed failed"
	if ((failed)); then
		echo "failed: ${names[*]}"
		cat "$T/failures.txt"
		return 1
	fi
}

case "${1:-}" in
--run) do_run ;;
--cleanup) do_cleanup ;;
--snapshot) init_tools && snapshot ;; # internal: used by --self-test
--self-test) self_test ;;
*)
	echo "usage: release-check-17.sh --run | --cleanup | --self-test" >&2
	exit 2
	;;
esac
