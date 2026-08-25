# pttd

`pttd` is a per-user push-to-talk daemon for the default PipeWire audio source. With the example configuration it starts muted, unmutes while F9 is held, and mutes when F9 is released. F10 toggles between push-to-talk mode and an open microphone.

## Build and automated verification

Run from the repository root:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
udevadm verify --resolve-names=never udev/desktop/70-pttd.rules udev/laptop/70-pttd.rules
```

For a foreground source run, after configuring device access:

```sh
cargo run --locked
```

## Initial installation

### Select this computer's profile

The hardware-specific udev rules are kept in separate profiles so changes made on one computer do not replace the rules for the other. Select the profile for the computer being configured and keep these variables in the current shell for the remaining commands:

```sh
PTTD_PROFILE=desktop # use laptop on the laptop
UDEV_RULE="udev/$PTTD_PROFILE/70-pttd.rules"
[ -f "$UDEV_RULE" ]
udevadm verify --resolve-names=never "$UDEV_RULE"
```

### Verify desktop device identity first

The identity and live-acceptance details below document the desktop profile. Do this before running any `sudo` command. Use the two verified stable links; do not substitute transient `eventN` paths. On the laptop, verify its devices against `udev/laptop/70-pttd.rules` before installation rather than using these desktop paths and identities.

```sh
MOUSE_BY_ID=/dev/input/by-id/usb-Logitech_USB_Receiver-if02-event-mouse
KEYBOARD_BY_ID=/dev/input/by-id/usb-Logitech_G815_RGB_MECHANICAL_GAMING_KEYBOARD_0B8032573031-event-kbd
MOUSE_NODE=$(readlink -f -- "$MOUSE_BY_ID")
KEYBOARD_NODE=$(readlink -f -- "$KEYBOARD_BY_ID")
[ -c "$MOUSE_NODE" ] && [ -c "$KEYBOARD_NODE" ]
[ "$MOUSE_NODE" != "$KEYBOARD_NODE" ]
udevadm info --attribute-walk --name="$MOUSE_BY_ID"
udevadm info --attribute-walk --name="$KEYBOARD_BY_ID"
MOUSE_DEVPATH=$(udevadm info --query=path --name="$MOUSE_BY_ID")
KEYBOARD_DEVPATH=$(udevadm info --query=path --name="$KEYBOARD_BY_ID")
printf 'mouse:    %s -> %s (%s)\n' "$MOUSE_BY_ID" "$MOUSE_NODE" "$MOUSE_DEVPATH"
printf 'keyboard: %s -> %s (%s)\n' "$KEYBOARD_BY_ID" "$KEYBOARD_NODE" "$KEYBOARD_DEVPATH"
```

Confirm the resolved paths are distinct character devices. In each attribute walk, confirm the accepted values occur together on one input parent: mouse `name` `Logitech G502 X LS` with `uniq` `c6-eb-16-e1`, and keyboard `name` `Logitech G815 RGB MECHANICAL GAMING KEYBOARD` with `uniq` `0B8032573031`. Stop if either pair does not match.

### Install files

Build first. Install the executable before verifying the unit because verification resolves `%h/.local/bin/pttd`; a pre-install missing-executable failure is expected. Verify before installing or enabling the unit, then install the exact config, unit, and root-owned rule:

```sh
cargo build --release --locked
install -Dm755 target/release/pttd "$HOME/.local/bin/pttd"
systemd-analyze --user verify systemd/pttd.service
install -Dm644 examples/config.toml "$HOME/.config/pttd/config.toml"
install -Dm644 systemd/pttd.service "$HOME/.config/systemd/user/pttd.service"
sudo install -Dm644 "$UDEV_RULE" /etc/udev/rules.d/70-pttd.rules
```

The installed example config is exactly:

```toml
[input]
devices = ["/dev/input/pttd-mouse", "/dev/input/pttd-keyboard"]
ptt_key = "KEY_F9"
toggle_key = "KEY_F10"
```

Reload only the udev rules and add-process only the two captured sysfs devices. `udevadm info --query=path` returns `/devices/...`, so prefix each captured DEVPATH with `/sys` when passing it to `udevadm trigger`:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --settle "/sys$MOUSE_DEVPATH" "/sys$KEYBOARD_DEVPATH"
udevadm settle
[ "$(readlink -f -- /dev/input/pttd-mouse)" = "$MOUSE_NODE" ]
[ "$(readlink -f -- /dev/input/pttd-keyboard)" = "$KEYBOARD_NODE" ]
for node in "$MOUSE_NODE" "$KEYBOARD_NODE"; do
    udevadm info --query=property --name="$node" | grep '^TAGS=.*:uaccess:'
    udevadm info --query=property --name="$node" | grep '^CURRENT_TAGS=.*:uaccess:'
    getfacl -cp "$node" | grep "^user:$USER:rw-"
    [ -r "$node" ] && [ -w "$node" ]
done
```

