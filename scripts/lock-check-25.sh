#!/usr/bin/env bash
# Manual check 4 for issue #25 (see the plan, Task 9): brewsoak's prune leaves
# a tap file alone while brew itself holds the formula lock, and removes it
# once brew lets go. It runs REAL brew and brewsoak against the real
# brewsoak tap and changes real state for the length of the check: one
# throwaway file, Formula/zzz-brewsoak-lock-check.rb, in brewsoak's tap and one
# `brew ruby` process holding brew's lock for that name. Usage:
#   lock-check-25.sh --run [BREWSOAK]   run the check (BREWSOAK: absolute path
#                                       of the binary, default
#                                       <repo>/target/release/brewsoak)
#   lock-check-25.sh --cleanup          remove leftovers of an interrupted run
#   lock-check-25.sh --self-test        test this script against a fake brew
#                                       and a fake brewsoak (changes nothing)
#
# Exit codes: 0 pass, 1 the check or the cleanup failed (including: the first
# run pruned the file while the lock was held), 2 a precondition is missing,
# 3 the check is void (the hold may have ended before the first run finished:
# run it again).
#
# Steps of --run: warm brewsoak's snapshots with one `update`; write the
# throwaway file; start the holder and wait until it prints `held` (it holds
# the lock for 120 s from then); run `brewsoak update -v` and expect
# `not pruning ...: brew holds the lock for zzz-brewsoak-lock-check` and the file
# still there; wait for the holder to finish; run it again and expect
# `pruned ...` and the file gone. The first run must end less than 100 s after
# the holder reported `held`, or the check is void.
#
# Second run: a passing run leaves nothing but brew's own empty lock file, so
# the next run behaves like the first. Overlapping runs: a lock directory
# refuses the second one. Leftovers of a failed run: --run refuses to start and
# names --cleanup, which acts only on what the marker file of the failed run
# recorded and kills a recorded pid only while its command line still names the
# lock. The marker and the lock live in
# ${XDG_STATE_HOME:-$HOME/.local/state}/brewsoak. An EXIT, INT, TERM or HUP
# trap kills the holder and removes the throwaway file. A SIGKILL or a power
# loss leaves the marker (and the lock directory, removed with the command the
# die message names); the holder ends by itself within 120 s.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SOAK_TAP="ericfitz/brewsoak"
NAME="zzz-brewsoak-lock-check"
# Seconds the holder keeps the lock. The first run must end 20 s before that.
HOLD="${LOCK_CHECK_HOLD:-120}"

die() {
	echo "lock-check-25: $*" >&2
	exit 2
}

[[ "$HOLD" =~ ^[0-9]+$ ]] && [[ "$HOLD" -ge 20 ]] ||
	die "LOCK_CHECK_HOLD must be a whole number of seconds, 20 or more (got '$HOLD')."
MAX_GAP=$((HOLD - 20))

