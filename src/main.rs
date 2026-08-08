use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use elgato_streamdeck::{DeviceStateUpdate, StreamDeck, list_devices, new_hidapi};
use serde::Deserialize;
use uinput::event::keyboard::Key;

/// The part of a profile used by this first increment.
///
/// Future versions can add key strokes and display settings without changing the
/// existing button mappings.
#[derive(Debug, Deserialize)]
struct Profile {
    #[serde(default)]
    buttons: Vec<Option<Button>>,
}

#[derive(Debug, Deserialize)]
struct Button {
    command: Option<String>,
    keys: Option<KeySequence>,
    profile: Option<PathBuf>,
    #[serde(default)]
    back: bool,
    icon: Option<PathBuf>,
    status: Option<Status>,
}

#[derive(Debug)]
struct KeySequence(Vec<Key>);

impl<'de> Deserialize<'de> for KeySequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let names = Vec::<String>::deserialize(deserializer)?;
        names
            .iter()
            .map(|name| parse_key(name).map_err(serde::de::Error::custom))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(Debug)]
struct Status {
    command: String,
    poll_seconds: u64,
    icons: StatusIcons,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StatusMode {
    #[default]
    String,
    Threshold,
}

#[derive(Debug)]
enum StatusIcons {
    String(BTreeMap<String, PathBuf>),
    Threshold {
        exact: BTreeMap<String, PathBuf>,
        thresholds: BTreeMap<i64, PathBuf>,
    },
}

impl StatusIcons {
    fn is_empty(&self) -> bool {
        match self {
            Self::String(icons) => icons.is_empty(),
            Self::Threshold { exact, thresholds } => exact.is_empty() && thresholds.is_empty(),
        }
    }

    fn icon_for(&self, status: &str) -> Option<&PathBuf> {
        match self {
            Self::String(icons) => icons.get(status),
            Self::Threshold { exact, thresholds } => exact.get(status).or_else(|| {
                status
                    .parse::<i64>()
                    .ok()
                    .and_then(|value| thresholds.range(..=value).next_back().map(|(_, icon)| icon))
            }),
        }
    }
}

#[derive(Deserialize)]
struct RawStatus {
    command: String,
    #[serde(default = "default_poll_seconds")]
    poll_seconds: u64,
    #[serde(default)]
    mode: StatusMode,
    icons: BTreeMap<String, PathBuf>,
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawStatus::deserialize(deserializer)?;
        let icons = match raw.mode {
            StatusMode::String => StatusIcons::String(raw.icons),
            StatusMode::Threshold => {
                let mut exact = BTreeMap::new();
                let mut thresholds = BTreeMap::new();
                for (key, icon) in raw.icons {
                    if let Ok(threshold) = key.parse::<i64>() {
                        thresholds.insert(threshold, icon);
                    } else {
                        exact.insert(key, icon);
                    }
                }
                StatusIcons::Threshold { exact, thresholds }
            }
        };
        Ok(Self {
            command: raw.command,
            poll_seconds: raw.poll_seconds,
            icons,
        })
    }
}

fn default_poll_seconds() -> u64 {
    1
}

const STATUS_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct StatusState {
    next_polls: BTreeMap<u8, Instant>,
    displayed_icons: BTreeMap<u8, DisplayedIcon>,
}

#[derive(Debug, PartialEq, Eq)]
enum DisplayedIcon {
    Base,
    Status(String),
}

impl StatusState {
    fn with_base_icons(profile: &Profile) -> Self {
        let displayed_icons = profile
            .buttons
            .iter()
            .enumerate()
            .filter_map(|(index, button)| {
                let button = button.as_ref()?;
                (button.status.is_some() && button.icon.is_some())
                    .then_some((index as u8, DisplayedIcon::Base))
            })
            .collect();
        Self {
            next_polls: BTreeMap::new(),
            displayed_icons,
        }
    }
}