The tag/property and ACL checks must pass for both captured nodes before starting the service. The final read/write tests confirm the ACL is effective for the current user, not merely present in metadata.

Start the daemon for the current graphical session:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pttd.service
systemctl --user status pttd.service
journalctl --user --unit=pttd.service --follow
```

The unit is tied to `graphical-session.target`: it starts only as part of that user session and stops with it. Do not enable lingering for this service.

### Live acceptance

Keep `journalctl --user --unit=pttd.service --follow` visible and use `wpctl get-volume @DEFAULT_AUDIO_SOURCE@` to observe the default microphone. Confirm startup is muted. Verify that G502 F9 and G815 F9 each unmute while held and remute when released. Verify that G815 F10 opens the microphone and a second G815 F10 returns to muted push-to-talk mode; this desktop profile does not claim that the G502 emits F10.

Coordinate the following physical actions with the owner:

1. In push-to-talk mode, have the owner hold G502 F9 while the operator runs `systemctl --user stop pttd.service`. Confirm the graceful stop returns the microphone to mute before the unit becomes inactive, then start it again.
2. Record `OLD_PID=$(systemctl --user show --property=MainPID --value pttd.service)`. Have the owner hold G502 F9, run `systemctl --user kill --signal=KILL --kill-whom=main pttd.service`, and keep G502 F9 held through the automatic restart. Poll for at most 15 seconds until the unit is active with a different nonzero main PID:

   ```sh
   deadline=$((SECONDS + 15))
   NEW_PID=0
   while [ "$SECONDS" -lt "$deadline" ]; do
       ACTIVE_STATE=$(systemctl --user show --property=ActiveState --value pttd.service)
       NEW_PID=$(systemctl --user show --property=MainPID --value pttd.service)
       if [ "$ACTIVE_STATE" = active ] && [ "$NEW_PID" -ne 0 ] && [ "$NEW_PID" != "$OLD_PID" ]; then
           break
       fi
       sleep 0.1
   done
   [ "$ACTIVE_STATE" = active ] && [ "$NEW_PID" -ne 0 ] && [ "$NEW_PID" != "$OLD_PID" ]
   ```

   Only after that poll succeeds, confirm restart startup muted the microphone despite G502 F9 remaining held. Have the owner release G502 F9, then require a fresh G502 F9 press before it unmutes.
3. In push-to-talk mode, have the owner hold G815 F9 and physically disconnect that keyboard as the final active hold. Confirm the microphone mutes. Reconnect it, resolve the exact keyboard by-id link again into `RECONNECTED_KEYBOARD_NODE`, require a character device, and confirm `/dev/input/pttd-keyboard` resolves to it. Confirm the `uaccess` property and effective current-user read/write ACL return, the journal reports reader recovery without daemon restart, and fresh G815 F9 and G815 F10 input works.

Check service status and the journal for device or audio errors after each case.

> **Security:** Access to the selected keyboard event node exposes its complete raw event stream to the logged-in user, not only F9 and F10. The rule deliberately grants access only to the verified device identity through `uaccess`.

## Updating

This update and rollback workflow applies only after the integration assets have been reviewed and committed into a clean known-good revision. Until such a revision exists, a failed first install or pre-commit change must use the complete first-install recovery and uninstall procedure below; Git rollback cannot restore untracked integration assets. Commits remain owner-controlled and none of these commands creates one.

Record the currently deployed, known-good Git revision before changing revisions. Repeat the profile selection and exact-link identity setup in the current shell before this transaction so the rule, node, and DEVPATH variables are current. A candidate update is one complete transaction: start from a clean checkout, run verification and build, reinstall the exact binary, config, unit, and selected root rule, then reload, restart, and verify:

```sh
test -z "$(git status --porcelain)"
KNOWN_GOOD=$(git rev-parse HEAD)
printf 'known-good revision: %s\n' "$KNOWN_GOOD"
# Check out or update to the intended candidate, then continue from its clean tree.
test -z "$(git status --porcelain)"
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
install -Dm755 target/release/pttd "$HOME/.local/bin/pttd"
systemd-analyze --user verify systemd/pttd.service
install -Dm644 examples/config.toml "$HOME/.config/pttd/config.toml"
install -Dm644 systemd/pttd.service "$HOME/.config/systemd/user/pttd.service"
sudo install -Dm644 "$UDEV_RULE" /etc/udev/rules.d/70-pttd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --settle "/sys$MOUSE_DEVPATH" "/sys$KEYBOARD_DEVPATH"
udevadm settle
systemctl --user daemon-reload
systemctl --user restart pttd.service
systemctl --user status pttd.service
journalctl --user --unit=pttd.service --since=-5min
```

Repeat the link, property, ACL, and live checks relevant to the changed assets. If the update fails, stop the service, require a clean tree, check out the recorded known-good revision, and repeat the entire install transaction from that checkout. Do not restore a mixture of old and new artifacts:

```sh
systemctl --user stop pttd.service
test -z "$(git status --porcelain)"
git switch --detach "$KNOWN_GOOD"
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
install -Dm755 target/release/pttd "$HOME/.local/bin/pttd"
systemd-analyze --user verify systemd/pttd.service
install -Dm644 examples/config.toml "$HOME/.config/pttd/config.toml"
install -Dm644 systemd/pttd.service "$HOME/.config/systemd/user/pttd.service"
sudo install -Dm644 "$UDEV_RULE" /etc/udev/rules.d/70-pttd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --settle "/sys$MOUSE_DEVPATH" "/sys$KEYBOARD_DEVPATH"
udevadm settle
systemctl --user daemon-reload
systemctl --user start pttd.service
systemctl --user status pttd.service
journalctl --user --unit=pttd.service --since=-5min
```

## First-install recovery and uninstall

If first-install live acceptance fails, stop the service. A muted microphone is the safe state; do not unmute it merely because installation failed. If diagnosis indicates the microphone is not safely muted, explicitly mute it before continuing:

```sh
systemctl --user stop pttd.service
wpctl set-mute @DEFAULT_AUDIO_SOURCE@ 1
```

To uninstall, first repeat the exact-link identity procedure above so `MOUSE_NODE`, `KEYBOARD_NODE`, `MOUSE_DEVPATH`, and `KEYBOARD_DEVPATH` describe the current devices. Stop and disable the unit first, remove only pttd's files and rule, reload, then remove-process and add-process only the two captured sysfs devices:

```sh
systemctl --user disable --now pttd.service
rm -f "$HOME/.config/systemd/user/pttd.service"
systemctl --user daemon-reload
rm -f "$HOME/.local/bin/pttd"
rm -f "$HOME/.config/pttd/config.toml"
sudo rm -f /etc/udev/rules.d/70-pttd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=remove --settle "/sys$MOUSE_DEVPATH" "/sys$KEYBOARD_DEVPATH"
sudo udevadm trigger --action=add --settle "/sys$MOUSE_DEVPATH" "/sys$KEYBOARD_DEVPATH"
udevadm settle
```

Inspect any residual per-user ACL entries after the targeted reprocessing. Remove only a remaining named entry for the current user:

```sh
for node in "$MOUSE_NODE" "$KEYBOARD_NODE"; do
    if getfacl -cp "$node" | grep -q "^user:$USER:"; then
        sudo setfacl -x "u:$USER" "$node"
    fi