# Resolve tools to absolute paths, failing fast with what to install.
init_tools() {
	export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1
	BREW="$(command -v brew || true)"
	[[ "$BREW" == /* ]] || die "brew not found on PATH; install Homebrew first."
	JQ="$(command -v jq || true)"
	[[ "$JQ" == /* ]] || die "jq not found on PATH; run: brew install jq"
	PERL="$(command -v perl || true)"
	[[ "$PERL" == /* ]] || die "perl not found on PATH."
	local repo
	repo="$("$BREW" --repository)" || die "'brew --repository' failed."
	[[ "$repo" == /* ]] || die "brew did not print an absolute --repository path."
	TAP="$repo/Library/Taps/ericfitz/homebrew-brewsoak"
	THROWAWAY="$TAP/Formula/$NAME.rb"
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
	MARKER="$state/lock-check-25.marker"
	LOCK="$state/lock-check-25.lock"
}

# resolve_bin <path-or-empty>: the brewsoak binary, absolute and executable.
resolve_bin() {
	BIN="${1:-$REPO_ROOT/target/release/brewsoak}"
	[[ "$BIN" == /* ]] || die "the brewsoak binary must be an absolute path (got '$BIN')."
	[[ -f "$BIN" && -x "$BIN" ]] ||
		die "no executable brewsoak at $BIN; run 'cargo build --release' in $REPO_ROOT or pass the path."
}

# tap_state: ready | missing | remote | untrusted. Mirrors brewsoak's own
# rule: tapped, local (remote null) and trusted (a missing field counts as
# trusted).
tap_state() {
	local json
	json="$("$BREW" tap-info --json --installed)" || die "'brew tap-info --json --installed' failed."
	# shellcheck disable=SC2016 # $t is a jq variable
	"$JQ" -r --arg t "$SOAK_TAP" '
		[.[] | select(.name == $t)] |
		if length == 0 then "missing"
		elif .[0].remote != null then "remote"
		elif .[0].trusted == false then "untrusted"
		else "ready" end' <<<"$json" || die "jq could not read the output of 'brew tap-info --json --installed'."
}

# alive <pid>: a live process; a zombie counts as gone.
alive() {
	local st
	[[ "$1" =~ ^[0-9]+$ ]] || return 1
	st="$(ps -o stat= -p "$1" 2>/dev/null || true)"
	st="${st//[[:space:]]/}"
	[[ -n "$st" && "$st" != Z* ]]
}

# is_holder <pid>: alive and its command line names the lock.
is_holder() {
	local cmd
	alive "$1" || return 1
	cmd="$(ps -o command= -p "$1" 2>/dev/null || true)"
	[[ "$cmd" == *"$NAME"* ]]
}

preconditions() {
	case "$(tap_state)" in
	ready) ;;
	missing) die "brewsoak's tap $SOAK_TAP is not tapped; run 'brewsoak upgrade ast-grep' (or any soaked upgrade) once to create it." ;;
	remote) die "$SOAK_TAP has a remote, so brewsoak will not use it; the check needs the local tap." ;;
	untrusted) die "$SOAK_TAP is not trusted; run 'brew trust $SOAK_TAP' yourself, then run again." ;;
	*) die "internal error: unknown tap state." ;;
	esac
	[[ -d "$TAP/Formula" ]] || die "$TAP has no Formula directory."
	if [[ -e "$MARKER" || -e "$THROWAWAY" ]]; then
		die "leftovers of an earlier run exist ($MARKER or $THROWAWAY); run: bash $SELF --cleanup"
	fi
}

WROTE_FILE=0 HOLDERS="" HOLDER="" HOLDER_REAPED=0
HAVE_LOCK=0 WORK="" DID_RUN=0

# begin <step>: record in the marker, then set the flag, before the step runs,
# so a --cleanup after any interruption undoes exactly what this script began.
begin() {
	echo "$1" >>"$MARKER"
	case "$1" in
	file) WROTE_FILE=1 ;;
	*) die "internal error: unknown step $1" ;;
	esac
}

# note_holder <pid>: record the holder's pid right after it starts.
note_holder() {
	echo "holder $1" >>"$MARKER"
	HOLDERS="$HOLDERS $1"
}

load_marker() {
	local k pid
	[[ -f "$MARKER" ]] || return 0
	while IFS= read -r k; do
		case "$k" in
		file) WROTE_FILE=1 ;;
		holder\ *)
			pid="${k#holder }"
			[[ "$pid" =~ ^[0-9]+$ ]] && HOLDERS="$HOLDERS $pid"
			;;
		esac
	done <"$MARKER"
}

# What this check can leave behind, one item per line (empty when clean).
leftovers() {
	local pid
	for pid in $HOLDERS; do
		if is_holder "$pid"; then echo "lock holder, process group $pid (kill -s TERM -- -$pid)"; fi
	done
	if [[ -e "$THROWAWAY" ]]; then echo "file $THROWAWAY"; fi
	return 0
}

# kill_holder <pid>: end the holder's process group, only while its command
# line still names the lock. Returns 1 when it could not be ended.
kill_holder() {
	local pid="$1" i
	alive "$pid" || return 0
	if ! is_holder "$pid"; then
		echo "cleanup: pid $pid is no longer the lock holder; left alone" >&2
		return 0
	fi
	kill -s TERM -- "-$pid" 2>/dev/null || kill -s TERM "$pid" 2>/dev/null || true
	for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		alive "$pid" || return 0
		sleep 0.2
	done
	kill -s KILL -- "-$pid" 2>/dev/null || kill -s KILL "$pid" 2>/dev/null || true
	sleep 0.2
	if alive "$pid"; then return 1; fi
	return 0
}

cleanup_steps() {
	local ok=0 pid
	for pid in $HOLDERS; do
		kill_holder "$pid" || {
			echo "cleanup warning: could not end the holder (pid $pid)" >&2
			ok=1
		}
	done
	# Reap our own child quietly: a killed job would otherwise be reported.
	if [[ -n "$HOLDER" ]] && ((HOLDER_REAPED == 0)); then
		{ wait "$HOLDER"; } 2>/dev/null || true
		HOLDER_REAPED=1
	fi
	if ((WROTE_FILE)) && [[ -e "$THROWAWAY" ]]; then
		rm -f -- "$THROWAWAY" || {
			echo "cleanup warning: could not remove $THROWAWAY" >&2
			ok=1
		}
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

indent() { # <text>: print each line indented two spaces
	local l
	while IFS= read -r l; do printf '  %s\n' "$l"; done <<<"$1"
}

finish() {
	local rc=$?
	trap - EXIT INT TERM HUP
	set +e
	if ((DID_RUN)); then
		cleanup_steps
		local crc=$?
		local left
		left="$(leftovers)"
		if [[ -n "$left" ]]; then
			rc=1
			echo "leftovers:" >&2
			indent "$left" >&2
			echo "retry the cleanup with: bash $SELF --cleanup" >&2
		elif ((crc != 0)); then
			rc=1
		else
			rm -f -- "$MARKER"
			echo "state: unchanged (throwaway file removed, holder ended)"
		fi
		case "$rc" in
		0)
			echo "lock-check-25: PASS"
			rm -r -- "$WORK"
			;;
		3) echo "lock-check-25: VOID; outputs kept in $WORK" >&2 ;;
		*) echo "lock-check-25: FAIL; outputs kept in $WORK; marker $MARKER is kept only if leftovers remain" >&2 ;;
		esac
	fi
	release_lock
	exit "$rc"
}

fail() {
	echo "FAIL: $*" >&2
	exit 1
}

take_lock() {
	mkdir "$LOCK" 2>/dev/null ||
		die "another run holds $LOCK; if none is running, remove it with: rm -r $LOCK"
	HAVE_LOCK=1
}

setup_work() {
	WORK="$(mktemp -d "${TMPDIR:-/tmp}/lock-check-25.XXXXXX")"
}

# The ruby that holds brew's own formula lock, then reports it.
holder_ruby() {
	# shellcheck disable=SC2016 # $stdout is ruby
	printf 'require "lock_file"; FormulaLock.new("%s").with_lock { puts "held"; $stdout.flush; sleep %s }' "$NAME" "$HOLD"
}

# one_run <label> <outfile>: brewsoak update -v, output kept and echoed.
one_run() {
	local label="$1" file="$2" rc=0
	"$BIN" update -v >"$file" 2>&1 || rc=$?
	echo "--- $label: brewsoak update -v (exit $rc), lines naming $NAME:"
	{ grep -F -- "$NAME" "$file" || true; } | while IFS= read -r l; do printf '  %s\n' "$l"; done
	((rc == 0)) || fail "$label: brewsoak update -v exited $rc; output in $file"
}

do_run() {
	init_tools
	resolve_bin "${1:-}"
	preconditions
	take_lock
	setup_work
	trap finish EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM
	trap 'exit 129' HUP
	: >"$MARKER"
	DID_RUN=1

	echo "step: warm brewsoak's snapshots with one update"
	"$BIN" update >"$WORK/warm.out" 2>&1 || fail "the warm-up 'brewsoak update' exited non-zero; output in $WORK/warm.out"

	echo "step: write the throwaway $THROWAWAY"
	begin file
	printf 'class ZzzBrewsoakLockCheck < Formula\n  url "https://example.com/zzz-1.0.tar.gz"\n  sha256 "0000000000000000000000000000000000000000000000000000000000000000"\nend\n' >"$THROWAWAY"

	echo "step: hold brew's formula lock for $NAME with brew ruby ($HOLD s)"
	"$PERL" -e 'setpgrp(0,0); exec @ARGV' "$BREW" ruby -e "$(holder_ruby)" >"$WORK/holder.out" 2>&1 &
	HOLDER=$!
	note_holder "$HOLDER"
	local i ready=0
	for ((i = 0; i < 300; i++)); do
		if grep -qx held "$WORK/holder.out" 2>/dev/null; then
			ready=1
			break
		fi
		alive "$HOLDER" || break
		sleep 0.2
	done
	if ((ready == 0)); then
		grep -qx held "$WORK/holder.out" 2>/dev/null && ready=1
	fi
	if ((ready == 0)); then
		fail "the lock holder did not take the lock (no 'held' from brew ruby); its output:
$(tail -n 15 "$WORK/holder.out" 2>/dev/null || true)"
	fi
	local t0 t1 gap
	t0="$(date +%s)"
	echo "holder: lock held at $(date)"

	echo "step: first run, while brew holds the lock"
	one_run "run 1" "$WORK/run1.out"
	t1="$(date +%s)"
	gap=$((t1 - t0))
	echo "timing: the first run ended $gap s after the lock was taken (must be under $MAX_GAP s)"
	if ((gap >= MAX_GAP)); then
		echo "VOID: the hold may have ended before the first run finished, so its result proves nothing; run the check again." >&2
		exit 3
	fi
	if grep -Eq "^[[:space:]]*pruned .*Formula/$NAME\.rb" "$WORK/run1.out" || [[ ! -e "$THROWAWAY" ]]; then
		fail "the first run removed the file while brew held its lock: brew's Ruby flock and brewsoak's lock do not exclude each other. STOP and report this; do not merge."
	fi
	grep -Eq "^[[:space:]]*not pruning .*Formula/$NAME\.rb: brew holds the lock for $NAME\$" "$WORK/run1.out" ||
		fail "the first run did not print 'not pruning ...: brew holds the lock for $NAME'; output in $WORK/run1.out"

	echo "step: wait for the holder to let go"
	local hrc=0
	wait "$HOLDER" || hrc=$?
	HOLDER_REAPED=1
	((hrc == 0)) || fail "the lock holder exited with status $hrc; output in $WORK/holder.out"

	echo "step: second run, after brew let go"
	one_run "run 2" "$WORK/run2.out"
	grep -Eq "^[[:space:]]*pruned .*Formula/$NAME\.rb \(no installed package uses it\)\$" "$WORK/run2.out" ||
		fail "the second run did not print 'pruned ...Formula/$NAME.rb (no installed package uses it)'; output in $WORK/run2.out"
	[[ ! -e "$THROWAWAY" ]] || fail "the second run left $THROWAWAY in place"
	echo "check: busy lock kept the file, the released lock let the prune remove it"
}

do_cleanup() {
	init_tools
	take_lock
	setup_work
	load_marker
	local left
	if [[ ! -f "$MARKER" ]]; then
		left="$(leftovers)"
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
	left="$(leftovers)"
	if [[ -n "$left" ]]; then
		echo "cleanup: leftovers remain:" >&2
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
# Fake brew for lock-check-25.sh --self-test. State lives under $FAKE_ROOT.
set -euo pipefail
R="$FAKE_ROOT"
[[ "${HOMEBREW_NO_AUTO_UPDATE:-}" == 1 ]] || { echo "fake brew: HOMEBREW_NO_AUTO_UPDATE not set" >&2; exit 97; }
echo "$*" >>"$R/brew.calls"
[[ "${FAKE_FAIL:-}" == "${1:-}" ]] && { echo "Error: fake $1 failure" >&2; exit 1; }
case "$1" in
--repository) echo "$R/repo" ;;
tap-info) cat "$R/tapinfo.json" ;;
ruby)
	# The holder: record the pid the check can probe, say "held", keep the
	# "lock" for FAKE_HOLD seconds. exec keeps the pid and puts the lock name
	# on the command line, as the real brew ruby -e script does.
	[[ "$2" == -e && "$3" == *'FormulaLock.new("zzz-brewsoak-lock-check")'* && "$3" == *'puts "held"'* ]] || { echo "fake brew: unexpected ruby script: $*" >&2; exit 65; }
	echo "$$" >"$R/holder.pid"
	echo held
	exec perl -e 'sleep shift' "${FAKE_HOLD:-120}" "$3"
	;;
*) echo "fake brew: unsupported: $*" >&2; exit 64 ;;
esac
STUB
	chmod +x "$1/brew"
}

write_fake_brewsoak() { # <path>
	cat >"$1" <<'STUB'
#!/usr/bin/env bash
# Fake brewsoak for lock-check-25.sh --self-test: prunes the throwaway file
# unless the fake brew ruby holder is alive (FAKE_PRUNE_ALWAYS=1: even then).
set -euo pipefail
R="$FAKE_ROOT"
echo "$*" >>"$R/brewsoak.calls"
[[ "${1:-}" == update ]] || exit 64
F="$R/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula/zzz-brewsoak-lock-check.rb"
verbose=0
if [[ "${2:-}" == -v ]]; then verbose=1; fi
[[ -e "$F" ]] || exit 0
held=0
if [[ -f "$R/holder.pid" ]]; then
	st="$(ps -o stat= -p "$(cat "$R/holder.pid")" 2>/dev/null || true)"
	st="${st//[[:space:]]/}"
	if [[ -n "$st" && "$st" != Z* ]]; then held=1; fi
fi
if ((held)) && [[ -z "${FAKE_PRUNE_ALWAYS:-}" ]]; then
	if ((verbose)); then echo "not pruning $F: brew holds the lock for zzz-brewsoak-lock-check"; fi
else
	rm -f -- "$F"
	if ((verbose)); then echo "pruned $F (no installed package uses it)"; fi
fi
STUB
	chmod +x "$1"
}

reset_fake() { # <root> <home>
	rm -rf "${1:?}" "${2:?}"
	mkdir -p "$1/repo/Library/Taps/ericfitz/homebrew-brewsoak/Formula" "$2" "$1/tmp"
	echo '[{"name":"ericfitz/brewsoak","remote":null,"trusted":true}]' >"$1/tapinfo.json"
	: >"$1/brew.calls"
	: >"$1/brewsoak.calls"
}

self_test() {
	local total=0 failed=0 names=()
	T="$(mktemp -d "${TMPDIR:-/tmp}/lock-check-25-selftest.XXXXXX")"
	trap 'rm -r -- "$T"' EXIT
	mkdir -p "$T/bin" "$T/tools"
	write_fake_brew "$T/bin"
	write_fake_brewsoak "$T/fake-brewsoak"
	local JQBIN
	JQBIN="$(command -v jq || true)"
	[[ "$JQBIN" == /* ]] || die "jq not found on PATH; run: brew install jq"
	local t p
	for t in bash jq perl dirname basename mktemp rm mkdir cat grep date sleep kill ps tail head tr cut env sed; do
		p="$(command -v "$t" || true)"
		if [[ "$p" == /* ]]; then ln -s "$p" "$T/tools/$t"; fi
	done

	local ROOT="$T/fake" HOME_="$T/home" out rc
	local STATE="$T/fake/state/brewsoak"
	local TAPDIR="$ROOT/repo/Library/Taps/ericfitz/homebrew-brewsoak"
	local FILE="$TAPDIR/Formula/$NAME.rb"

	# in_env <PATH> [VAR=value...]: environment for one run against the fakes.
	in_env() {
		local path="$1"
		shift
		env -i PATH="$path" HOME="$HOME_" XDG_STATE_HOME="$ROOT/state" FAKE_ROOT="$ROOT" TMPDIR="$ROOT/tmp" FAKE_HOLD=2 "$@"
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
	hasnt() { check "$1" "$(grep -qF -- "$2" <<<"$out" && echo 1 || echo 0)" "unexpected '$2' in: $out"; }
	# nothing_left <case>: no throwaway file, marker, lock directory or live holder
	nothing_left() {
		local pid=""
		[[ -f "$ROOT/holder.pid" ]] && pid="$(cat "$ROOT/holder.pid")"
		check "$1-no-file" "$([[ ! -e "$FILE" ]] && echo 0 || echo 1)" "throwaway file left"
		check "$1-no-marker" "$([[ ! -e "$STATE/lock-check-25.marker" ]] && echo 0 || echo 1)" "marker left"
		check "$1-no-lock" "$([[ ! -e "$STATE/lock-check-25.lock" ]] && echo 0 || echo 1)" "lock directory left"
		check "$1-no-holder" "$(alive "$pid" && echo 1 || echo 0)" "holder pid $pid still alive"
	}
	P="$T/bin:$T/tools"
	: >"$T/failures.txt"

	# 1. pass: both prune lines, state clean, calls as planned
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc pass-exit 0
	has pass-summary "lock-check-25: PASS"
	has pass-busy-line "not pruning $FILE: brew holds the lock for $NAME"
	has pass-pruned-line "pruned $FILE (no installed package uses it)"
	has pass-timing "timing: the first run ended"
	nothing_left pass
	check pass-calls "$([[ "$(cat "$ROOT/brewsoak.calls")" == "$(printf 'update\nupdate -v\nupdate -v')" ]] && echo 0 || echo 1)" "brewsoak calls: $(cat "$ROOT/brewsoak.calls")"

	# 2. a second run behaves like the first
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc second-run-exit 0
	has second-run-pass "lock-check-25: PASS"
	nothing_left second-run

	# 3. the first run prunes while the lock is held: fail, say stop, clean up
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_PRUNE_ALWAYS=1 bash "$SELF" --run "$T/fake-brewsoak"
	want_rc prunes-exit 1
	has prunes-msg "do not merge"
	has prunes-fail "lock-check-25: FAIL"
	nothing_left prunes

	# 4. a timing that cannot prove anything is void (exit 3), and cleaned up
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" LOCK_CHECK_HOLD=20 bash "$SELF" --run "$T/fake-brewsoak"
	want_rc void-exit 3
	has void-msg "VOID"
	hasnt void-not-pass "lock-check-25: PASS"
	nothing_left void

	# 5. the holder fails to start: fail, nothing run against the file, clean
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_FAIL=ruby bash "$SELF" --run "$T/fake-brewsoak"
	want_rc holder-fails-exit 1
	has holder-fails-msg "the lock holder did not take the lock"
	has holder-fails-why "fake ruby failure"
	check holder-fails-no-update-v "$(grep -qx 'update -v' "$ROOT/brewsoak.calls" && echo 1 || echo 0)" "$(cat "$ROOT/brewsoak.calls")"
	rm -f "$ROOT/holder.pid"
	nothing_left holder-fails

	# 6. SIGTERM while the holder runs: the trap kills it and removes the file
	reset_fake "$ROOT" "$HOME_"
	in_env "$P" FAKE_HOLD=30 bash "$SELF" --run "$T/fake-brewsoak" >"$T/term.out" 2>&1 &
	local pid=$! i
	for i in $(seq 1 150); do
		[[ -f "$ROOT/holder.pid" && "$(grep -c . "$ROOT/brewsoak.calls")" -ge 2 ]] && break
		sleep 0.1
	done
	local t_kill
	t_kill="$(date +%s)"
	kill -TERM "$pid" 2>/dev/null || true
	rc=0
	wait "$pid" || rc=$?
	out="$(cat "$T/term.out")"
	want_rc term-exit 143
	# The holder sleeps 30 s: finishing sooner means the trap killed it.
	check term-kills-holder "$([[ $(($(date +%s) - t_kill)) -lt 15 ]] && echo 0 || echo 1)" "the script waited for the holder to end by itself"
	nothing_left term

	# 6b. SIGHUP behaves the same
	reset_fake "$ROOT" "$HOME_"
	in_env "$P" FAKE_HOLD=30 bash "$SELF" --run "$T/fake-brewsoak" >"$T/hup.out" 2>&1 &
	pid=$!
	for i in $(seq 1 150); do
		[[ -f "$ROOT/holder.pid" && "$(grep -c . "$ROOT/brewsoak.calls")" -ge 2 ]] && break
		sleep 0.1
	done
	kill -HUP "$pid" 2>/dev/null || true
	rc=0
	wait "$pid" || rc=$?
	out="$(cat "$T/hup.out")"
	want_rc hup-exit 129
	nothing_left hup

	# 7. leftovers of a killed run: --run refuses, --cleanup ends the holder and
	# removes the file (twice is safe); a recorded pid that no longer names the
	# lock is left alone
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$STATE"
	: >"$FILE"
	(
		"$(command -v perl)" -e 'setpgrp(0,0); exec @ARGV' perl -e 'sleep shift' 60 "$NAME" >/dev/null 2>&1 &
		echo $! >"$T/orphan.pid"
	)
	local orphan
	for i in $(seq 1 50); do
		[[ -s "$T/orphan.pid" ]] && break
		sleep 0.1
	done
	orphan="$(cat "$T/orphan.pid")"
	printf 'file\nholder %s\n' "$orphan" >"$STATE/lock-check-25.marker"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc leftovers-exit 2
	has leftovers-msg "--cleanup"
	check leftovers-untouched "$(is_holder "$orphan" && [[ -e "$FILE" ]] && echo 0 || echo 1)" "--run touched the leftovers"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-exit 0
	has cleanup-msg "cleanup: done"
	check cleanup-holder-ended "$(alive "$orphan" && echo 1 || echo 0)" "orphan $orphan still alive"
	nothing_left cleanup
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-idempotent-exit 0
	has cleanup-idempotent-msg "nothing to do"
	(
		"$(command -v perl)" -e 'setpgrp(0,0); exec @ARGV' perl -e 'sleep shift' 60 unrelated >/dev/null 2>&1 &
		echo $! >"$T/other.pid"
	)
	for i in $(seq 1 50); do
		[[ -s "$T/other.pid" ]] && break
		sleep 0.1
	done
	local other
	other="$(cat "$T/other.pid")"
	printf 'file\nholder %s\n' "$other" >"$STATE/lock-check-25.marker"
	: >"$FILE"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-reused-pid-exit 0
	has cleanup-reused-pid-msg "no longer the lock holder; left alone"
	check cleanup-reused-pid-kept "$(alive "$other" && echo 0 || echo 1)" "an unrelated process was killed"
	kill -s KILL -- "-$other" 2>/dev/null || kill -s KILL "$other" 2>/dev/null || true

	# 7b. --cleanup with no marker removes nothing, even a file of that name
	reset_fake "$ROOT" "$HOME_"
	: >"$FILE"
	run_case "$P" bash "$SELF" --cleanup
	want_rc cleanup-nomarker-exit 1
	has cleanup-nomarker-msg "no marker"
	check cleanup-nomarker-kept "$([[ -e "$FILE" ]] && echo 0 || echo 1)" "removed a file no marker named"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc stale-file-exit 2
	has stale-file-msg "--cleanup"
	rm -f "$FILE"

	# 8. preconditions
	reset_fake "$ROOT" "$HOME_"
	echo '[]' >"$ROOT/tapinfo.json"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc no-tap-exit 2
	has no-tap-msg "is not tapped"
	reset_fake "$ROOT" "$HOME_"
	echo '[{"name":"ericfitz/brewsoak","remote":"https://example.com/x","trusted":true}]' >"$ROOT/tapinfo.json"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc remote-tap-exit 2
	has remote-tap-msg "has a remote"
	reset_fake "$ROOT" "$HOME_"
	echo '[{"name":"ericfitz/brewsoak","remote":null,"trusted":false}]' >"$ROOT/tapinfo.json"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc untrusted-tap-exit 2
	has untrusted-tap-msg "is not trusted; run 'brew trust ericfitz/brewsoak' yourself"
	reset_fake "$ROOT" "$HOME_"
	echo '[{"name":"ericfitz/brewsoak","remote":null}]' >"$ROOT/tapinfo.json"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc missing-trusted-field-exit 0
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" FAKE_FAIL=tap-info bash "$SELF" --run "$T/fake-brewsoak"
	want_rc tap-info-fails-exit 2
	has tap-info-fails-msg "'brew tap-info --json --installed' failed"
	reset_fake "$ROOT" "$HOME_"
	run_case "$P" bash "$SELF" --run "$T/does-not-exist"
	want_rc no-binary-exit 2
	has no-binary-msg "no executable brewsoak at"
	run_case "$P" bash "$SELF" --run "relative/brewsoak"
	want_rc relative-binary-exit 2
	has relative-binary-msg "must be an absolute path"
	check preconditions-no-mutation "$(grep -qE '^ruby' "$ROOT/brew.calls" && echo 1 || echo 0)" "$(cat "$ROOT/brew.calls")"

	# 9. missing tools
	reset_fake "$ROOT" "$HOME_"
	run_case "$T/tools" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc no-brew-exit 2
	has no-brew-msg "brew not found"
	mkdir -p "$T/tools-nojq"
	for p in "$T/tools"/*; do
		if [[ "${p##*/}" != jq ]]; then ln -s "$(readlink "$p")" "$T/tools-nojq/${p##*/}"; fi
	done
	ln -s "$T/bin/brew" "$T/tools-nojq/brew"
	run_case "$T/tools-nojq" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc no-jq-exit 2
	has no-jq-msg "brew install jq"

	# 10. a held lock directory refuses a second run and is not removed by it
	reset_fake "$ROOT" "$HOME_"
	mkdir -p "$STATE/lock-check-25.lock"
	run_case "$P" bash "$SELF" --run "$T/fake-brewsoak"
	want_rc lock-exit 2
	has lock-msg "another run holds"
	check lock-kept "$([[ -d "$STATE/lock-check-25.lock" ]] && echo 0 || echo 1)" "the other run's lock was removed"
	rm -r -- "$STATE/lock-check-25.lock"

	# 11. a bad hold length is refused
	run_case "$P" LOCK_CHECK_HOLD=5 bash "$SELF" --run "$T/fake-brewsoak"
	want_rc bad-hold-exit 2
	has bad-hold-msg "LOCK_CHECK_HOLD"

	echo "self-test: $total checks, $((total - failed)) passed, $failed failed"
	if ((failed)); then
		echo "failed: ${names[*]}"
		cat "$T/failures.txt"
		return 1
	fi
}

case "${1:-}" in
--run) do_run "${2:-}" ;;
--cleanup) do_cleanup ;;
--self-test) self_test ;;
*)
	echo "usage: lock-check-25.sh --run [BREWSOAK] | --cleanup | --self-test" >&2
	exit 2
	;;
esac
