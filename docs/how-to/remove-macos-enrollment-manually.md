# Remove a macOS enrollment manually

Use this recovery procedure when the Glass app was removed before its enrollment was cleaned up.
For ordinary removal while Glass is installed, use [Manage the macOS environment](manage-macos-environment.md#return-to-shared-desktop-use).

If Glass is available, use the supported normal-user command first:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp unenroll
```

It completes workspace recovery before privileged registration removal.

Use this procedure if the app bundle has already been removed and `glass-mcp unenroll` is unavailable.
It removes the account's Glass worker and your enrollment while retaining the development account,
home, privacy grants and normal HTTP/menu-bar startup configuration. Administrator authorisation is
needed for the worker and enrollment files.

Manual file removal cannot restore unfinished workspace permission changes. Preserve the permission
journal and enrollment record until step 4 confirms recovery has finished. If recovery is pending,
restore a compatible Glass app at its original path and run `unenroll` instead.

Run these commands in the same Terminal window, from the normal account that enrolled Glass.
Stop if a command fails, except for the explicitly described already-absent jobs or files.
Keep other Glass setup, unenrollment and uninstall operations stopped during this procedure.

1. **Stop Glass work and identify your enrollment.** Disconnect Glass in MCP clients so they cannot
   restart it during cleanup, and quit the normal account's Glass menu-bar app. Save and close the
   development applications in the selected account; log that account out if needed to stop its work.
   Return to your normal account before continuing.

   ```bash
   glass_owner_uid="$(id -u)"
   printf 'Normal account UID: %s\n' "$glass_owner_uid"
   glass_enrollment_root="/Library/Application Support/Glass/accounts/$glass_owner_uid"
   sudo /usr/libexec/PlistBuddy -c Print "$glass_enrollment_root/account.plist"
   ```

   Only if that record is absent, try the earlier preview location:

   ```bash
   glass_enrollment_root="/Library/Application Support/Glass"
   sudo /usr/libexec/PlistBuddy -c Print "$glass_enrollment_root/account.plist"
   ```

   Confirm `owner_uid` equals the UID printed by `id -u`. Check that `name`, `uid`, `record_uuid` and
   `home` still identify your chosen development account. If the record is missing or incomplete,
   stop rather than guessing which account or files to remove.

   Check the current account identity against that record:

   ```bash
   glass_development_account="$(sudo /usr/libexec/PlistBuddy -c 'Print :name' "$glass_enrollment_root/account.plist")"
   /usr/bin/dscl /Local/Default -read "/Users/$glass_development_account" UniqueID GeneratedUID NFSHomeDirectory
   ```

   `UniqueID`, `GeneratedUID` and `NFSHomeDirectory` must match `uid`, `record_uuid` and `home` in the
   enrollment. Stop if the account has been deleted or its identity has changed.

   If both layouts contain your records, complete the procedure for the per-user directory first,
   then repeat it for the older location. Inspect `owner_uid` each time; leave records belonging to
   other users intact.

2. **Check the worker registration.** Read its location from the enrollment record:

   ```bash
   glass_development_uid="$(sudo /usr/libexec/PlistBuddy -c 'Print :uid' "$glass_enrollment_root/account.plist")"
   glass_development_home="$(sudo /usr/libexec/PlistBuddy -c 'Print :home' "$glass_enrollment_root/account.plist")"
   glass_worker_plist="$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.worker.plist"
   sudo /usr/bin/plutil -p "$glass_worker_plist"
   sudo ls -la "$glass_enrollment_root"
   ```

   The worker's arguments must name your Glass executable, `macos-worker`, and your normal account's
   UID; its socket path must be `$glass_enrollment_root/worker.sock`. Older preview jobs omit the UID
   argument and use the older enrollment root. If the plist is already absent, confirm in step 3
   that its job is also absent. Stop if the job points to another enrollment or the enrollment
   directory contains unexpected files. Its expected entries are `account.plist`, `worker.requirement`,
   and possibly `disabled`, `worker.sock` and `tech.fixedwidth.glass.onboarding.plist`. If that permission
   helper record exists, verify its copy in the development account too:

   ```bash
   glass_onboarding_plist="$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.onboarding.plist"
   sudo /usr/bin/plutil -p "$glass_enrollment_root/tech.fixedwidth.glass.onboarding.plist"
   sudo /usr/bin/plutil -p "$glass_onboarding_plist"
   sudo /usr/bin/cmp "$glass_enrollment_root/tech.fixedwidth.glass.onboarding.plist" "$glass_onboarding_plist"
   ```

   They must match and name the same Glass executable, `macos-onboarding`, and your normal account's
   UID. If a copy is missing, differs or points elsewhere, restore Glass and use `unenroll` to check
   the interrupted registration. The older shared root may also contain the `accounts`
   directory for newer registrations; leave that directory intact. The enrollment root must be a
   root-owned directory, not a symbolic link; its metadata files must be regular root-owned files
   with a single link. Stop on unexpected ownership, types or permission errors.

3. **Disable the worker and remove its startup job.** The marker prevents a remaining worker from
   accepting new work while cleanup is incomplete:

   ```bash
   glass_disabled_marker="$(mktemp -t glass-unenroll)" &&
       printf '%s\n' 'Glass account registration removed' > "$glass_disabled_marker" &&
       sudo /usr/bin/install -S -o root -g wheel -m 644 "$glass_disabled_marker" "$glass_enrollment_root/disabled" &&
       rm "$glass_disabled_marker" &&
       /bin/sync
   ```

   This installs the marker atomically and flushes it before the worker is unloaded. After that
   block succeeds:

   ```bash
   sudo /bin/launchctl bootout "gui/$glass_development_uid/tech.fixedwidth.glass.worker"
   sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.worker"
   ```

   `bootout` may report that the job or GUI domain is already absent. The final `print` must report
   that it cannot find the job/domain; if it still prints a loaded job, stop and resolve that first.
   An authentication or permission error is not confirmation that the job is gone. Then remove only
   this worker's persistent plist:

   ```bash
   sudo rm -f "$glass_worker_plist"
   ```

   If step 2 verified a permission helper, unload it too:

   ```bash
   sudo /bin/launchctl bootout "gui/$glass_development_uid/tech.fixedwidth.glass.onboarding"
   sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.onboarding"
   ```

   Apply the same absence/error checks before removing its verified persistent plist:

   ```bash
   sudo rm -f "$glass_onboarding_plist"
   ```

   Confirm the worker and development applications have exited before removing enrollment metadata.
   Unloading a job alone does not prove that all applications it launched have stopped.

4. **Check permission recovery before deleting the enrollment record.** In your normal account:

   ```bash
   glass_journal="$HOME/Library/Application Support/glass/macos/permission-recovery/pending.jsonl"
   ls -l "$glass_journal"
   if [ -s "$glass_journal" ]; then
       tail -n 1 "$glass_journal"
   fi
   ```

   Continue only if the journal is absent or empty, or its final record is `{"kind":"Closed"}`.
   A `No such file or directory` message is expected if no journal was created. Permission and I/O
   errors do not establish that the journal is absent; stop if it cannot be inspected.
   Otherwise, leave the disabled enrollment and journal in place. Restore Glass at its original
   path and run `unenroll` to replay the recorded permission changes. Removing the worker plist
   in step 3 does not prevent that recovery. Keep the journal; Glass needs its recorded filesystem
   identities to restore the correct objects, including after renames.

5. **Remove only the verified enrollment files.** For the per-user layout, move the directory out
   of the active location before deleting its contents. The commands stop if a previous retired
   directory already exists:

   ```bash
   glass_retired_root="/Library/Application Support/Glass/accounts/.unenroll-$glass_owner_uid"
   test "$glass_enrollment_root" = "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
       sudo /bin/test ! -e "$glass_retired_root" &&
       sudo /bin/test ! -L "$glass_retired_root" &&
       sudo rm -f "$glass_enrollment_root/worker.sock" &&
       sudo mv "$glass_enrollment_root" "$glass_retired_root" &&
       /bin/sync &&
       sudo rm -f "$glass_retired_root/worker.requirement" \
           "$glass_retired_root/disabled" "$glass_retired_root/account.plist" \
           "$glass_retired_root/tech.fixedwidth.glass.onboarding.plist" &&
       sudo rmdir "$glass_retired_root" &&
       /bin/sync
   ```

   For the older layout, remove its known files in place; its directory may also hold other users'
   registrations:

   ```bash
   test "$glass_enrollment_root" = "/Library/Application Support/Glass" &&
       sudo rm -f "$glass_enrollment_root/worker.sock" \
           "$glass_enrollment_root/worker.requirement" "$glass_enrollment_root/disabled" &&
       sudo rm "$glass_enrollment_root/account.plist" &&
       /bin/sync
   ```

   Clear this normal user's pending setup request, if present, so reopening Glass does not offer
   to continue it. This removes only setup metadata; keep the permission-recovery journal:

   ```bash
   rm -f "$HOME/Library/Application Support/glass/macos/permission-recovery/pending-setup.json"
   ```

   Leave `/Library/Application Support/Glass`, its `accounts` directory, lock files and other users'
   registrations in place. Check for an older record belonging to you as described in step 1, then
   restart your MCP clients or normal Glass integration when the app is available again.

If interrupted after the per-user directory was moved, its original path is absent and
`accounts/.unenroll-<your UID>` may remain. The work and journal checks above must already have
completed. Confirm the original path is still absent and inspect the retired directory:

```bash
glass_owner_uid="$(id -u)"
glass_retired_root="/Library/Application Support/Glass/accounts/.unenroll-$glass_owner_uid"
sudo /bin/test ! -e "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "$glass_retired_root" &&
    sudo ls -la "$glass_retired_root"
```

Continue only if it is a root-owned directory containing just the remaining `account.plist`,
`worker.requirement`, `disabled`, and verified `tech.fixedwidth.glass.onboarding.plist` files. Then
remove those known files and the empty directory:

```bash
sudo /bin/test ! -e "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "$glass_retired_root" &&
    sudo rm -f "$glass_retired_root/worker.requirement" \
        "$glass_retired_root/disabled" "$glass_retired_root/account.plist" \
        "$glass_retired_root/tech.fixedwidth.glass.onboarding.plist" &&
    sudo rmdir "$glass_retired_root" &&
    /bin/sync
```

If a new enrollment exists or unexpected files remain, stop and inspect them. Keep the permission
journal, and leave other directories intact.
