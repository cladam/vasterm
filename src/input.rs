use alacritty_terminal::term::TermMode;
use eframe::egui::{Event, ImeEvent, Key, Modifiers};

#[derive(Default)]
pub(crate) struct TerminalInput {
    pub preedit: String,
}

impl TerminalInput {
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
                    continue;
                }
                Event::Ime(ImeEvent::Preedit { text, .. }) => {
                    self.preedit.clone_from(text);
                    continue;
                }
                Event::Ime(ImeEvent::Commit(text)) => {
                    self.preedit.clear();
                    committed = Some(text);
                    text.as_bytes().to_vec()
                }
                Event::Text(text) if committed.take() == Some(text) => continue,
                Event::Text(text)
                    if self.preedit.is_empty()
                        && !modifiers.mac_cmd
                        && (!modifiers.ctrl || modifiers.alt) =>
                {
                    text.chars()
                        .filter(|c| !c.is_control())
                        .collect::<String>()
                        .into_bytes()
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers: key_modifiers,
                    ..
                } => {
                    modifiers = *key_modifiers;
                    committed = None;
                    if *pressed && self.preedit.is_empty() {
                        encode_key(*key, modifiers, mode)
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
}