impl Profile {
    fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("could not read profile {}", path.display()))?;
        serde_yaml::from_str(&contents)
            .with_context(|| format!("could not parse YAML profile {}", path.display()))
    }
}

fn main() -> Result<()> {
    let mut profile_path = profile_path()?;
    let mut profile = Profile::load(&profile_path)?;

    let hid = new_hidapi()?;
    let devices = list_devices(&hid);
    let (kind, serial) = devices
        .into_iter()
        .next()
        .context("no supported Stream Deck found")?;
    let key_count = kind.key_count();

    validate_profile(&profile, key_count)?;

    println!(
        "Loaded {} button binding(s) from {}. Connected to {kind} ({serial}).",
        profile.buttons.iter().flatten().count(),
        profile_path.display()
    );

    #[allow(clippy::arc_with_non_send_sync)]
    let deck = Arc::new(
        StreamDeck::connect(&hid, kind, &serial).context("could not connect to Stream Deck")?,
    );
    load_icons(&deck, &profile, &profile_path, false)?;
    let reader = deck.get_reader();
    let mut status_state = StatusState::with_base_icons(&profile);
    let mut keyboard = create_virtual_keyboard(&profile)?;
    let mut profile_history = Vec::new();

    loop {
        update_statuses(&deck, &profile, &profile_path, &mut status_state);

        for update in reader.read(Some(Duration::from_millis(100)))? {
            let DeviceStateUpdate::ButtonDown(button) = update else {
                continue;
            };
            let Some(binding) = profile
                .buttons
                .get(button as usize)
                .and_then(Option::as_ref)
            else {
                continue;
            };

            if let Some(target) = &binding.profile {
                match switch_profile(&deck, &profile_path, target, key_count, true) {
                    Ok((new_profile_path, new_profile, new_status_state, new_keyboard)) => {
                        profile_history.push(profile_path.clone());
                        profile_path = new_profile_path;
                        profile = new_profile;
                        status_state = new_status_state;
                        keyboard = new_keyboard;
                    }
                    Err(error) => eprintln!("Button {button}: could not switch profile: {error:#}"),
                }
                continue;
            }
            if binding.back {
                let Some(target) = profile_history.last().cloned() else {
                    continue;
                };
                let show_back_action = profile_history.len() > 1;
                match switch_profile(&deck, &profile_path, &target, key_count, show_back_action) {
                    Ok((new_profile_path, new_profile, new_status_state, new_keyboard)) => {
                        profile_history.pop();
                        profile_path = new_profile_path;
                        profile = new_profile;
                        status_state = new_status_state;
                        keyboard = new_keyboard;
                    }
                    Err(error) => {
                        eprintln!("Button {button}: could not return to profile: {error:#}")
                    }
                }
                continue;
            }
            if let Some(command) = &binding.command {
                run_command(button, command);
            }
            if let Some(keys) = &binding.keys
                && let Some(keyboard) = &mut keyboard
                && let Err(error) = send_keys(keyboard, keys)
            {
                eprintln!("Button {button}: could not send keys: {error:#}");
            }
        }
    }
}

fn switch_profile(
    deck: &StreamDeck,
    current_profile_path: &Path,
    target: &Path,
    key_count: u8,
    show_back_actions: bool,
) -> Result<(PathBuf, Profile, StatusState, Option<uinput::Device>)> {
    let profile_path = resolve_profile_path(current_profile_path, target);
    let profile = Profile::load(&profile_path)?;
    validate_profile(&profile, key_count)?;
    let keyboard = create_virtual_keyboard(&profile)?;
    load_icons(deck, &profile, &profile_path, show_back_actions)?;
    let status_state = StatusState::with_base_icons(&profile);

    println!(
        "Switched to {} button binding(s) from {}.",
        profile.buttons.iter().flatten().count(),
        profile_path.display()
    );
    Ok((profile_path, profile, status_state, keyboard))
}

