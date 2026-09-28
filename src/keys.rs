//! Cross-platform parser for the `send-keys` mini-language.
//!
//! ```text
//! plain text        typed as Unicode text ("\n" → ENTER, "\t" → TAB)
//! {NAME}            special key: ENTER, TAB, ESC, SPACE, BACKSPACE, DELETE, INSERT,
//!                   HOME, END, PGUP, PGDN, UP, DOWN, LEFT, RIGHT, F1–F24,
//!                   CAPSLOCK, PRINTSCREEN, MENU, PLUS, MINUS, LBRACE, RBRACE
//! {MOD+…+KEY}       chord, e.g. {CTRL+A}, {CTRL+SHIFT+T}, {ALT+F4}, {WIN+D}
//!                   modifiers: CTRL, ALT (OPTION), SHIFT, WIN (SUPER, META, CMD),
//!                   MOD (= Cmd on macOS, Ctrl elsewhere)
//! {WIN}             a lone modifier is pressed and released
//! {NAME N}          repeat N times (1–100), e.g. {TAB 3}
//! {{ and }}         literal braces
//! ```
//! Names are case-insensitive. Parsing happens before anything is sent, so an
//! invalid sequence never results in a half-typed string.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    /// Windows key / Super / Command.
    Meta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    CapsLock,
    PrintScreen,
    Menu,
    /// Function key F1–F24.
    F(u8),
    /// A printable character key (ASCII letters are stored lowercase).
    Char(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Modifiers held down while `key` is pressed, in press order.
    pub modifiers: Vec<Modifier>,
    /// `None` for a lone modifier press such as `{WIN}`.
    pub key: Option<Key>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStep {
    /// Literal text, typed as Unicode characters.
    Text(String),
    /// A special key or key combination.
    Chord(Chord),
}

const MAX_REPEAT: u32 = 100;

/// Parse a send-keys string into steps. Returns a human-readable error.
pub fn parse(input: &str) -> Result<Vec<KeyStep>, String> {
    let mut steps = Vec::new();
    let mut text = String::new();
    let mut chars = input.chars().peekable();

    fn flush(text: &mut String, steps: &mut Vec<KeyStep>) {
        if !text.is_empty() {
            steps.push(KeyStep::Text(std::mem::take(text)));
        }
    }
    fn key_step(key: Key) -> KeyStep {
        KeyStep::Chord(Chord {
            modifiers: vec![],
            key: Some(key),
        })
    }

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '{' => {
                let mut inner = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(c2);
                }
                if !closed {
                    return Err(
                        "Unclosed '{' in key sequence (use '{{' for a literal brace)".into(),
                    );
                }
                flush(&mut text, &mut steps);
                let (chord, repeat) = parse_group(&inner)?;
                for _ in 0..repeat {
                    steps.push(KeyStep::Chord(chord.clone()));
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                text.push('}');
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                flush(&mut text, &mut steps);
                steps.push(key_step(Key::Enter));
            }
            '\n' => {
                flush(&mut text, &mut steps);
                steps.push(key_step(Key::Enter));
            }
            '\t' => {
                flush(&mut text, &mut steps);
                steps.push(key_step(Key::Tab));
            }
            _ => text.push(c),
        }
    }
    flush(&mut text, &mut steps);
    Ok(steps)
}

fn parse_group(inner: &str) -> Result<(Chord, u32), String> {
    let inner = inner.trim();
    if inner.is_empty() {
        return Err("Empty '{}' in key sequence".into());
    }

    // Optional repeat count: "{TAB 3}"
    let (name, repeat) = match inner.rsplit_once(' ') {
        Some((name, count)) => {
            let n: u32 = count
                .trim()
                .parse()
                .map_err(|_| format!("Invalid repeat count in '{{{inner}}}'"))?;
            if n == 0 || n > MAX_REPEAT {
                return Err(format!("Repeat count must be between 1 and {MAX_REPEAT}"));
            }
            (name.trim(), n)
        }
        None => (inner, 1),
    };

    // Split "CTRL+SHIFT+T" into modifiers + key, allowing '+' itself as a key.
    let (mod_part, key_part) = if name == "+" {
        ("", "+")
    } else if let Some(prefix) = name.strip_suffix("++") {
        (prefix, "+")
    } else {
        match name.rsplit_once('+') {
            Some((prefix, key)) => (prefix, key),
            None => ("", name),
        }
    };

    let mut modifiers = Vec::new();
    if !mod_part.is_empty() {
        for part in mod_part.split('+') {
            let m = parse_modifier(part)
                .ok_or_else(|| format!("Unknown modifier '{part}' in '{{{inner}}}'"))?;
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }
    }

    if key_part.is_empty() {
        return Err(format!("Missing key in '{{{inner}}}'"));
    }

    let key = match parse_key(key_part) {
        Some(k) => Some(k),
        None => match parse_modifier(key_part) {
            // "{WIN}" or "{CTRL+SHIFT}" — press and release the modifiers.
            Some(m) => {
                if !modifiers.contains(&m) {
                    modifiers.push(m);
                }
                None
            }
            None => return Err(format!("Unknown key '{key_part}' in '{{{inner}}}'")),
        },
    };

    Ok((Chord { modifiers, key }, repeat))
}

