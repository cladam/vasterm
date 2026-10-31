use alacritty_terminal::term::TermMode;
use eframe::egui::{Event, ImeEvent, Key, Modifiers};

#[derive(Default)]
pub(crate) struct TerminalInput {
    pub preedit: String,
    left_alt: bool,
}

impl TerminalInput {
    pub fn reset(&mut self) {
        self.preedit.clear();
        self.left_alt = false;
    }

    fn meta(&self, modifiers: Modifiers) -> bool {
        modifiers.alt && (!cfg!(target_os = "macos") || self.left_alt)
    }

    pub fn encode(
        &mut self,
        events: &[Event],
        mut modifiers: Modifiers,
        mode: TermMode,
    ) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        let mut committed = None;
        for event in events {
            let bytes = match event {
                Event::ModifiersChanged(value) => {
                    modifiers = *value;
                    if !modifiers.alt {
                        self.left_alt = false;
                    }
                    continue;
                }
                Event::WindowFocused(false) => {
                    self.reset();
                    continue;
                }
                Event::Ime(ImeEvent::Preedit { text, .. }) => {
                    self.preedit.clone_from(text);
                    continue;
                }
                Event::Ime(ImeEvent::Commit(text)) => {
                    let composing = !self.preedit.is_empty();
                    self.preedit.clear();
                    committed = Some(text);
                    if modifiers.mac_cmd {
                        continue;
                    }
                    encode_text(text, self.meta(modifiers) && !composing)
                }
                Event::Text(text) if committed.take() == Some(text) => continue,
                Event::Text(text)
                    if self.preedit.is_empty()
                        && !modifiers.mac_cmd
                        && (!modifiers.ctrl || (modifiers.alt && !self.meta(modifiers))) =>
                {
                    let text = text.chars().filter(|c| !c.is_control()).collect::<String>();
                    encode_text(&text, self.meta(modifiers))
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers: key_modifiers,
                    physical_key,
                    ..
                } => {
                    modifiers = *key_modifiers;
                    if physical_key.unwrap_or(*key) == Key::AltLeft {
                        self.left_alt = *pressed;
                    }
                    committed = None;
                    if *pressed && self.preedit.is_empty() {
                        let encoded_modifiers = Modifiers {
                            alt: self.meta(modifiers),
                            ..modifiers
                        };
                        encode_key(*key, encoded_modifiers, mode)
                    } else {
                        Vec::new()
                    }
                }
                _ => continue,
            };
            if !bytes.is_empty() {
                output.push(bytes);
            }
        }
        output
    }
}

