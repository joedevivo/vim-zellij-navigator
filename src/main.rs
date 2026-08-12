use zellij_tile::prelude::*;

use std::collections::{BTreeMap, VecDeque};
use std::str::FromStr;
use std::time::{Duration, Instant};

struct State {
    permissions_granted: bool,
    current_term_command: Option<String>,
    current_pane_id: Option<PaneId>,
    pane_manifest: Option<PaneManifest>,
    command_queue: VecDeque<Command>,
    // guards against escalating twice for one logical keypress: run_command
    // is fire-and-forget, so there's a window before Hammerspoon actually
    // moves OS focus where the pane title (what decides whether a keypress
    // reaches us at all) still looks unchanged. A fast repeat in that window
    // -- key-repeat, or a reflexive second tap -- would otherwise escalate
    // again from the still-current pane and overshoot by one window.
    last_escalation: Option<Instant>,

    // Configuration
    move_mod: Vec<Mod>,
    resize_mod: Vec<Mod>,
    use_arrow_keys: bool,
    hammerspoon_cli: String,
}

const ESCALATION_DEBOUNCE: Duration = Duration::from_millis(400);

// run_command's subprocess environment isn't guaranteed to carry the
// interactive shell's PATH, so this wants to be an absolute path. Default
// matches a Homebrew-installed `hs` on Apple Silicon; override via the
// `hammerspoon_cli` plugin configuration key for other install locations
// (e.g. Intel Homebrew's /usr/local/bin, or a nix-darwin-provided path).
const DEFAULT_HAMMERSPOON_CLI: &str = "/opt/homebrew/bin/hs";

enum Command {
    MoveFocus(Direction),
    MoveFocusOrTab(Direction),
    // Sent via `zellij pipe` (not the KDL keybind) by the neovim-side plugin
    // once nvim has already exhausted its own splits in this direction.
    // Skips the current_pane_is_vim() forward-to-nvim check below -- that's
    // exactly the case we're already past -- and goes straight to the
    // tiled-neighbor-or-escalate logic.
    MoveFocusFromEditor(Direction),
    MoveFocusOrTabFromEditor(Direction),
    Resize(Direction),
}

#[derive(Debug)]
enum Mod {
    Shift,
    Alt,
    Ctrl,
    Super,
    Hyper,
    Meta,
    CapsLock,
    NumLock,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.parse_configuration(configuration);

        request_permission(&[
            PermissionType::WriteToStdin,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadApplicationState,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::ListClients,
            EventType::PaneUpdate,
        ]);
        if self.permissions_granted {
            hide_self();
        }
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ListClients(list) => {
                let current = current_client(&list);
                self.current_term_command = current.and_then(|c| command_name(&c.running_command));
                self.current_pane_id = current.map(|c| c.pane_id);

                if !self.command_queue.is_empty() {
                    let command = self.command_queue.pop_front().unwrap();
                    self.execute_command(command);
                }
            }
            Event::PaneUpdate(manifest) => {
                self.pane_manifest = Some(manifest);
            }
            Event::PermissionRequestResult(permission) => {
                self.permissions_granted = match permission {
                    PermissionStatus::Granted => true,
                    PermissionStatus::Denied => false,
                };
                if self.permissions_granted {
                    hide_self();
                }
            }
            _ => {}
        }
        true
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if let Some(command) = parse_command(pipe_message) {
            self.handle_command(command);
        }
        true
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            permissions_granted: false,
            current_term_command: None,
            current_pane_id: None,
            pane_manifest: None,
            command_queue: VecDeque::new(),
            last_escalation: None,

            move_mod: vec![Mod::Ctrl],
            resize_mod: vec![Mod::Alt],
            use_arrow_keys: false,
            hammerspoon_cli: DEFAULT_HAMMERSPOON_CLI.to_string(),
        }
    }
}

impl State {
    fn handle_command(&mut self, command: Command) {
        self.command_queue.push_back(command);
        list_clients();
    }

    fn execute_command(&mut self, command: Command) {
        let forward_to_editor = self.current_pane_is_vim()
            && !matches!(
                command,
                Command::MoveFocusFromEditor(_) | Command::MoveFocusOrTabFromEditor(_)
            );
        if forward_to_editor {
            write_chars(&self.command_to_keybind(&command));
            return;
        }

        match command {
            Command::MoveFocus(direction) | Command::MoveFocusFromEditor(direction) => {
                self.move_focus_smart(direction, false)
            }
            Command::MoveFocusOrTab(direction) | Command::MoveFocusOrTabFromEditor(direction) => {
                self.move_focus_smart(direction, true)
            }
            Command::Resize(direction) => {
                resize_focused_pane_with_direction(Resize::Increase, direction)
            }
        }
    }