fn validate_profile(profile: &Profile, key_count: u8) -> Result<()> {
    for (index, binding) in profile.buttons.iter().enumerate() {
        let Some(binding) = binding else {
            continue;
        };
        if index >= key_count as usize {
            bail!(
                "profile configures button {}, but the connected Stream Deck has only {key_count} buttons",
                index
            );
        }
        match (&binding.command, &binding.keys) {
            (Some(_), Some(_)) => bail!(
                "button {} configures both command and keys; choose exactly one action",
                index
            ),
            (None, None)
                if binding.profile.is_none() && !binding.back && binding.status.is_none() =>
            {
                bail!(
                    "button {} has no action or status; configure command, keys, profile, or status",
                    index
                )
            }
            _ => {}
        }
        if binding.profile.is_some() && (binding.command.is_some() || binding.keys.is_some()) {
            bail!(
                "button {} configures profile switching alongside command or keys; choose exactly one action",
                index
            );
        }
        if binding.back
            && (binding.profile.is_some() || binding.command.is_some() || binding.keys.is_some())
        {
            bail!(
                "button {} configures back navigation alongside another action; choose exactly one action",
                index
            );
        }
        if let Some(keys) = &binding.keys
            && keys.0.is_empty()
        {
            bail!("button {} has an empty keys action", index);
        }
        if let Some(status) = &binding.status {
            if status.poll_seconds == 0 {
                bail!("button {} has a status poll interval of zero", index);
            }
            if status.icons.is_empty() {
                bail!("button {} has no icons configured for its status", index);
            }
            if binding.icon.is_none() {
                bail!(
                    "button {} has status icons but no base icon for status-query failures",
                    index
                );
            }
        }
    }
    Ok(())
}

fn create_virtual_keyboard(profile: &Profile) -> Result<Option<uinput::Device>> {
    if !profile
        .buttons
        .iter()
        .flatten()
        .any(|button| button.keys.is_some())
    {
        return Ok(None);
    }

    uinput::default()
        .context("could not open /dev/uinput; load the uinput kernel module and grant this user access to the device")?
        .name("streamdeck_kb virtual keyboard")
        .context("could not name virtual keyboard")?
        .event(uinput::event::Keyboard::All)
        .context("could not configure virtual keyboard")?
        .create()
        .context("could not create virtual keyboard")
        .map(Some)
}

fn send_keys(keyboard: &mut uinput::Device, keys: &KeySequence) -> Result<()> {
    for key in &keys.0 {
        keyboard.press(key).context("could not press key")?;
    }
    keyboard
        .synchronize()
        .context("could not synchronize key press")?;

    for key in keys.0.iter().rev() {
        keyboard.release(key).context("could not release key")?;
    }
    keyboard
        .synchronize()
        .context("could not synchronize key release")?;
    Ok(())
}

