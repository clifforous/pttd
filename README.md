# pttd

`pttd` is a per-user push-to-talk daemon for the default PipeWire audio source. With the example configuration it starts muted, unmutes while F9 is held, and mutes when F9 is released. F10 toggles between push-to-talk mode and an open microphone.

## Build and automated verification

Run from the repository root:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
udevadm verify --resolve-names=never udev/70-pttd.rules
```

For a foreground source run, after configuring device access:

```sh
cargo run --locked
```

## Initial installation

### Verify device identity first

Do this before running any `sudo` command. Use Bash for the following command blocks in the same shell, and stop if any command or identity check fails. Capture the G815 and each currently attached supported mouse interface using these verified stable links; do not substitute transient `eventN` paths. At least one mouse interface must be present.

```sh
MOUSE_BY_ID=/dev/input/by-id/usb-Logitech_USB_Receiver-if02-event-mouse
MOUSE_USB_BY_ID=/dev/input/by-id/usb-Logitech_G502_X_LIGHTSPEED_C6EB16E1-if01-event-kbd
KEYBOARD_BY_ID=/dev/input/by-id/usb-Logitech_G815_RGB_MECHANICAL_GAMING_KEYBOARD_0B8032573031-event-kbd
BY_IDS=("$MOUSE_BY_ID" "$MOUSE_USB_BY_ID" "$KEYBOARD_BY_ID")
PTTD_LINKS=(/dev/input/pttd-mouse /dev/input/pttd-mouse-usb /dev/input/pttd-keyboard)
DEVICE_NODES=() DEVICE_SYS_PATHS=() DEVICE_LINKS=()
for i in "${!BY_IDS[@]}"; do
    by_id=${BY_IDS[$i]}
    if [ "$i" -lt 2 ] && [ ! -e "$by_id" ]; then
        continue
    fi
    node=$(readlink -f -- "$by_id")
    [ -c "$node" ]
    for previous in "${DEVICE_NODES[@]}"; do
        [ "$node" != "$previous" ]
    done
    udevadm info --attribute-walk --name="$by_id"
    devpath=$(udevadm info --query=path --name="$by_id")
    printf '%s -> %s (%s)\n' "$by_id" "$node" "$devpath"
    DEVICE_NODES+=("$node")
    DEVICE_SYS_PATHS+=("/sys$devpath")
    DEVICE_LINKS+=("${PTTD_LINKS[$i]}")
done
[ "${#DEVICE_NODES[@]}" -ge 2 ]
```

Confirm the resolved paths are distinct character devices. In each attribute walk, confirm the accepted `name` and `uniq` occur together on one input parent:

| Interface | `name` | `uniq` | pttd link |
| --- | --- | --- | --- |
| Wireless mouse (receiver if02 event-mouse) | `Logitech G502 X LS` | `c6-eb-16-e1` | `/dev/input/pttd-mouse` |
| Direct USB mouse (if01 event-kbd) | `Logitech G502 X LIGHTSPEED Keyboard` | `C6EB16E1` | `/dev/input/pttd-mouse-usb` |
| G815 keyboard | `Logitech G815 RGB MECHANICAL GAMING KEYBOARD` | `0B8032573031` | `/dev/input/pttd-keyboard` |

Stop if any captured pair does not match. The direct USB motion-only interface is named `Logitech G502 X LIGHTSPEED` without `Keyboard`; it cannot emit F9 and must not be selected.

### Install files

Build first. Install the executable before verifying the unit because verification resolves `%h/.local/bin/pttd`; a pre-install missing-executable failure is expected. Verify before installing or enabling the unit, then install the exact config, unit, and root-owned rule:

```sh
cargo build --release --locked
install -Dm755 target/release/pttd "$HOME/.local/bin/pttd"
systemd-analyze --user verify systemd/pttd.service
install -Dm644 examples/config.toml "$HOME/.config/pttd/config.toml"
install -Dm644 systemd/pttd.service "$HOME/.config/systemd/user/pttd.service"
sudo install -Dm644 udev/70-pttd.rules /etc/udev/rules.d/70-pttd.rules
```

The installed example config is exactly:

```toml
[input]
devices = ["/dev/input/pttd-mouse", "/dev/input/pttd-mouse-usb", "/dev/input/pttd-keyboard"]
ptt_key = "KEY_F9"
toggle_key = "KEY_F10"
```

The separate wireless and USB paths are intentional: the receiver can remain plugged in during direct USB use without switching one live alias between two devices. Each of the three configured paths has an independent reader. A disconnected mouse path may log retries without blocking connected devices, and reconnects independently. After a hardware transition and reader recovery, release any held key and use a fresh press.

Reload only the udev rules and add-process only the captured sysfs devices. The identity block prefixes each captured `/devices/...` DEVPATH with `/sys`. Never trigger all input events. If hardware changed since capture, repeat the identity block first:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --settle "${DEVICE_SYS_PATHS[@]}"
udevadm settle
for i in "${!DEVICE_NODES[@]}"; do
    node=${DEVICE_NODES[$i]}
    [ "$(readlink -f -- "${DEVICE_LINKS[$i]}")" = "$node" ]
    udevadm info --query=property --name="$node" | grep '^TAGS=.*:uaccess:'
    udevadm info --query=property --name="$node" | grep '^CURRENT_TAGS=.*:uaccess:'
    getfacl -cp "$node" | grep "^user:$USER:rw-"
    [ -r "$node" ] && [ -w "$node" ]
done
```

