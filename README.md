# streamdeck_kb

A small Linux Stream Deck command runner. This first increment loads a profile
and runs a shell command when a configured button is pressed. Buttons can also
display PNG icons.

```sh
cargo run -- example-profile.toml
```

Profiles are TOML files. Button numbers are zero-based; configured commands run
through `/bin/sh -c` and are started without waiting for completion:

```toml
[[buttons]]
index = 0
command = "notify-send 'Stream Deck' 'Button 0 pressed'"
icon = "icons/notification.png"

[[buttons]]
index = 1
command = "playerctl play-pause"
icon = "icons/play-pause.png"

[buttons.status]
command = "playerctl status"
poll_seconds = 1

[buttons.status.icons]
Playing = "icons/playing.png"
Paused = "icons/paused.png"

[[buttons]]
index = 2
keys = ["CTRL", "ALT", "T"]
icon = "icons/terminal.png"
```

`icon` is optional. It must name a PNG image; it may be any size because the
program resizes it to the connected Stream Deck's button resolution. Relative
paths are resolved relative to the profile file, not the current directory.
The included example profile references [notification.png](icons/notification.png)
as well as [play-pause.png](icons/play-pause.png), [playing.png](icons/playing.png),
and [paused.png](icons/paused.png), so it is ready to test as-is.

## Status icons

A button can query a command periodically and select an icon from the exact,
trimmed standard-output text. The `playerctl status` command emits `Playing` or
`Paused`, which the example maps to [playing.png](icons/playing.png) and
[paused.png](icons/paused.png). A status icon is updated only after its output
changes. `poll_seconds` defaults to `1` and must be greater than zero.

Status buttons must also define a base `icon`. It is restored whenever the
status command fails, or produces output that has no icon mapping. Only a
successful command's trimmed standard output is used to select the status icon.

Set `mode = "integer"` to treat the output as an integer and choose the icon at
the highest threshold less than or equal to it. The base icon is used below the
lowest threshold or when the output is not an integer:

```toml
[buttons.status]
command = "some-command-that-prints-a-number"
mode = "integer"

[buttons.status.icons]
3000 = "icons/three-thousand.png"
5000 = "icons/five-thousand.png"
```

Without `mode`, status matching stays in the default `string` mode and matches
the complete trimmed output exactly.

The example profile includes a KDE brightness gauge. It queries
`qdbus-qt6 org.kde.Solid.PowerManagement /org/kde/Solid/PowerManagement/Actions/BrightnessControl brightness`
and maps its `0`–`10000` result to gauges at every `1000` increment. Its action
is `true`, so pressing the button is a harmless no-op.

## Key bindings

`keys` sends a key chord through a Linux `/dev/uinput` virtual keyboard, which
is accepted by both X11 and Wayland. Keys are pressed in listed order and
released in reverse order. Common names include letters, digits, `F1`–`F24`,
`CTRL`, `ALT`, `SHIFT`, `META`, arrow keys, `ENTER`, `ESC`, `TAB`, and the
literal unshifted punctuation keys `` ` ``, `,`, `.`, `/`, `;`, `'`, `[`, `]`,
`-`, `=`, and `\\`.

The `uinput` kernel module must be loaded and the running user must have read
and write access to `/dev/uinput`. Each button must configure exactly one of
`command` or `keys`.

The program selects the first supported connected device and exits with a clear
error when no profile or Stream Deck is available.