fn parse_key(name: &str) -> Result<Key> {
    let trimmed = name.trim();
    let normalized = if trimmed.len() == 1 {
        trimmed.to_ascii_uppercase()
    } else {
        trimmed.to_ascii_uppercase().replace('-', "_")
    };
    let key = match normalized.as_str() {
        "CTRL" | "CONTROL" | "LEFTCTRL" | "LEFT_CONTROL" => Key::LeftControl,
        "RIGHTCTRL" | "RIGHT_CONTROL" => Key::RightControl,
        "ALT" | "LEFTALT" | "LEFT_ALT" => Key::LeftAlt,
        "RIGHTALT" | "RIGHT_ALT" | "ALTGR" => Key::RightAlt,
        "SHIFT" | "LEFTSHIFT" | "LEFT_SHIFT" => Key::LeftShift,
        "RIGHTSHIFT" | "RIGHT_SHIFT" => Key::RightShift,
        "META" | "WIN" | "LEFTMETA" | "LEFT_META" => Key::LeftMeta,
        "RIGHTMETA" | "RIGHT_META" => Key::RightMeta,
        "ESC" | "ESCAPE" => Key::Esc,
        "ENTER" | "RETURN" => Key::Enter,
        "TAB" => Key::Tab,
        "SPACE" => Key::Space,
        "BACKSPACE" => Key::BackSpace,
        "DELETE" | "DEL" => Key::Delete,
        "INSERT" | "INS" => Key::Insert,
        "HOME" => Key::Home,
        "END" => Key::End,
        "PAGEUP" | "PAGE_UP" => Key::PageUp,
        "PAGEDOWN" | "PAGE_DOWN" => Key::PageDown,
        "UP" => Key::Up,
        "DOWN" => Key::Down,
        "LEFT" => Key::Left,
        "RIGHT" => Key::Right,
        "`" | "GRAVE" | "BACKTICK" => Key::Grave,
        "-" | "MINUS" => Key::Minus,
        "=" | "EQUAL" => Key::Equal,
        "[" | "LEFTBRACKET" | "LEFT_BRACKET" => Key::LeftBrace,
        "]" | "RIGHTBRACKET" | "RIGHT_BRACKET" => Key::RightBrace,
        "\\" | "BACKSLASH" => Key::BackSlash,
        ";" | "SEMICOLON" | "SEMI_COLON" => Key::SemiColon,
        "'" | "APOSTROPHE" | "QUOTE" => Key::Apostrophe,
        "," | "COMMA" => Key::Comma,
        "." | "DOT" | "PERIOD" => Key::Dot,
        "/" | "SLASH" => Key::Slash,
        "A" => Key::A,
        "B" => Key::B,
        "C" => Key::C,
        "D" => Key::D,
        "E" => Key::E,
        "F" => Key::F,
        "G" => Key::G,
        "H" => Key::H,
        "I" => Key::I,
        "J" => Key::J,
        "K" => Key::K,
        "L" => Key::L,
        "M" => Key::M,
        "N" => Key::N,
        "O" => Key::O,
        "P" => Key::P,
        "Q" => Key::Q,
        "R" => Key::R,
        "S" => Key::S,
        "T" => Key::T,
        "U" => Key::U,
        "V" => Key::V,
        "W" => Key::W,
        "X" => Key::X,
        "Y" => Key::Y,
        "Z" => Key::Z,
        "0" => Key::_0,
        "1" => Key::_1,
        "2" => Key::_2,
        "3" => Key::_3,
        "4" => Key::_4,
        "5" => Key::_5,
        "6" => Key::_6,
        "7" => Key::_7,
        "8" => Key::_8,
        "9" => Key::_9,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "F13" => Key::F13,
        "F14" => Key::F14,
        "F15" => Key::F15,
        "F16" => Key::F16,
        "F17" => Key::F17,
        "F18" => Key::F18,
        "F19" => Key::F19,
        "F20" => Key::F20,
        "F21" => Key::F21,
        "F22" => Key::F22,
        "F23" => Key::F23,
        "F24" => Key::F24,
        _ => bail!("unsupported key {name:?}"),
    };
    Ok(key)
}

fn update_statuses(
    deck: &StreamDeck,
    profile: &Profile,
    profile_path: &Path,
    state: &mut StatusState,
) {
    let now = Instant::now();
    let mut changed = false;

    for (index, binding) in profile.buttons.iter().enumerate() {
        let Some(binding) = binding else {
            continue;
        };
        let button = index as u8;
        let Some(status_config) = &binding.status else {
            continue;
        };
        if state
            .next_polls
            .get(&button)
            .is_some_and(|next| *next > now)
        {
            continue;
        }
        let status_result = query_status(&status_config.command);
        // Schedule from the completion time. Otherwise a slow or timed-out command
        // becomes due again immediately and can starve button-event processing.
        state.next_polls.insert(
            button,
            Instant::now() + Duration::from_secs(status_config.poll_seconds),
        );

        let status = match status_result {
            Ok(status) => status,
            Err(error) => {
                eprintln!("Button {button}: status query failed: {error:#}");
                changed |= restore_base_icon(deck, button, binding, profile_path, state);
                continue;
            }
        };
        let Some(icon) = status_config.icons.icon_for(&status) else {
            eprintln!(
                "Button {}: status {:?} has no configured icon",
                button, status
            );
            changed |= restore_base_icon(deck, button, binding, profile_path, state);
            continue;
        };
        if state.displayed_icons.get(&button) == Some(&DisplayedIcon::Status(status.clone())) {
            continue;
        }

        if let Err(error) = set_button_icon(deck, button, icon, profile_path) {
            eprintln!("Button {}: could not update status icon: {error:#}", button);
            continue;
        }
        println!("Button {button} status: {status}");
        state
            .displayed_icons
            .insert(button, DisplayedIcon::Status(status));
        changed = true;
    }

    if changed && let Err(error) = deck.flush() {
        eprintln!("Could not send status icons to Stream Deck: {error}");
    }
}