The link, tag/property, and ACL checks must pass for every captured node before starting the service. An absent mouse mode does not need a link or ACL until connected. The final read/write tests confirm the ACL is effective for the current user, not merely present in metadata.

Start the daemon for the current graphical session:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pttd.service
systemctl --user status pttd.service
journalctl --user --unit=pttd.service --follow
```

The unit is tied to `graphical-session.target`: it starts only as part of that user session and stops with it. Do not enable lingering for this service.

### Live acceptance

Keep `journalctl --user --unit=pttd.service --follow` visible and use `wpctl get-volume @DEFAULT_AUDIO_SOURCE@` to observe the default microphone. Confirm startup is muted. Verify that G502 F9 in each supported mouse mode and G815 F9 each unmute while held and remute when released. Verify that G815 F10 opens the microphone and a second G815 F10 returns to muted push-to-talk mode. Direct USB advertises F9/F10 capability, but capability alone does not prove emitted input or live acceptance; this profile does not claim G502 F10 operation in either mode.

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
4. Test wireless-to-USB and USB-to-wireless transitions with the receiver left plugged in. Repeat the identity and link/property/effective ACL checks for the attached mode(s), confirm reader recovery without daemon restart, then release any held G502 F9 and require a fresh press before verifying push-to-talk in the recovered mode. An inactive mouse reader retry must not prevent G815 input.

Check service status and the journal for device or audio errors after each case.

> **Security:** Access to the selected keyboard event nodes exposes their complete raw event streams to the logged-in user, not only F9 and F10. The rules deliberately grant access only to the verified device identities through `uaccess`.

## Updating

This update and rollback workflow applies only after the integration assets have been reviewed and committed into a clean known-good revision. Until such a revision exists, a failed first install or pre-commit change must use the complete first-install recovery and uninstall procedure below; Git rollback cannot restore untracked integration assets. Commits remain owner-controlled and none of these commands creates one.

Record the currently deployed, known-good Git revision before changing revisions. Repeat the exact-link identity setup in the current Bash shell before this transaction so the captured node, sysfs path, and link arrays describe the currently attached supported mouse mode(s) and G815. A candidate update is one complete transaction: start from a clean checkout, run verification and build, reinstall the exact binary, config, and unit, reinstall and reprocess the root rule only if that tracked asset changed, then reload, restart, and verify:

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
if ! git diff --quiet "$KNOWN_GOOD" HEAD -- udev/70-pttd.rules; then
    sudo install -Dm644 udev/70-pttd.rules /etc/udev/rules.d/70-pttd.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger --action=add --settle "${DEVICE_SYS_PATHS[@]}"
    udevadm settle
fi
systemctl --user daemon-reload
systemctl --user restart pttd.service
systemctl --user status pttd.service
journalctl --user --unit=pttd.service --since=-5min
```

