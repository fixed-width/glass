#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin || "${GITHUB_ACTIONS:-}" != true || "${RUNNER_ENVIRONMENT:-}" != github-hosted ]]; then
  echo 'Account lifecycle tests require a disposable GitHub-hosted macOS runner.' >&2
  exit 77
fi
account_context="${1:-}"
case "$account_context" in
  direct|system) ;;
  *) echo 'usage: test-macos-account.sh direct|system' >&2; exit 64 ;;
esac

account_test_dir=$(mktemp -d)
account_bootstrap_attempted=false
account_observed_pid=
account_label="tech.fixedwidth.glass.test.account.$(uuidgen)"

# Return 0 for our job, 1 for exact absence, and 2 for an unknown inspection result.
observe_account_job() {
  if account_job_state=$(sudo -n /bin/launchctl print "system/$account_label" 2>&1); then
    if ! printf '%s\n' "$account_job_state" | grep -Eq '^[[:space:]]*program = /usr/bin/env$' ||
       ! printf '%s\n' "$account_job_state" | sed 's/^[[:space:]]*//' | grep -Fxq "$account_test_dir/account-creation"; then
      return 2
    fi
    account_current_pid=$(printf '%s\n' "$account_job_state" | sed -nE 's/^[[:space:]]*pid = ([0-9]+)$/\1/p')
    if [[ -n "$account_current_pid" ]]; then
      [[ "$account_current_pid" =~ ^[0-9]+$ ]] || return 2
      [[ -z "$account_observed_pid" || "$account_current_pid" == "$account_observed_pid" ]] || return 2
      account_observed_pid=$account_current_pid
    fi
    return 0
  else
    account_query_status=$?
    if [[ "$account_query_status" == 113 &&
          "$(printf '%s\n' "$account_job_state" | tail -n 1)" == "Could not find service \"$account_label\" in domain for system" ]]; then
      return 1
    fi
    return 2
  fi
}

account_process_gone() {
  [[ -n "$account_observed_pid" ]] || return 0
  if account_pid_state=$(sudo -n /bin/ps -p "$account_observed_pid" -o pid= 2>&1); then
    return 1
  else
    account_pid_status=$?
    [[ "$account_pid_status" == 1 && -z "$account_pid_state" ]] && return 0
    return 2
  fi
}

cleanup() {
  account_exit_status=$?
  trap - EXIT
  account_may_remove=true
  if [[ "$account_bootstrap_attempted" == true ]]; then
    if observe_account_job; then
      sudo -n /bin/launchctl bootout "system/$account_label" || account_exit_status=1
    else
      account_inspection_status=$?
      if [[ "$account_inspection_status" != 1 ]]; then account_may_remove=false; fi
    fi
    account_cleanup_deadline=$((SECONDS+15))
    account_cleanup_confirmed=false
    while [[ "$account_may_remove" == true ]] && (( SECONDS < account_cleanup_deadline )); do
      if observe_account_job; then
        :
      else
        account_inspection_status=$?
        if [[ "$account_inspection_status" == 1 ]]; then
          if account_process_gone; then account_cleanup_confirmed=true; break; fi
        elif [[ "$account_inspection_status" == 2 ]]; then
          break
        fi
      fi
      sleep 1
    done
    if [[ "$account_cleanup_confirmed" != true ]]; then account_may_remove=false; fi
  fi
  if [[ "$account_may_remove" == true ]]; then
    sudo -n /bin/rm -rf "$account_test_dir"
  else
    echo "Owned job/process absence is unverified; retaining $account_test_dir on the disposable host." >&2
    account_exit_status=1
  fi
  exit "$account_exit_status"
}
trap cleanup EXIT

validate_account_result() {
  account_result_uid=$(printf '%s\n' "$1" | plutil -extract audit_uid raw -o - -) || return 1
  account_result_session=$(printf '%s\n' "$1" | plutil -extract audit_session raw -o - -) || return 1
  account_result_passed=$(printf '%s\n' "$1" | plutil -extract passed raw -o - -) || return 1
  if [[ "$account_result_uid" != "$2" || ! "$account_result_session" =~ ^[1-9][0-9]*$ ]]; then
    echo "Unexpected audit context: UID $account_result_uid, session $account_result_session; expected UID $2 and a positive session." >&2
    return 1
  fi
  [[ "$account_result_passed" == 1 || "$account_result_passed" == true ]]
}