done
```

Do not use `setfacl -b`; it would erase unrelated ACLs. Finally, verify that the service, installed files, custom links, and named current-user ACL access are gone:

```sh
! systemctl --user is-active --quiet pttd.service
! systemctl --user is-enabled --quiet pttd.service
[ ! -e "$HOME/.config/systemd/user/pttd.service" ]
[ ! -e "$HOME/.local/bin/pttd" ]
[ ! -e "$HOME/.config/pttd/config.toml" ]
sudo test ! -e /etc/udev/rules.d/70-pttd.rules
[ ! -e /dev/input/pttd-mouse ] && [ ! -L /dev/input/pttd-mouse ]
[ ! -e /dev/input/pttd-keyboard ] && [ ! -L /dev/input/pttd-keyboard ]
for node in "$MOUSE_NODE" "$KEYBOARD_NODE"; do
    ! getfacl -cp "$node" | grep -q "^user:$USER:"
done
```

## Scope and portability

Repository source, a successful build, files copied into installation paths, and a service actually running in a graphical session are distinct states. A commit or successful automated check does not prove installation, startup, live acceptance, or deployment on another machine.

The binary, user service, and `/dev/input/pttd-*` configuration contract are generic. The tracked desktop and laptop udev profiles contain their respective hardware-specific matches while providing those same stable links. Keep computer-specific changes in the corresponding profile rather than replacing another computer's rules.

IPC and Noctalia integration are deferred and optional. They are not required to install or operate this slice.