Repeat the link, property, ACL, and live checks relevant to the changed assets. If the update fails, record the failed candidate revision, stop the service, require a clean tree, check out the recorded known-good revision, and repeat the entire install transaction from that checkout. Do not restore a mixture of old and new artifacts:

Restoring a rule without USB support can clear current `uaccess` without removing an existing named-user ACL. After rule reprocessing, the rollback below removes only the current user's residual ACL on the captured USB node if `CURRENT_TAGS` no longer grants `uaccess`, then checks that its retired link and named entry are absent. Historical `TAGS` can persist; they do not prove a current grant. Wireless/G815 nodes and USB nodes still granted current `uaccess` are left unchanged.

```sh
FAILED_REVISION=$(git rev-parse HEAD)
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
if ! git diff --quiet "$FAILED_REVISION" "$KNOWN_GOOD" -- udev/70-pttd.rules; then
    sudo install -Dm644 udev/70-pttd.rules /etc/udev/rules.d/70-pttd.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger --action=add --settle "${DEVICE_SYS_PATHS[@]}"
    udevadm settle
    for i in "${!DEVICE_NODES[@]}"; do
        [ "${DEVICE_LINKS[$i]}" = /dev/input/pttd-mouse-usb ] || continue
        node=${DEVICE_NODES[$i]}
        properties=$(udevadm info --query=property --name="$node") || exit 1
        if ! grep -q '^CURRENT_TAGS=.*:uaccess:' <<< "$properties"; then
            acl=$(getfacl -cp "$node") || exit 1
            if grep -q "^user:$USER:" <<< "$acl"; then
                sudo setfacl -x "u:$USER" "$node" || exit 1
            fi
            [ ! -e "${DEVICE_LINKS[$i]}" ] && [ ! -L "${DEVICE_LINKS[$i]}" ] || exit 1
            acl=$(getfacl -cp "$node") || exit 1
            ! grep -q "^user:$USER:" <<< "$acl" || exit 1
        fi
    done
fi
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

To uninstall, first repeat the exact-link identity procedure above so `DEVICE_NODES`, `DEVICE_SYS_PATHS`, and `DEVICE_LINKS` describe the G815 and currently attached supported mouse mode(s). Stop and disable the unit first, remove only pttd's files and rule, reload, then remove-process and add-process only the captured sysfs devices:

```sh
systemctl --user disable --now pttd.service
rm -f "$HOME/.config/systemd/user/pttd.service"
systemctl --user daemon-reload
rm -f "$HOME/.local/bin/pttd"
rm -f "$HOME/.config/pttd/config.toml"
sudo rm -f /etc/udev/rules.d/70-pttd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=remove --settle "${DEVICE_SYS_PATHS[@]}"
sudo udevadm trigger --action=add --settle "${DEVICE_SYS_PATHS[@]}"
udevadm settle
```

Inspect any residual per-user ACL entries after the targeted reprocessing. Remove only a remaining named entry for the current user:

```sh
for node in "${DEVICE_NODES[@]}"; do
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
[ ! -e /dev/input/pttd-mouse-usb ] && [ ! -L /dev/input/pttd-mouse-usb ]
[ ! -e /dev/input/pttd-keyboard ] && [ ! -L /dev/input/pttd-keyboard ]
for node in "${DEVICE_NODES[@]}"; do
    ! getfacl -cp "$node" | grep -q "^user:$USER:"
done
```

## Scope and portability

Repository source, a successful build, files copied into installation paths, and a service actually running in a graphical session are distinct states. A commit or successful automated check does not prove installation, startup, live acceptance, or deployment on another machine.

The binary, user service, and `/dev/input/pttd-*` configuration contract are generic. The tracked udev file is the verified hardware profile for one desktop. A laptop should inspect its real devices and provide host-specific same-parent udev matches and matching stable config links while reusing the daemon and service; this repository does not provide an installer or hypothetical laptop rules.

IPC and Noctalia integration are deferred and optional. They are not required to install or operate this slice.