fn encode_text(text: &str, meta: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(text.len() + usize::from(meta));
    if meta {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn encode_key(key: Key, modifiers: Modifiers, mode: TermMode) -> Vec<u8> {
    if modifiers.mac_cmd {
        return Vec::new();
    }
    let parameter =
        1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);
    let cursor = match key {
        Key::ArrowUp => Some('A'),
        Key::ArrowDown => Some('B'),
        Key::ArrowRight => Some('C'),
        Key::ArrowLeft => Some('D'),
        Key::Home => Some('H'),
        Key::End => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = cursor {
        return if parameter != 1 {
            format!("\x1b[1;{parameter}{final_byte}").into_bytes()
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        };
    }
    let tilde = match key {
        Key::Insert => Some(2),
        Key::Delete => Some(3),
        Key::PageUp => Some(5),
        Key::PageDown => Some(6),
        Key::F5 => Some(15),
        Key::F6 => Some(17),
        Key::F7 => Some(18),
        Key::F8 => Some(19),
        Key::F9 => Some(20),
        Key::F10 => Some(21),
        Key::F11 => Some(23),
        Key::F12 => Some(24),
        _ => None,
    };
    if let Some(number) = tilde {
        return if parameter == 1 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{parameter}~").into_bytes()
        };
    }
    let function = match key {
        Key::F1 => Some('P'),
        Key::F2 => Some('Q'),
        Key::F3 => Some('R'),
        Key::F4 => Some('S'),
        _ => None,
    };
    if let Some(final_byte) = function {
        return if parameter == 1 {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[1;{parameter}{final_byte}").into_bytes()
        };
    }
    let mut bytes = match key {
        Key::Enter if mode.contains(TermMode::LINE_FEED_NEW_LINE) => b"\r\n".to_vec(),
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![if modifiers.ctrl { 0x08 } else { 0x7f }],
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => vec![b'\t'],
        Key::Escape => vec![0x1b],
        _ if modifiers.ctrl => {
            let name = key.name().as_bytes();
            let control = match key {
                Key::Space | Key::Num2 => Some(0),
                Key::OpenBracket | Key::Num3 => Some(27),
                Key::Backslash | Key::Num4 => Some(28),
                Key::CloseBracket | Key::Num5 => Some(29),
                Key::Num6 => Some(30),
                Key::Minus | Key::Num7 => Some(31),
                Key::Num8 => Some(127),
                _ if name.len() == 1 && name[0].is_ascii_alphabetic() => {
                    Some(name[0].to_ascii_uppercase() - b'@')
                }
                _ => None,
            };
            control.into_iter().collect()
        }
        _ => Vec::new(),
    };
    if modifiers.alt && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn navigation_observes_application_cursor_and_modifiers() {
        for (key, normal, application) in [
            (Key::ArrowUp, "\x1b[A", "\x1bOA"),
            (Key::ArrowLeft, "\x1b[D", "\x1bOD"),
            (Key::Home, "\x1b[H", "\x1bOH"),
            (Key::End, "\x1b[F", "\x1bOF"),
        ] {
            assert_eq!(
                encode_key(key, Modifiers::NONE, TermMode::empty()),
                normal.as_bytes()
            );
            assert_eq!(
                encode_key(key, Modifiers::NONE, TermMode::APP_CURSOR),
                application.as_bytes()
            );
        }
        assert_eq!(
            encode_key(Key::ArrowUp, Modifiers::CTRL, TermMode::APP_CURSOR),
            b"\x1b[1;5A"
        );
        assert_eq!(
            encode_key(Key::Delete, Modifiers::SHIFT, TermMode::empty()),
            b"\x1b[3;2~"
        );
    }

    #[test]
    fn control_and_basic_keys_have_terminal_encodings() {
        for (key, expected) in [(Key::C, 3), (Key::D, 4), (Key::Space, 0)] {
            assert_eq!(
                encode_key(key, Modifiers::CTRL, TermMode::empty()),
                [expected]
            );
        }
        assert_eq!(
            encode_key(Key::Backspace, Modifiers::NONE, TermMode::empty()),
            [127]
        );
        assert_eq!(
            encode_key(Key::Tab, Modifiers::SHIFT, TermMode::empty()),
            b"\x1b[Z"
        );
        assert_eq!(
            encode_key(Key::Enter, Modifiers::NONE, TermMode::empty()),
            b"\r"
        );
        assert_eq!(
            encode_key(Key::Enter, Modifiers::NONE, TermMode::LINE_FEED_NEW_LINE),
            b"\r\n"
        );
    }

    #[test]
    fn text_and_ime_are_sent_once_not_as_printable_keys() {
        let mut input = TerminalInput::default();
        let bytes = input.encode(
            &[
                key(Key::A, Modifiers::NONE),
                Event::Text("a".into()),
                key(Key::Enter, Modifiers::NONE),
                Event::Text("\r\n".into()),
            ],
            Modifiers::NONE,
            TermMode::empty(),
        );
        assert_eq!(bytes, [b"a".to_vec(), b"\r".to_vec()]);
        assert!(
            input
                .encode(
                    &[
                        Event::Ime(ImeEvent::Preedit {
                            text: "candidate".into(),
                            active_range_chars: None,
                        }),
                        key(Key::Enter, Modifiers::NONE)
                    ],
                    Modifiers::NONE,
                    TermMode::empty()
                )
                .is_empty()
        );
        assert_eq!(
            input.encode(
                &[
                    Event::Ime(ImeEvent::Commit("é".into())),
                    Event::Text("é".into())
                ],
                Modifiers::NONE,
                TermMode::empty(),
            ),
            ["é".as_bytes()]
        );
    }

    #[test]
    fn command_shortcuts_and_control_text_do_not_leak() {
        let mut input = TerminalInput::default();
        assert!(encode_key(Key::C, Modifiers::MAC_CMD, TermMode::empty()).is_empty());
        assert_eq!(
            input.encode(
                &[key(Key::C, Modifiers::CTRL), Event::Text("c".into())],
                Modifiers::CTRL,
                TermMode::empty(),
            ),
            [vec![3]]
        );
    }

    #[test]
    fn modifier_changes_and_repeated_unicode_text_preserve_event_order() {
        let mut input = TerminalInput::default();
        assert_eq!(
            input.encode(
                &[
                    key(Key::A, Modifiers::NONE),
                    Event::Text("a".into()),
                    key(Key::C, Modifiers::CTRL),
                    Event::Text("c".into()),
                    Event::ModifiersChanged(Modifiers::NONE),
                    Event::Text("é".into()),
                    Event::Ime(ImeEvent::Commit("é".into())),
                    Event::Text("é".into()),
                    Event::Text("é".into()),
                ],
                Modifiers::CTRL,
                TermMode::empty(),
            ),
            [
                b"a".to_vec(),
                vec![3],
                "é".as_bytes().to_vec(),
                "é".as_bytes().to_vec(),
                "é".as_bytes().to_vec(),
            ],
        );
    }

    #[test]
    fn meta_text_is_prefixed_once_and_preserves_case_unicode_and_repeats() {
        let mut input = TerminalInput::default();
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            input.encode(
                &[
                    key(Key::AltLeft, Modifiers::NONE),
                    Event::ModifiersChanged(Modifiers::ALT),
                    key(Key::B, Modifiers::ALT),
                    Event::Text("b".into()),
                    key(Key::B, alt_shift),
                    Event::Text("B".into()),
                    key(Key::B, Modifiers::ALT),
                    Event::Text("b".into()),
                    Event::Text("界".into()),
                ],
                Modifiers::NONE,
                TermMode::empty(),
            ),
            [
                b"\x1bb".to_vec(),
                b"\x1bB".to_vec(),
                b"\x1bb".to_vec(),
                "\x1b界".as_bytes().to_vec()
            ],
        );
    }

    #[test]
    fn meta_control_keys_and_navigation_use_the_existing_encodings() {
        let mut input = TerminalInput::default();
        let control_alt = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            input.encode(
                &[
                    key(Key::AltLeft, Modifiers::ALT),
                    key(Key::C, control_alt),
                    Event::Text("c".into()),
                    key(Key::ArrowLeft, Modifiers::ALT),
                    key(Key::Backspace, Modifiers::ALT),
                ],
                Modifiers::NONE,
                TermMode::APP_CURSOR,
            ),
            [
                b"\x1b\x03".to_vec(),
                b"\x1b[1;3D".to_vec(),
                b"\x1b\x7f".to_vec()
            ]
        );
    }

    #[test]
    fn meta_ime_key_commits_are_prefixed_but_composition_is_not() {
        let mut input = TerminalInput::default();
        assert_eq!(
            input.encode(
                &[
                    key(Key::AltLeft, Modifiers::ALT),
                    Event::Ime(ImeEvent::Commit("b".into())),
                    Event::Text("b".into()),
                    Event::Ime(ImeEvent::Preedit {
                        text: "candidate".into(),
                        active_range_chars: None
                    }),
                    Event::Ime(ImeEvent::Commit("é".into())),
                    Event::Text("é".into()),
                ],
                Modifiers::NONE,
                TermMode::empty(),
            ),
            [b"\x1bb".to_vec(), "é".as_bytes().to_vec()]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn right_option_keeps_composed_text_without_a_meta_prefix() {
        let mut input = TerminalInput::default();
        assert_eq!(
            input.encode(
                &[
                    key(Key::AltRight, Modifiers::ALT),
                    key(Key::B, Modifiers::ALT),
                    Event::Text("∫".into()),
                    Event::Ime(ImeEvent::Preedit {
                        text: "´".into(),
                        active_range_chars: None
                    }),
                    Event::Ime(ImeEvent::Commit("é".into())),
                ],
                Modifiers::NONE,
                TermMode::empty(),
            ),
            ["∫".as_bytes().to_vec(), "é".as_bytes().to_vec()]
        );
    }

    #[test]
    fn releasing_meta_or_losing_focus_does_not_leave_it_stuck() {
        let mut input = TerminalInput::default();
        input.encode(
            &[key(Key::AltLeft, Modifiers::ALT)],
            Modifiers::NONE,
            TermMode::empty(),
        );
        let release = Event::Key {
            key: Key::AltLeft,
            physical_key: Some(Key::AltLeft),
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(
            input.encode(
                &[release, Event::Text("b".into())],
                Modifiers::NONE,
                TermMode::empty(),
            ),
            [b"b".to_vec()]
        );
        input.encode(
            &[key(Key::AltLeft, Modifiers::ALT)],
            Modifiers::NONE,
            TermMode::empty(),
        );
        input.reset();
        assert!(!input.left_alt);
        input.encode(
            &[
                key(Key::AltLeft, Modifiers::ALT),
                Event::WindowFocused(false),
            ],
            Modifiers::NONE,
            TermMode::empty(),
        );
        assert!(!input.left_alt);
    }
}