fn parse_modifier(s: &str) -> Option<Modifier> {
    match s.trim().to_ascii_uppercase().as_str() {
        "CTRL" | "CONTROL" => Some(Modifier::Ctrl),
        "ALT" | "OPTION" | "OPT" => Some(Modifier::Alt),
        "SHIFT" => Some(Modifier::Shift),
        "WIN" | "SUPER" | "META" | "CMD" | "COMMAND" => Some(Modifier::Meta),
        "MOD" | "PRIMARY" => Some(if cfg!(target_os = "macos") {
            Modifier::Meta
        } else {
            Modifier::Ctrl
        }),
        _ => None,
    }
}

fn parse_key(s: &str) -> Option<Key> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(Key::Char(c.to_ascii_lowercase()));
    }
    let upper = s.trim().to_ascii_uppercase();
    let key = match upper.as_str() {
        "ENTER" | "RETURN" => Key::Enter,
        "TAB" => Key::Tab,
        "ESC" | "ESCAPE" => Key::Escape,
        "SPACE" => Key::Space,
        "BACKSPACE" | "BS" | "BKSP" => Key::Backspace,
        "DELETE" | "DEL" => Key::Delete,
        "INSERT" | "INS" => Key::Insert,
        "HOME" => Key::Home,
        "END" => Key::End,
        "PGUP" | "PAGEUP" | "PAGE_UP" => Key::PageUp,
        "PGDN" | "PAGEDOWN" | "PAGE_DOWN" => Key::PageDown,
        "UP" => Key::Up,
        "DOWN" => Key::Down,
        "LEFT" => Key::Left,
        "RIGHT" => Key::Right,
        "CAPSLOCK" => Key::CapsLock,
        "PRINTSCREEN" | "PRTSC" | "PRINT" => Key::PrintScreen,
        "MENU" | "APPS" => Key::Menu,
        "PLUS" => Key::Char('+'),
        "MINUS" => Key::Char('-'),
        "LBRACE" => Key::Char('{'),
        "RBRACE" => Key::Char('}'),
        f if f.starts_with('F') => {
            let n: u8 = f[1..].parse().ok()?;
            if (1..=24).contains(&n) {
                Key::F(n)
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(mods: &[Modifier], key: Option<Key>) -> KeyStep {
        KeyStep::Chord(Chord {
            modifiers: mods.to_vec(),
            key,
        })
    }

    #[test]
    fn text_and_special_keys() {
        assert_eq!(
            parse("hello world{ENTER}").unwrap(),
            vec![
                KeyStep::Text("hello world".into()),
                chord(&[], Some(Key::Enter))
            ]
        );
    }

    #[test]
    fn names_are_case_insensitive() {
        assert_eq!(parse("{enter}").unwrap(), parse("{ENTER}").unwrap());
        assert_eq!(parse("{ctrl+a}").unwrap(), parse("{CTRL+A}").unwrap());
    }

    #[test]
    fn documented_keys_are_supported() {
        for k in [
            "{SPACE}", "{WIN+D}", "{ALT+F4}", "{PGUP}", "{PGDN}", "{F12}", "{CTRL+Y}",
        ] {
            parse(k).unwrap_or_else(|e| panic!("{k}: {e}"));
        }
    }

    #[test]
    fn multi_modifier_chords() {
        assert_eq!(
            parse("{CTRL+SHIFT+T}").unwrap(),
            vec![chord(
                &[Modifier::Ctrl, Modifier::Shift],
                Some(Key::Char('t'))
            )]
        );
    }

    #[test]
    fn lone_modifier() {
        assert_eq!(
            parse("{WIN}").unwrap(),
            vec![chord(&[Modifier::Meta], None)]
        );
    }

    #[test]
    fn literal_braces() {
        assert_eq!(
            parse("{{\"a\": 1}}").unwrap(),
            vec![KeyStep::Text("{\"a\": 1}".into())]
        );
    }

    #[test]
    fn plus_as_key() {
        assert_eq!(
            parse("{CTRL++}").unwrap(),
            vec![chord(&[Modifier::Ctrl], Some(Key::Char('+')))]
        );
        assert_eq!(
            parse("{+}").unwrap(),
            vec![chord(&[], Some(Key::Char('+')))]
        );
    }

    #[test]
    fn repeat_count() {
        assert_eq!(parse("{TAB 3}").unwrap().len(), 3);
        assert!(parse("{TAB 0}").is_err());
        assert!(parse("{TAB 1000}").is_err());
    }

    #[test]
    fn newlines_become_enter() {
        assert_eq!(
            parse("a\nb").unwrap(),
            vec![
                KeyStep::Text("a".into()),
                chord(&[], Some(Key::Enter)),
                KeyStep::Text("b".into())
            ]
        );
    }

    #[test]
    fn unicode_text_is_kept() {
        assert_eq!(
            parse("çğıöşü 🙂").unwrap(),
            vec![KeyStep::Text("çğıöşü 🙂".into())]
        );
    }

    #[test]
    fn errors() {
        assert!(parse("{ENTER").is_err());
        assert!(parse("{}").is_err());
        assert!(parse("{FOO}").is_err());
        assert!(parse("{HYPER+A}").is_err());
        assert!(parse("{F25}").is_err());
    }
}