    /// Move focus if there's a tiled neighbor in `direction` on the current
    /// tab; if `try_tab` and there's another tab, fall back to zellij's own
    /// tab-cycling; otherwise there's truly nothing left in zellij, so hand
    /// off to Codex (via Hammerspoon) to move OS-level window focus instead.
    fn move_focus_smart(&mut self, direction: Direction, try_tab: bool) {
        if self.has_tiled_neighbor(direction) {
            move_focus(direction);
            return;
        }
        if try_tab && self.has_other_tab() {
            move_focus_or_tab(direction);
            return;
        }
        self.escalate_to_hammerspoon(direction);
    }

    /// Is there a selectable, non-floating pane adjacent to the currently
    /// focused pane, in `direction`, on the same tab? Standard tiling-WM
    /// directional-neighbor check: candidate's edge is at/past the focused
    /// pane's edge in `direction`, and their perpendicular-axis spans overlap.
    fn has_tiled_neighbor(&self, direction: Direction) -> bool {
        let Some(manifest) = &self.pane_manifest else { return false };
        let Some(current_id) = &self.current_pane_id else { return false };

        let focused = manifest
            .panes
            .values()
            .flatten()
            .find(|p| pane_id_matches(current_id, p));
        let Some(focused) = focused else { return false };

        let tab_panes = manifest
            .panes
            .values()
            .find(|panes| panes.iter().any(|p| pane_id_matches(current_id, p)));
        let Some(tab_panes) = tab_panes else { return false };

        let (fx0, fx1) = (focused.pane_x, focused.pane_x + focused.pane_columns);
        let (fy0, fy1) = (focused.pane_y, focused.pane_y + focused.pane_rows);

        tab_panes.iter().any(|p| {
            if pane_id_matches(current_id, p) { return false; }
            if p.is_floating || p.is_suppressed || !p.is_selectable { return false; }

            let (px0, px1) = (p.pane_x, p.pane_x + p.pane_columns);
            let (py0, py1) = (p.pane_y, p.pane_y + p.pane_rows);
            let vertical_overlap = py0 < fy1 && py1 > fy0;
            let horizontal_overlap = px0 < fx1 && px1 > fx0;

            match direction {
                Direction::Left => px1 <= fx0 && vertical_overlap,
                Direction::Right => px0 >= fx1 && vertical_overlap,
                Direction::Up => py1 <= fy0 && horizontal_overlap,
                Direction::Down => py0 >= fy1 && horizontal_overlap,
            }
        })
    }

    fn has_other_tab(&self) -> bool {
        self.pane_manifest.as_ref().is_some_and(|m| m.panes.len() > 1)
    }

    fn escalate_to_hammerspoon(&mut self, direction: Direction) {
        let now = Instant::now();
        if let Some(last) = self.last_escalation {
            if now.duration_since(last) < ESCALATION_DEBOUNCE {
                return;
            }
        }
        self.last_escalation = Some(now);

        let action = match direction {
            Direction::Left => "focus_left",
            Direction::Right => "focus_right",
            Direction::Up => "focus_up",
            Direction::Down => "focus_down",
        };
        let lua = format!("Codex.actions.actions().{}()", action);
        run_command(&[self.hammerspoon_cli.as_str(), "-c", &lua], BTreeMap::new());
    }

    fn current_pane_is_vim(&self) -> bool {
        if let Some(current_command) = &self.current_term_command {
            if current_command == "nvim" || current_command == "vim" {
                return true;
            }
        }
        false
    }

    fn parse_configuration(&mut self, configuration: BTreeMap<String, String>) {
        self.move_mod = configuration.get("move_mod").map_or(vec![Mod::Ctrl], |f| {
            Self::parse_modifiers(f).expect("Illegal modifier for move_mod")
        });
        self.resize_mod = configuration.get("resize_mod").map_or(vec![Mod::Alt], |f| {
            Self::parse_modifiers(f).expect("Illegal modifier for resize_mod")
        });
        self.use_arrow_keys = configuration
            .get("use_arrow_keys")
            .is_some_and(|v| v.to_lowercase() == "true");
        self.hammerspoon_cli = configuration
            .get("hammerspoon_cli")
            .cloned()
            .unwrap_or_else(|| DEFAULT_HAMMERSPOON_CLI.to_string());
    }

    fn parse_modifiers(input: &str) -> Result<Vec<Mod>, String> {
        input.split('+').map(|s| s.trim().parse::<Mod>()).collect()
    }

