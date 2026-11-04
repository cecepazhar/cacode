#!/usr/bin/env bash
# CACode — real-provider end-to-end test.
#
# Runs 7 realistic tasks against every provider you have a key for, in fresh
# throw-away Rust fixtures, with an isolated HOME (your ~/.cacode is untouched).
#
#   ANTHROPIC_API_KEY=... DEEPSEEK_API_KEY=... scripts/e2e_real.sh
#   scripts/e2e_real.sh anthropic ollama          # only these providers
#
# Providers & env vars:
#   anthropic   ANTHROPIC_API_KEY            (model: $E2E_ANTHROPIC_MODEL, default claude-sonnet-4-20250514)
#   openai      OPENAI_API_KEY               (model: $E2E_OPENAI_MODEL,    default gpt-4o-mini)
#   deepseek    DEEPSEEK_API_KEY             (model: $E2E_DEEPSEEK_MODEL,  default deepseek-chat)
#   openrouter  OPENROUTER_API_KEY           (model: $E2E_OPENROUTER_MODEL, default anthropic/claude-sonnet-4)
#   ollama      OLLAMA_URL or local :11434   (model: $E2E_OLLAMA_MODEL,    default qwen2.5-coder:7b)
#   router9     ROUTER9_URL                  (model: $E2E_ROUTER9_MODEL,   default ZA126_PRO)
#
# Requires: cargo, git, python3. Output: e2e-results/<timestamp>/report.md + results.json
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${CACODE_BIN:-$REPO/target/release/cacode}"
TASK_TIMEOUT="${E2E_TIMEOUT:-600}"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$REPO/e2e-results/$STAMP"
FX="$(mktemp -d "${TMPDIR:-/tmp}/cacode-e2e.XXXXXX")"
mkdir -p "$OUT"

for tool in cargo git python3; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done
if [ -z "${CACODE_BIN:-}" ]; then
  echo "== building release binary (no-op when up to date)"
  (cd "$REPO" && cargo build --release -q -p cacode-app) || exit 1
fi

REAL_HOME="$HOME"
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"

# ---------------------------------------------------------------- fixture --
make_fixture() {  # $1 = dir
  local d="$1"
  mkdir -p "$d/src" "$d/tests"
  cat > "$d/Cargo.toml" <<'T'
[workspace]

[package]
name = "e2e_fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
T
  cat > "$d/src/lib.rs" <<'T'
pub mod billing;
pub mod config;

/// Parses a TCP port. Returns None for non-numbers and values outside 1..=65535.
pub fn parse_port(s: &str) -> Option<u16> {
    let n: u32 = s.trim().parse().ok()?;
    if n == 0 || n > 65535 {
        return None;
    }
    Some(n as u16)
}

/// Sum of the first `n` positive integers (1 + 2 + ... + n).
pub fn triangle(n: u64) -> u64 {
    (1..n).sum() // BUG: should include n
}
T
  cat > "$d/src/billing.rs" <<'T'
pub struct LineItem {
    pub price_cents: u64,
    pub qty: u64,
}

pub fn calc_total(items: &[LineItem]) -> u64 {
    items.iter().map(|i| i.price_cents * i.qty).sum()
}

pub fn calc_total_with_tax(items: &[LineItem], tax_pct: u64) -> u64 {
    let t = calc_total(items);
    t + t * tax_pct / 100
}
T
  cat > "$d/src/config.rs" <<'T'
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub port: u16,
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: 8080, name: "app".into() }
    }
}
T
  cat > "$d/tests/core.rs" <<'T'
use e2e_fixture::billing::{calc_total, LineItem};
use e2e_fixture::*;

#[test]
fn triangle_works() {
    assert_eq!(triangle(1), 1);
    assert_eq!(triangle(4), 10);
}

#[test]
fn port_parsing() {
    assert_eq!(parse_port("8080"), Some(8080));
    assert_eq!(parse_port("0"), None);
    assert_eq!(parse_port("x"), None);
}

#[test]
fn totals() {
    assert_eq!(calc_total(&[LineItem { price_cents: 250, qty: 2 }]), 500);
}
T
  echo "target/" > "$d/.gitignore"
  (cd "$d" && git init -q && git add -A && git -c user.email=e2e@local -c user.name=e2e commit -qm fixture)
  (cd "$d" && cargo build -q --tests >/dev/null 2>&1)
}