sw_vers
xcrun clang -fobjc-arc -std=c11 \
  crates/glass-macos/tests/account_creation.m \
  crates/glass-macos/src/private_session.m \
  -framework Foundation -framework OpenDirectory -framework ApplicationServices \
  -framework Security -framework SystemConfiguration -lbsm \
  -o "$account_test_dir/account-creation"
"$account_test_dir/account-creation" --contract

account_args=(/usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin
  "GITHUB_ACTIONS=${GITHUB_ACTIONS:?}" "RUNNER_ENVIRONMENT=${RUNNER_ENVIRONMENT:?}"
  "$account_test_dir/account-creation" --disposable-ci)
if [[ "$account_context" == direct ]]; then
  sudo -n "${account_args[@]}" | tee "$account_test_dir/direct.stdout"
  validate_account_result "$(tail -n 1 "$account_test_dir/direct.stdout")" "$(id -u)"
  exit
fi

# launchd supplies a root security session independently of the runner's login session.
account_plist="$account_test_dir/account.plist"
account_stdout="$account_test_dir/account.stdout"
account_stderr="$account_test_dir/account.stderr"
touch "$account_stdout" "$account_stderr"
plutil -create xml1 "$account_plist"
plutil -insert Label -string "$account_label" "$account_plist"
plutil -insert ProgramArguments -json '[]' "$account_plist"
account_index=0
for account_arg in "${account_args[@]}"; do
  plutil -insert "ProgramArguments.$account_index" -string "$account_arg" "$account_plist"
  account_index=$((account_index+1))
done
plutil -insert RunAtLoad -bool YES "$account_plist"
plutil -insert KeepAlive -bool NO "$account_plist"
plutil -insert StandardOutPath -string "$account_stdout" "$account_plist"
plutil -insert StandardErrorPath -string "$account_stderr" "$account_plist"
sudo -n /usr/sbin/chown root:wheel "$account_test_dir" "$account_test_dir/account-creation" "$account_plist" "$account_stdout" "$account_stderr"
sudo -n /bin/chmod 755 "$account_test_dir" "$account_test_dir/account-creation"
sudo -n /bin/chmod 644 "$account_plist" "$account_stdout" "$account_stderr"
if observe_account_job; then
  echo 'The generated system job label is already in use.' >&2; exit 1
else
  [[ "$?" == 1 ]] || { echo 'Could not verify system job absence before bootstrap.' >&2; exit 1; }
fi
account_bootstrap_attempted=true
sudo -n /bin/launchctl bootstrap system "$account_plist"

account_deadline=$((SECONDS+180))
account_seen_lines=0
while (( SECONDS < account_deadline )); do
  observe_account_job || { echo 'Could not verify the owned system job.' >&2; exit 1; }
  account_new_lines=$(wc -l < "$account_stdout" | tr -d '[:space:]')
  if (( account_new_lines > account_seen_lines )); then
    sed -n "$((account_seen_lines+1)),${account_new_lines}p" "$account_stdout"
    account_seen_lines=$account_new_lines
  fi
  account_last_line=$(tail -n 1 "$account_stdout")
  if ! printf '%s\n' "$account_job_state" | grep -Eq '^[[:space:]]*pid = [0-9]+$' &&
     printf '%s\n' "$account_job_state" | grep -Eq '^[[:space:]]*last exit code = -?[0-9]+$'; then
    if printf '%s\n' "$account_job_state" | grep -Eq '^[[:space:]]*last exit code = 0$' &&
       validate_account_result "$account_last_line" 4294967295; then
      exit 0
    fi
    cat "$account_stderr" >&2
    echo 'The system-context account process exited without a passing result.' >&2
    exit 1
  fi
  sleep 1
done
echo 'The system-context account test did not complete; the disposable host must be discarded.' >&2
exit 1