fn restore_base_icon(
    deck: &StreamDeck,
    button: u8,
    binding: &Button,
    profile_path: &Path,
    state: &mut StatusState,
) -> bool {
    if state.displayed_icons.get(&button) == Some(&DisplayedIcon::Base) {
        return false;
    }

    let Some(icon) = &binding.icon else {
        return false;
    };
    if let Err(error) = set_button_icon(deck, button, icon, profile_path) {
        eprintln!(
            "Button {}: could not restore base icon after status failure: {error:#}",
            button
        );
        return false;
    }
    println!("Button {button}: restored base icon");
    state.displayed_icons.insert(button, DisplayedIcon::Base);
    true
}

fn query_status(command: &str) -> Result<String> {
    query_status_with_timeout(command, STATUS_COMMAND_TIMEOUT)
}

fn query_status_with_timeout(command: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {command:?}"))?;
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if !output.status.success() {
                bail!("{command:?} exited with {}", output.status);
            }
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{command:?} did not finish within {} seconds",
                timeout.as_secs_f32()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn load_icons(
    deck: &StreamDeck,
    profile: &Profile,
    profile_path: &Path,
    show_back_actions: bool,
) -> Result<()> {
    let mut icon_count = 0;

    deck.clear_all_button_images()
        .context("could not clear existing button icons from Stream Deck")?;

    for (index, binding) in profile.buttons.iter().enumerate() {
        let Some(binding) = binding else {
            continue;
        };
        if !should_load_button_icon(binding, show_back_actions) {
            continue;
        }
        let Some(icon) = &binding.icon else {
            continue;
        };
        set_button_icon(deck, index as u8, icon, profile_path)?;
        icon_count += 1;
    }

    deck.flush()
        .context("could not send button icons to Stream Deck")?;

    if icon_count > 0 {
        println!("Loaded {icon_count} button icon(s).");
    } else {
        println!("Cleared all button icons.");
    }
    Ok(())
}

fn should_load_button_icon(binding: &Button, show_back_actions: bool) -> bool {
    !binding.back || show_back_actions
}

fn set_button_icon(deck: &StreamDeck, button: u8, icon: &Path, profile_path: &Path) -> Result<()> {
    let path = resolve_profile_path(profile_path, icon);
    let image = image::open(&path).with_context(|| {
        format!(
            "could not load icon for button {button} from {}",
            path.display()
        )
    })?;
    deck.set_button_image(button, image)
        .with_context(|| format!("could not set icon for button {button}"))
}

/// Resolve asset paths against the profile's directory so profiles can be run
/// from any working directory.
fn resolve_profile_path(profile_path: &Path, asset_path: &Path) -> PathBuf {
    if asset_path.is_absolute() {
        asset_path.to_path_buf()
    } else {
        profile_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(asset_path)
    }
}

fn profile_path() -> Result<PathBuf> {
    let mut args = env::args_os();
    let executable = args.next().unwrap_or_default();
    let path = args.next().map(PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!("usage: {} <profile.yaml>", Path::new(&executable).display())
    })?;

    if args.next().is_some() {
        bail!("usage: {} <profile.yaml>", Path::new(&executable).display());
    }
    Ok(path)
}