# ------------------------------------------------------------- providers --
setup_provider() {  # sets PROV_ENV (array) and PROV_MODEL; returns 1 if unavailable
  PROV_ENV=()
  case "$1" in
    anthropic)  [ -n "${ANTHROPIC_API_KEY:-}" ] || return 1
                PROV_ENV=(ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY"); PROV_MODEL="${E2E_ANTHROPIC_MODEL:-claude-sonnet-4-20250514}" ;;
    openai)     [ -n "${OPENAI_API_KEY:-}" ] || return 1
                PROV_ENV=(OPENAI_API_KEY="$OPENAI_API_KEY"); PROV_MODEL="${E2E_OPENAI_MODEL:-gpt-4o-mini}" ;;
    deepseek)   [ -n "${DEEPSEEK_API_KEY:-}" ] || return 1
                PROV_ENV=(OPENAI_API_KEY="$DEEPSEEK_API_KEY" OPENAI_BASE_URL="https://api.deepseek.com"); PROV_MODEL="${E2E_DEEPSEEK_MODEL:-deepseek-chat}" ;;
    openrouter) [ -n "${OPENROUTER_API_KEY:-}" ] || return 1
                PROV_ENV=(OPENAI_API_KEY="$OPENROUTER_API_KEY" OPENAI_BASE_URL="https://openrouter.ai/api/v1"); PROV_MODEL="${E2E_OPENROUTER_MODEL:-anthropic/claude-sonnet-4}" ;;
    ollama)     local u="${OLLAMA_URL:-http://127.0.0.1:11434}"
                curl -fsS --max-time 3 "$u/api/tags" >/dev/null 2>&1 || return 1
                PROV_ENV=(OLLAMA_URL="$u"); PROV_MODEL="${E2E_OLLAMA_MODEL:-qwen2.5-coder:7b}" ;;
    router9)    [ -n "${ROUTER9_URL:-}" ] || return 1
                PROV_ENV=(ROUTER9_URL="$ROUTER9_URL" ROUTER9_API_KEY="${ROUTER9_API_KEY:-dummy}"); PROV_MODEL="${E2E_ROUTER9_MODEL:-ZA126_PRO}" ;;
    *) echo "unknown provider $1" >&2; return 1 ;;
  esac
}