    fn command_to_keybind(&mut self, command: &Command) -> String {
        // MoveFocusFromEditor/MoveFocusOrTabFromEditor never reach here in
        // practice (execute_command routes them straight to move_focus_smart),
        // but are included for match exhaustiveness.
        let modifiers = match command {
            Command::MoveFocus(_)
            | Command::MoveFocusOrTab(_)
            | Command::MoveFocusFromEditor(_)
            | Command::MoveFocusOrTabFromEditor(_) => &self.move_mod,
            Command::Resize(_) => &self.resize_mod,
        };

        let direction = match command {
            Command::MoveFocus(direction)
            | Command::MoveFocusOrTab(direction)
            | Command::MoveFocusFromEditor(direction)
            | Command::MoveFocusOrTabFromEditor(direction)
            | Command::Resize(direction) => direction,
        };

        // Use the ASCII control characters for single modifier keybindings
        if modifiers.len() == 1 && !self.use_arrow_keys {
            match &modifiers[0] {
                Mod::Ctrl => return ctrl_keybinding(direction),
                Mod::Alt => return alt_keybinding(direction),
                _ => {}
            }
        }

        if self.use_arrow_keys {
            return arrow_kitty_keybinding(direction, modifiers);
        }

        kitty_keybinding(direction, modifiers)
    }
}

fn current_client(clients: &[ClientInfo]) -> Option<&ClientInfo> {
    clients.iter().find(|c| c.is_current_client)
}

fn command_name(running_command: &str) -> Option<String> {
    let command = running_command.split(' ').next()?;
    let command = command.split('/').next_back()?;
    Some(command.to_string())
}

fn pane_id_matches(pane_id: &PaneId, info: &PaneInfo) -> bool {
    match pane_id {
        PaneId::Terminal(id) => !info.is_plugin && *id == info.id,
        PaneId::Plugin(id) => info.is_plugin && *id == info.id,
    }
}

fn mod_to_kitty_protocol(modifier: &Mod) -> u8 {
    match modifier {
        Mod::Shift => 1,
        Mod::Alt => 2,
        Mod::Ctrl => 4,
        Mod::Super => 8,
        Mod::Hyper => 16,
        Mod::Meta => 32,
        Mod::CapsLock => 64,
        Mod::NumLock => 128,
    }
}

fn ctrl_keybinding(direction: &Direction) -> String {
    let direction = match direction {
        Direction::Left => "\u{0008}",
        Direction::Right => "\u{000C}",
        Direction::Up => "\u{000B}",
        Direction::Down => "\u{000A}",
    };
    direction.to_string()
}

fn alt_keybinding(direction: &Direction) -> String {
    let mut char_vec: Vec<char> = vec![0x1b as char];
    char_vec.push(match direction {
        Direction::Left => 'h',
        Direction::Right => 'l',
        Direction::Up => 'k',
        Direction::Down => 'j',
    });
    char_vec.iter().collect()
}

fn mods_to_kitty_protocol(modifiers: &[Mod]) -> String {
    let mut kitty_modifiers = 1;
    for modifier in modifiers {
        kitty_modifiers += mod_to_kitty_protocol(modifier);
    }
    format!("{}", kitty_modifiers)
}

fn arrow_kitty_keybinding(direction: &Direction, modifiers: &[Mod]) -> String {
    let key_code = match direction {
        Direction::Up => "A",
        Direction::Down => "B",
        Direction::Right => "C",
        Direction::Left => "D",
    };
    let mod_code = mods_to_kitty_protocol(modifiers);
    format!("\x1b\x5b1;{}{}", mod_code, key_code)
}

fn kitty_keybinding(direction: &Direction, modifiers: &[Mod]) -> String {
    let key_code = match direction {
        Direction::Left => "104",
        Direction::Right => "108",
        Direction::Up => "107",
        Direction::Down => "106",
    };

    let mod_code = mods_to_kitty_protocol(modifiers);

    format!("\x1b\x5b{};{}u", key_code, mod_code)
}

fn string_to_direction(s: &str) -> Option<Direction> {
    match s {
        "left" => Some(Direction::Left),
        "right" => Some(Direction::Right),
        "up" => Some(Direction::Up),
        "down" => Some(Direction::Down),
        _ => None,
    }
}

impl FromStr for Mod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "shift" => Ok(Mod::Shift),
            "alt" => Ok(Mod::Alt),
            "ctrl" => Ok(Mod::Ctrl),
            "super" => Ok(Mod::Super),
            "hyper" => Ok(Mod::Hyper),
            "meta" => Ok(Mod::Meta),
            "caps_lock" => Ok(Mod::CapsLock),
            "num_lock" => Ok(Mod::NumLock),
            _ => Err(format!("Invalid modifier: {}", s)),
        }
    }
}

fn parse_command(pipe_message: PipeMessage) -> Option<Command> {
    let payload = pipe_message.payload?;
    let command = pipe_message.name;

    let direction = string_to_direction(payload.as_str())?;

    match command.as_str() {
        "move_focus" => Some(Command::MoveFocus(direction)),
        "move_focus_or_tab" => Some(Command::MoveFocusOrTab(direction)),
        "move_focus_from_editor" => Some(Command::MoveFocusFromEditor(direction)),
        "move_focus_or_tab_from_editor" => Some(Command::MoveFocusOrTabFromEditor(direction)),
        "resize" => Some(Command::Resize(direction)),
        _ => None,
    }
}
