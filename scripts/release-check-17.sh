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
# Leftovers of a failed run: --run refuses to start and names --cleanup.
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
	TRUST="$HOME/.homebrew/trust.json"
	LOCK="${TMPDIR:-/tmp}/release-check-17.lock"
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
	if [[ -f "$TRUST" ]]; then "$JQ" -S . "$TRUST"; else echo "(absent)"; fi
	echo "## receipts"
	receipts cellar "$CELLAR"
	receipts caskroom "$CASKROOM"
}

preconditions() {
	[[ -d "$TAP/Formula" ]] ||
		die "brewsoak's tap is missing at $TAP; run 'brewsoak upgrade ast-grep' (Task 13.1) first."
	[[ -f "$TRUST" ]] ||
		die "$TRUST is missing; run 'brew trust $SOAK_TAP' (Task 13.2) first."
	# shellcheck disable=SC2016 # $t is a jq variable
	"$JQ" -e --arg t "$SOAK_TAP" 'tostring | ascii_downcase | contains($t)' "$TRUST" >/dev/null ||
		die "$SOAK_TAP is not in $TRUST; run 'brew trust $SOAK_TAP' (Task 13.2) first."
	if [[ -e "$SPIKE" || -e "$TAP/Formula/$PKG.rb" ]]; then
		die "leftovers of an earlier run exist ($SPIKE or $TAP/Formula/$PKG.rb); run: bash $SELF --cleanup"
	fi
}

hello_installed() {
	"$BREW" list --formula | grep -qx "$PKG"
}

CREATED_TAP=0 TRUSTED_SPIKE=0 INSTALLED=0 WROTE_FILE=0 TRUSTED_PKG=0
HAVE_LOCK=0 WORK="" LOG="" FORCE_CLEANUP=0 DID_RUN=0

cleanup_steps() {
	local ok=0
	step_c() {
		if ! "$@" >>"$LOG" 2>&1; then
			echo "cleanup warning: '$*' failed (see $LOG)" >&2
			ok=1
		fi
	}
	if ((INSTALLED || FORCE_CLEANUP)) && hello_installed; then step_c "$BREW" uninstall "$PKG"; fi
	if ((WROTE_FILE || FORCE_CLEANUP)) && [[ -e "$TAP/Formula/$PKG.rb" ]]; then step_c rm -f "$TAP/Formula/$PKG.rb"; fi
	if ((CREATED_TAP || FORCE_CLEANUP)) && [[ -e "$SPIKE" ]]; then step_c "$BREW" untap "$SPIKE_TAP"; fi
	if ((TRUSTED_SPIKE || FORCE_CLEANUP)); then step_c "$BREW" untrust --tap "$SPIKE_TAP"; fi
	if ((TRUSTED_PKG || FORCE_CLEANUP)); then step_c "$BREW" untrust --formula "$SOAK_TAP/$PKG"; fi
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
	trap - EXIT INT TERM
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
		if ((rc == 0)); then
			echo "release-check-17: PASS"
			rm -r -- "$WORK"
		else
			echo "release-check-17: FAIL; log and snapshots in $WORK" >&2
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
	snapshot >"$WORK/before"
	DID_RUN=1
	echo "snapshot: $(wc -l <"$WORK/before" | tr -d ' ') lines saved in $WORK/before"

	CREATED_TAP=1
	run_step "create the throwaway tap directory" mkdir -p "$SPIKE/Formula"
	# shellcheck disable=SC2016 # expanded by the inner bash
	run_step "copy $PKG into it" bash -c '"$1" cat "$2" >"$3"' _ "$BREW" "$PKG" "$SPIKE/Formula/$PKG.rb"
	[[ -s "$SPIKE/Formula/$PKG.rb" ]] || fail "brew cat $PKG printed nothing"
	TRUSTED_SPIKE=1
	run_step "trust the throwaway tap" "$BREW" trust "$SPIKE_TAP"
	INSTALLED=1
	run_step "install $PKG from the throwaway tap" "$BREW" install --formula "$SPIKE_TAP/$PKG"
	WROTE_FILE=1
	run_step "write $PKG with 'revision 1' into brewsoak's tap" add_revision "$SPIKE/Formula/$PKG.rb" "$TAP/Formula/$PKG.rb"
	TRUSTED_PKG=1
	run_step "upgrade $PKG through brewsoak's tap" "$BREW" upgrade --formula "$SOAK_TAP/$PKG"

	local receipt tap
	receipt="$(find "$CELLAR/$PKG" -maxdepth 2 -path '*_1/INSTALL_RECEIPT.json' | head -n 1)"
	[[ -n "$receipt" ]] || fail "no $PKG keg with a _1 revision exists after the upgrade"
	tap="$("$JQ" -r .source.tap "$receipt")"
	[[ "$tap" == "$SOAK_TAP" ]] || fail "the _1 receipt names '$tap', expected '$SOAK_TAP'"
	echo "check: $PKG upgraded to the _1 revision; receipt names $SOAK_TAP"
}