# run_cacode <provider> <fixture> <task-id> <mode> <prompt> [extra args...]
run_cacode() {
  local prov="$1" dir="$2" id="$3" mode="$4" prompt="$5"; shift 5
  local home="$OUT/home-$prov"; mkdir -p "$home"
  local log="$OUT/$prov-$id"
  ( cd "$dir" && env -i PATH="$PATH" HOME="$home" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" TERM=dumb \
      CACODE_SANDBOX="${E2E_SANDBOX:-auto}" "${PROV_ENV[@]}" \
      timeout "$TASK_TIMEOUT" "$BIN" -p "$prompt" --output-format json --permission-mode "$mode" --model "$PROV_MODEL" --max-steps 30 "$@" \
      >"$log.json" 2>"$log.stderr" )
  echo $? > "$log.exit"
}

# --------------------------------------------------------------- checks --
# Each check prints "PASS <note>" or "FAIL <note>".
check() {  # <provider> <task> <dir>
  local prov="$1" id="$2" dir="$3" log="$OUT/$1-$2"
  python3 - "$log" "$id" "$dir" <<'PY'
import json, subprocess, sys, os, hashlib
log, tid, d = sys.argv[1:4]
try:
    res = json.loads(open(log + ".json").read().strip().splitlines()[-1])
except Exception as e:
    print("FAIL no JSON result (%s); see %s.stderr" % (e, os.path.basename(log))); sys.exit()
code = int(open(log + ".exit").read().strip() or 1)
text = (res.get("result") or "")
tools = res.get("tool_calls", [])
def cargo_test():
    r = subprocess.run(["cargo", "test", "-q"], cwd=d, capture_output=True, text=True)
    return r.returncode == 0, (r.stdout + r.stderr)[-400:]
def git_diff(path): return subprocess.run(["git", "diff", "--stat", "--", path], cwd=d, capture_output=True, text=True).stdout.strip()
ok, why = False, ""
if code == 124: print("FAIL timeout"); sys.exit()
if tid == "T1_read_only_question":
    ok = "option" in text.lower() and "u16" in text.lower(); why = "answer: " + text[:80].replace("\n", " ")
    ok = ok and not res.get("files_touched")
elif tid == "T2_fix_failing_test":
    passed, out = cargo_test(); ok = passed and git_diff("tests") == ""; why = "tests unchanged" if git_diff("tests") == "" else "TESTS WERE MODIFIED"
    if not passed: why = "cargo test still fails: " + out[-160:].replace("\n", " ")
elif tid == "T3_add_feature_with_test":
    passed, out = cargo_test(); src = open(os.path.join(d, "src/lib.rs")).read()
    has_fn = "fn is_even" in src or any("fn is_even" in open(os.path.join(r, f)).read() for r, _, fs in os.walk(os.path.join(d, "src")) for f in fs)
    has_test = "is_even" in src.split("fn is_even", 1)[-1] or "is_even" in "".join(open(os.path.join(d, "tests", f)).read() for f in os.listdir(os.path.join(d, "tests")))
    ok = passed and has_fn and has_test; why = "fn=%s test=%s cargo_test=%s" % (has_fn, has_test, passed)
elif tid == "T4_multi_file_rename":
    grep = subprocess.run(["git", "grep", "-n", "calc_total\\b"], cwd=d, capture_output=True, text=True).stdout
    new = subprocess.run(["git", "grep", "-c", "compute_total"], cwd=d, capture_output=True, text=True).stdout
    passed, out = cargo_test(); ok = passed and grep.strip() == "" and new.strip() != ""
    why = "old name left: %s" % grep.strip()[:120] if grep.strip() else ("cargo test ok" if passed else "cargo test fails")
elif tid == "T5_subagents":
    n_task = sum(1 for t in tools if t.get("tool") == "task")
    ok = n_task >= 2 and "config" in text.lower(); why = "%d task calls" % n_task
elif tid == "T6_resume_session":
    ok = "pelangi" in text.lower(); why = "answer: " + text[:60].replace("\n", " ")
elif tid == "T7_web_fetch":
    wf = [t for t in tools if t.get("tool") == "web_fetch"]
    ok = bool(wf) and not all(t.get("is_error") for t in wf) and any(ch.isdigit() for ch in text); why = "%d web_fetch calls" % len(wf)
u = res.get("usage", {})
meta = "exit=%s tok=%s%s calls=%s %.0fs" % (code, u.get("total_tokens"), " EST" if u.get("estimated") else "", res.get("llm_calls"), res.get("duration_ms", 0) / 1000)
print(("PASS " if ok and code in (0,) else "FAIL ") + why + " | " + meta)
PY
}

TASKS=(T1_read_only_question T2_fix_failing_test T3_add_feature_with_test T4_multi_file_rename T5_subagents T6_resume_session T7_web_fetch)
PROVIDERS=("$@"); [ ${#PROVIDERS[@]} -eq 0 ] && PROVIDERS=(anthropic openai deepseek openrouter ollama router9)

REPORT="$OUT/report.md"
{
  echo "# CACode e2e — $STAMP"
  echo
  echo "cacode: \`$("$BIN" --version)\` · host: \`$(uname -srm)\` · bwrap: \`$(command -v bwrap || echo none)\`"
  echo
  echo "| Provider | Model | Task | Result |"
  echo "|---|---|---|---|"
} > "$REPORT"

TOTAL=0; PASSED=0
for prov in "${PROVIDERS[@]}"; do
  if ! setup_provider "$prov"; then echo "== skip $prov (no key / not reachable)"; continue; fi
  echo "== $prov ($PROV_MODEL)"
  for t in "${TASKS[@]}"; do
    dir="$FX/$prov-$t"; make_fixture "$dir"
    case "$t" in
      T1_read_only_question) run_cacode "$prov" "$dir" "$t" read-only "What does parse_port return when the input is not a valid port? Reply with just the Rust return type." ;;
      T2_fix_failing_test)   run_cacode "$prov" "$dir" "$t" auto-edit "cargo test fails. Find and fix the bug in src/ (do not modify anything under tests/). Verify with cargo test." ;;
      T3_add_feature_with_test) run_cacode "$prov" "$dir" "$t" auto-edit "Add a public function is_even(n: i64) -> bool to src/lib.rs with a unit test for it. Make sure cargo test passes; if an existing test fails, fix the bug in src/ (never delete or weaken tests)." ;;
      T4_multi_file_rename)  run_cacode "$prov" "$dir" "$t" auto-edit "Rename the function calc_total to compute_total everywhere in the project (definition and all call sites, including tests). Verify it builds with cargo build --tests." ;;
      T5_subagents)          run_cacode "$prov" "$dir" "$t" read-only "Use the task tool to run two independent subagents in parallel: one counts the .rs files in the project, the other finds where the Config struct is defined and its default port. Then summarise both results." ;;
      T6_resume_session)     run_cacode "$prov" "$dir" "T6a" read-only "Remember this code word for later: PELANGI. Reply only with OK."
                             run_cacode "$prov" "$dir" "$t" read-only "What was the code word I asked you to remember?" --continue ;;
      T7_web_fetch)          run_cacode "$prov" "$dir" "$t" ask "Use web_fetch on https://crates.io/api/v1/crates/serde and tell me the max_version of serde." ;;
    esac
    r="$(check "$prov" "$t" "$dir" | sed "s/ | / · /g")"
    TOTAL=$((TOTAL+1)); [[ "$r" == PASS* ]] && PASSED=$((PASSED+1))
    printf '   %-26s %s\n' "$t" "$r"
    echo "| $prov | $PROV_MODEL | $t | $r |" >> "$REPORT"
  done
done

python3 - "$OUT" <<'PY' > "$OUT/results.json"
import json, os, sys, glob
out = sys.argv[1]; rows = []
for f in sorted(glob.glob(os.path.join(out, "*.json"))):
    if f.endswith("results.json"): continue
    try: rows.append({"run": os.path.basename(f)[:-5], "result": json.loads(open(f).read().strip().splitlines()[-1])})
    except Exception as e: rows.append({"run": os.path.basename(f)[:-5], "error": str(e)})
print(json.dumps(rows, indent=1))
PY
{ echo; echo "**Total: $PASSED / $TOTAL passed**"; } >> "$REPORT"
echo
echo "== $PASSED / $TOTAL passed"
echo "Report:  $REPORT"
echo "Details: $OUT  (send report.md + results.json back to Claude)"
echo "Fixtures kept for inspection: $FX"