fn run_command(button: u8, command: &str) {
    println!("Button {button}: {command}");
    if let Err(error) = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!("Button {button}: could not start command: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        Button, Key, KeySequence, Profile, Status, parse_key, query_status_with_timeout,
        should_load_button_icon, validate_profile,
    };
    use std::time::Duration;

    #[test]
    fn parses_button_commands() {
        let profile: Profile = serde_yaml::from_str(
            r#"
                buttons:
                  - command: "notify-send 'Hello'"
                    icon: icons/hello.png
                  -
                  - command: playerctl play-pause
                    status:
                      command: playerctl status
                      poll_seconds: 2
                      icons:
                        Playing: icons/playing.png
                        Paused: icons/paused.png
                  - keys: [CTRL, ALT, T]
            "#,
        )
        .unwrap();

        assert_eq!(profile.buttons.len(), 4);
        assert_eq!(
            profile.buttons[0].as_ref().unwrap().command.as_deref(),
            Some("notify-send 'Hello'")
        );
        assert_eq!(
            profile.buttons[0].as_ref().unwrap().icon,
            Some(PathBuf::from("icons/hello.png"))
        );
        assert!(profile.buttons[1].is_none());
        assert_eq!(
            profile.buttons[2].as_ref().unwrap().command.as_deref(),
            Some("playerctl play-pause")
        );
        let status = profile.buttons[2]
            .as_ref()
            .unwrap()
            .status
            .as_ref()
            .unwrap();
        assert_eq!(status.command, "playerctl status");
        assert_eq!(status.poll_seconds, 2);
        assert_eq!(
            status.icons.icon_for("Playing"),
            Some(&PathBuf::from("icons/playing.png"))
        );
        assert_eq!(
            profile.buttons[3]
                .as_ref()
                .unwrap()
                .keys
                .as_ref()
                .unwrap()
                .0,
            vec![Key::LeftControl, Key::LeftAlt, Key::T,]
        );
    }

    #[test]
    fn resolves_relative_icons_from_profile_directory() {
        assert_eq!(
            super::resolve_profile_path(
                Path::new("profiles/work.yaml"),
                Path::new("icons/play.png")
            ),
            PathBuf::from("profiles/icons/play.png")
        );
        assert_eq!(
            super::resolve_profile_path(
                Path::new("profiles/work.yaml"),
                Path::new("/tmp/play.png")
            ),
            PathBuf::from("/tmp/play.png")
        );
    }

    #[test]
    fn trims_status_command_output() {
        assert_eq!(
            super::query_status("printf 'Playing\\n'").unwrap(),
            "Playing"
        );
    }

    #[test]
    fn status_queries_time_out() {
        let error = query_status_with_timeout("sleep 1", Duration::from_millis(10))
            .expect_err("a slow status command should time out");
        assert!(error.to_string().contains("did not finish"));
    }

    #[test]
    fn rejects_non_success_status_commands() {
        assert!(super::query_status("printf 'Playing\\n'; exit 1").is_err());
    }

    #[test]
    fn status_buttons_require_a_base_icon() {
        let profile: Profile = serde_yaml::from_str(
            r#"
                buttons:
                  - command: playerctl play-pause
                    status:
                      command: playerctl status
                      icons:
                        Playing: icons/playing.png
            "#,
        )
        .unwrap();

        assert!(
            validate_profile(&profile, 15)
                .unwrap_err()
                .to_string()
                .contains("no base icon")
        );
    }

    #[test]
    fn status_buttons_can_omit_an_action() {
        let profile: Profile = serde_yaml::from_str(
            r#"
                buttons:
                  - icon: icons/status-unavailable.png
                    status:
                      command: playerctl status
                      icons:
                        Playing: icons/playing.png
            "#,
        )
        .unwrap();

        validate_profile(&profile, 15).unwrap();
        assert!(profile.buttons[0].as_ref().unwrap().command.is_none());
        assert!(profile.buttons[0].as_ref().unwrap().keys.is_none());
    }

    #[test]
    fn threshold_status_selects_the_highest_matching_threshold() {
        let status: Status = serde_yaml::from_str(
            r#"
                command: echo 0
                mode: threshold
                icons:
                  "3000": icons/three-thousand.png
                  "5000": icons/five-thousand.png
            "#,
        )
        .unwrap();

        assert_eq!(status.icons.icon_for("2999"), None);
        assert_eq!(
            status.icons.icon_for("3000"),
            Some(&PathBuf::from("icons/three-thousand.png"))
        );
        assert_eq!(
            status.icons.icon_for("4999"),
            Some(&PathBuf::from("icons/three-thousand.png"))
        );
        assert_eq!(
            status.icons.icon_for("5000"),
            Some(&PathBuf::from("icons/five-thousand.png"))
        );
        assert_eq!(status.icons.icon_for("not an integer"), None);
    }

    #[test]
    fn threshold_status_supports_exact_overrides() {
        let status: Status = serde_yaml::from_str(
            r#"
                command: echo 0
                mode: threshold
                icons:
                  MUTED: icons/muted.png
                  "1000": icons/low.png
            "#,
        )
        .unwrap();
        assert_eq!(
            status.icons.icon_for("MUTED"),
            Some(&PathBuf::from("icons/muted.png"))
        );
        assert_eq!(
            status.icons.icon_for("1000"),
            Some(&PathBuf::from("icons/low.png"))
        );
    }

    #[test]
    fn rejects_a_button_with_both_action_types() {
        let profile = Profile {
            buttons: vec![Some(Button {
                command: Some("echo command".to_owned()),
                keys: Some(KeySequence(vec![Key::LeftControl, Key::C])),
                profile: None,
                back: false,
                icon: None,
                status: None,
            })],
        };

        assert!(
            validate_profile(&profile, 15)
                .unwrap_err()
                .to_string()
                .contains("both command and keys")
        );
    }

    #[test]
    fn accepts_profile_switching_actions() {
        let profile: Profile = serde_yaml::from_str(
            r#"
                buttons:
                  - profile: function-keys-profile.yaml
            "#,
        )
        .unwrap();

        validate_profile(&profile, 15).unwrap();
        assert_eq!(
            profile.buttons[0].as_ref().unwrap().profile.as_deref(),
            Some(Path::new("function-keys-profile.yaml"))
        );
    }

    #[test]
    fn back_actions_are_hidden_without_a_calling_profile() {
        let profile: Profile = serde_yaml::from_str(
            r#"
                buttons:
                  - back: true
                    icon: icons/switch-back.png
            "#,
        )
        .unwrap();
        let button = profile.buttons[0].as_ref().unwrap();

        validate_profile(&profile, 15).unwrap();
        assert!(!should_load_button_icon(button, false));
        assert!(should_load_button_icon(button, true));
    }

    #[test]
    fn accepts_common_key_aliases() {
        assert_eq!(parse_key("ctrl").unwrap(), Key::LeftControl);
        assert_eq!(parse_key("ALTGR").unwrap(), Key::RightAlt);
        assert_eq!(parse_key("f12").unwrap(), Key::F12);
        assert!(parse_key("not-a-key").is_err());
    }

    #[test]
    fn accepts_unshifted_punctuation() {
        for (name, key) in [
            ("`", Key::Grave),
            (",", Key::Comma),
            (".", Key::Dot),
            ("/", Key::Slash),
            (";", Key::SemiColon),
            ("'", Key::Apostrophe),
            ("[", Key::LeftBrace),
            ("]", Key::RightBrace),
            ("-", Key::Minus),
            ("=", Key::Equal),
            ("\\", Key::BackSlash),
        ] {
            assert_eq!(parse_key(name).unwrap(), key);
        }
    }

    #[test]
    fn rejects_unknown_keys_while_loading_the_profile() {
        let error = serde_yaml::from_str::<Profile>(
            r#"
                buttons:
                  - keys: [NOT_A_KEY]
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unsupported key"));
    }
}