do_cleanup() {
	init_tools
	take_lock
	setup_work
	FORCE_CLEANUP=1
	if ! cleanup_steps; then
		echo "cleanup: some steps failed; see $LOG" >&2
		release_lock
		exit 1
	fi
	echo "cleanup: done"
	release_lock
	rm -r -- "$WORK"
}

# ---------------------------------------------------------------- self-test

write_fake_brew() { # <dir>
	cat >"$1/brew" <<'STUB'
#!/usr/bin/env bash
# Fake brew for release-check-17.sh --self-test. State lives under $FAKE_ROOT.
set -euo pipefail
R="$FAKE_ROOT"
CELLAR="$R/cellar"; CASKROOM="$R/caskroom"; TAPS="$R/repo/Library/Taps"
TRUST="$HOME/.homebrew/trust.json"
echo "$*" >>"$R/calls"
[[ "${FAKE_FAIL:-}" == "${1:-}" ]] && { echo "Error: fake $1 failure" >&2; exit 1; }
receipt() { mkdir -p "$CELLAR/$1/$2"; printf '{"source":{"tap":"%s"}}\n' "$3" >"$CELLAR/$1/$2/INSTALL_RECEIPT.json"; }
untrust() { jq --arg k "$1" --arg v "$2" '.[$k] -= [$v]' "$TRUST" >"$TRUST.new" && mv "$TRUST.new" "$TRUST"; }
case "$1" in
--repository) echo "$R/repo" ;;
--cellar) echo "$CELLAR" ;;
--caskroom) echo "$CASKROOM" ;;
tap) for d in "$TAPS"/*/homebrew-*; do [[ -d "$d" ]] && { u="${d%/*}"; echo "${u##*/}/${d##*/homebrew-}"; }; done; true ;;
list) if [[ "$2" == --formula ]]; then ls "$CELLAR"; else ls "$CASKROOM"; fi ;;
cat) printf 'class Hello < Formula\n  url "https://x/hello.tgz"\n  sha256 "abc"\n  bottle do\n    sha256 cellar: :any, arm64: "def"\n  end\nend\n' ;;
trust) jq --arg v "$2" '.taps += [$v]' "$TRUST" >"$TRUST.new" && mv "$TRUST.new" "$TRUST" ;;
untrust) if [[ "$2" == --tap ]]; then untrust taps "$3"; else untrust formulae "$3"; fi ;;
install) receipt hello 2.12 spike17c/t3 ;;
upgrade)
	[[ -f "$TAPS/ericfitz/homebrew-brewsoak/Formula/hello.rb" ]] || { echo "Error: no such formula" >&2; exit 1; }
	if [[ "${FAKE_HANG:-}" == upgrade ]]; then : >"$R/hang"; sleep 1; fi
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
		"$1/cellar/wget/1.0" "$1/caskroom/firefox/.metadata" "$2/.homebrew" "$1/tmp"
	printf '{"source":{"tap":"homebrew/core"}}\n' >"$1/cellar/wget/1.0/INSTALL_RECEIPT.json"
	printf '{"source":{"tap":"homebrew/cask"}}\n' >"$1/caskroom/firefox/.metadata/INSTALL_RECEIPT.json"
	printf '{"taps":["ericfitz/brewsoak"],"formulae":[]}\n' >"$2/.homebrew/trust.json"
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
10c10,12
<   "formulae": [],
---
>   "formulae": [
>     "ericfitz/brewsoak/hello"
>   ],
12c14,15
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
	for t in bash jq shasum dirname basename mktemp rm mkdir cat sed awk sort find diff grep cut head tail wc tr ls mv sleep env; do
		p="$(command -v "$t" || true)"
		if [[ "$p" == /* ]]; then ln -s "$p" "$T/tools/$t"; fi
	done

	local ROOT="$T/fake" HOME_="$T/home" out rc
	local wget_sum firefox_sum
	wget_sum="$(printf '{"source":{"tap":"homebrew/core"}}\n' | "$SHABIN" -a 256 | cut -d' ' -f1)"
	firefox_sum="$(printf '{"source":{"tap":"homebrew/cask"}}\n' | "$SHABIN" -a 256 | cut -d' ' -f1)"
	expected_snapshot | sed "s/@@WGET@@/$wget_sum/; s/@@FIREFOX@@/$firefox_sum/" >"$T/expected.snap"

	# in_env <PATH> [VAR=value...]: environment for one run against the fake.
	in_env() {
		local path="$1"
		shift
		env -i PATH="$path" HOME="$HOME_" FAKE_ROOT="$ROOT" TMPDIR="$ROOT/tmp" "$@"
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
	check happy-lock-released "$([[ ! -e "$ROOT/tmp/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left behind"

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
	printf '%s\n' "$out" | sed -n '/^state: CHANGED/,/^release-check-17: FAIL/p' | sed '1d' | sed -n '/^[0-9]/,/^release-check-17: FAIL/p' | sed '$d' >"$T/leak.got"
	expected_leak_diff >"$T/leak.want"
	check residue-diff "$(diff "$T/leak.want" "$T/leak.got" >"$T/leak.diff" && echo 0 || echo 1)" "$(cat "$T/leak.diff")"

	# 6. hello already installed: skipped, nothing touched
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/cellar/hello/2.12"
	run_case "$P" bash "$SELF" --run
	want_rc skip-exit 0
	has skip-msg "SKIP: hello is installed"
	check skip-no-mutation "$(grep -qE '^(trust|install|upgrade|uninstall|untap|untrust)' "$ROOT/calls" && echo 1 || echo 0)" "$(cat "$ROOT/calls")"

	# 7. leftovers of an earlier run: refuses, names --cleanup; --cleanup clears them
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$ROOT/repo/Library/Taps/spike17c/homebrew-t3/Formula"
	: >"$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/hello.rb"
	run_case "$P" bash "$SELF" --run
	want_rc leftovers-exit 2
	has leftovers-msg "--cleanup"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-exit 0
	check cleanup-removes "$([[ ! -e "$ROOT/repo/Library/Taps/spike17c/homebrew-t3" && ! -e "$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/hello.rb" ]] && echo 0 || echo 1)" "leftovers remain: $out"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-idempotent 0

	# 8. preconditions: tap missing, trust missing, tap not trusted
	reset_fake "$ROOT" "$HOME_"
	rm -r -- "$ROOT/repo/Library/Taps/ericfitz"
	run_case "$P" bash "$SELF" --run
	want_rc no-tap-exit 2
	has no-tap-msg "Task 13.1"
	reset_fake "$ROOT" "$HOME_"
	rm -- "$HOME_/.homebrew/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc no-trust-exit 2
	has no-trust-msg "brew trust ericfitz/brewsoak"
	reset_fake "$ROOT" "$HOME_"
	echo '{"taps":[],"formulae":[]}' >"$HOME_/.homebrew/trust.json"
	run_case "$P" bash "$SELF" --run
	want_rc untrusted-exit 2
	has untrusted-msg "is not in"

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
	mkdir "$ROOT/tmp/release-check-17.lock"
	run_case "$P" bash "$SELF" --run
	want_rc lock-exit 2
	has lock-msg "another run holds"
	check lock-kept "$([[ -d "$ROOT/tmp/release-check-17.lock" ]] && echo 0 || echo 1)" "the other run's lock was removed"
	rm -r -- "$ROOT/tmp/release-check-17.lock"

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
	check term-lock "$([[ ! -e "$ROOT/tmp/release-check-17.lock" ]] && echo 0 || echo 1)" "lock left behind"

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
