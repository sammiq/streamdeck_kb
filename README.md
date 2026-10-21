# streamdeck_kb

A small Linux Stream Deck command runner. This first increment loads a profile
and runs a shell command when a configured button is pressed. Buttons can also
display PNG icons.

```sh
cargo run -- example-profile.yaml
```

Pass `-v` / `--verbose` to log every status icon change. Status errors are
reported once per button until that button's status recovers.

Profiles are YAML files. Button numbers are zero-based; configured commands run
through `/bin/sh -c` and are started without waiting for completion:

```yaml
buttons:
  - command: "notify-send 'Stream Deck' 'Button 0 pressed'"
    icon: icons/notification.png

  - command: playerctl play-pause
    icon: icons/play-pause.png
    status:
      command: playerctl status
      poll_seconds: 1
      icons:
        Playing: icons/playing.png
        Paused: icons/paused.png

  - keys: [CTRL, ALT, T]
    icon: icons/terminal.png
```

List position selects the zero-based physical button: the first entry is button
`0`, the next is button `1`, and so on. Use `null` for an intentionally empty
button when you need to skip a position. `icon` is optional. It must name a PNG
image; it may be any size because the
program resizes it to the connected Stream Deck's button resolution. Relative
paths are resolved relative to the profile file, not the current directory.
The included example profile references [notification.png](icons/notification.png)
as well as [play-pause.png](icons/play-pause.png), [playing.png](icons/playing.png),
and [paused.png](icons/paused.png), so it is ready to test as-is.

## Profile switching

Set a button's `profile` action to switch immediately to another YAML profile.
The target path is resolved relative to the active profile. A switch button is
an action, so it cannot also define `command` or `keys`:

```yaml
buttons:
  - null # Button 0 is intentionally empty.
  - profile: function-keys-profile.yaml # Button 1 switches profile.
    icon: icons/switch-to-function-keys.png
```

Set `back: true` to return to the profile that opened the current one. It uses
a navigation stack, so nested profile switches return in reverse order. A back
button's icon is left blank when there is no calling profile, which makes a
profile reusable as either an entry point or a sub-profile:

```yaml
buttons:
  - back: true
    icon: icons/switch-back.png
```

The included example profile opens the function-key profile on physical button
`14`; that profile uses `back: true` on button `14` to return.

## Status icons

A button can query a command periodically and select an icon from the exact,
trimmed standard-output text. The `playerctl status` command emits `Playing` or
`Paused`, which the example maps to [playing.png](icons/playing.png) and
[paused.png](icons/paused.png). A status icon is updated only after its output
changes. `poll_seconds` defaults to `1` and must be greater than zero.

Status buttons must also define a base `icon`. It is restored whenever the
status command fails, or produces output that has no icon mapping. Only a
successful command's trimmed standard output is used to select the status icon.

Set `mode = "threshold"` to treat the output as an integer and choose the icon at
the highest threshold less than or equal to it. The base icon is used below the
lowest threshold or when the output is not an integer:

```yaml
status:
  command: some-command-that-prints-a-number
  mode: threshold
  icons:
    "3000": icons/three-thousand.png
    "5000": icons/five-thousand.png
```

Without `mode`, status matching stays in the default `string` mode and matches
the complete trimmed output exactly.

Threshold mode also supports exact string overrides in the same table; exact
matches win before numeric thresholds, so `MUTED = "icons/volume-muted.png"`
can sit alongside `1000 = "icons/volume-1000.png"`.

The example profile includes a KDE brightness gauge. It queries
`qdbus-qt6 org.kde.Solid.PowerManagement /org/kde/Solid/PowerManagement/Actions/BrightnessControl brightness`
and maps its `0`–`10000` result to gauges at every `1000` increment. Its action
is omitted, so pressing the button is a harmless no-op. A button with a
`status` section may omit both `command` and `keys` when it is display-only.

The example also includes a PipeWire volume test button. It toggles mute on the
default audio sink and uses `wpctl get-volume` plus `awk` to map its fractional
volume into the same `0`–`10000` gauge range, or emit the `MUTED` override.

## Key bindings

`keys` sends a key chord through a Linux `/dev/uinput` virtual keyboard, which
is accepted by both X11 and Wayland. Keys are pressed in listed order and
released in reverse order. Common names include letters, digits, `F1`–`F24`,
`CTRL`, `ALT`, `SHIFT`, `META`, arrow keys, `ENTER`, `ESC`, `TAB`, and the
literal unshifted punctuation keys `` ` ``, `,`, `.`, `/`, `;`, `'`, `[`, `]`,
`-`, `=`, and `\\`.

The `uinput` kernel module must be loaded and the running user must have read
and write access to `/dev/uinput`. Each button may configure one action
(`command`, `keys`, `profile`, or `back`), or be a display-only status button
with neither action.

The program selects the first supported connected device and exits with a clear
error when no profile or Stream Deck is available.
