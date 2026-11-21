//! Roblox 88-key virtual piano layout (A0..C8).
//!
//! Middle 61 keys (C2..C7) = standard virtual piano:
//!   white keys: 1234567890qwertyuiopasdfghjklzxcvbnm
//!   black keys: Shift + the white key just below (! @ $ % ^ * ( Q W E T ...)
//! Outer 27 keys, one key per semitone with Ctrl held:
//!   A0..B1  = Ctrl + 1 2 3 4 5 6 7 8 9 0 q w e r t
//!   C#7..C8 = Ctrl + y u i o p a s d f g h j
//!
//! Keys are sent as physical scancodes (set 1, US/JIS position), so the
//! Windows keyboard layout (e.g. JIS, where Shift+2 is `"`) doesn't matter.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mod {
    None,
    Shift,
    Ctrl,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub scan: u16,
    pub modifier: Mod,
    pub ch: char,
}

pub const SC_LSHIFT: u16 = 0x2A;
pub const SC_LCTRL: u16 = 0x1D;
pub const SC_SPACE: u16 = 0x39;

pub const LOWEST: u8 = 21; // A0
pub const HIGHEST: u8 = 108; // C8

const WHITE_KEYS: &str = "1234567890qwertyuiopasdfghjklzxcvbnm";
const LOW_EXT: &str = "1234567890qwert"; // A0..B1
const HIGH_EXT: &str = "yuiopasdfghj"; // C#7..C8

pub fn scancode(c: char) -> u16 {
    match c {
        '1' => 0x02, '2' => 0x03, '3' => 0x04, '4' => 0x05, '5' => 0x06,
        '6' => 0x07, '7' => 0x08, '8' => 0x09, '9' => 0x0A, '0' => 0x0B,
        'q' => 0x10, 'w' => 0x11, 'e' => 0x12, 'r' => 0x13, 't' => 0x14,
        'y' => 0x15, 'u' => 0x16, 'i' => 0x17, 'o' => 0x18, 'p' => 0x19,
        'a' => 0x1E, 's' => 0x1F, 'd' => 0x20, 'f' => 0x21, 'g' => 0x22,
        'h' => 0x23, 'j' => 0x24, 'k' => 0x25, 'l' => 0x26,
        'z' => 0x2C, 'x' => 0x2D, 'c' => 0x2E, 'v' => 0x2F, 'b' => 0x30,
        'n' => 0x31, 'm' => 0x32,
        _ => panic!("no scancode for {c}"),
    }
}

fn is_black(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// Map a MIDI pitch (21..=108) to its key. Returns None outside the piano.
pub fn key_for(pitch: u8) -> Option<Key> {
    let mk = |c: char, m: Mod| Key { scan: scancode(c), modifier: m, ch: c };
    match pitch {
        21..=35 => Some(mk(LOW_EXT.as_bytes()[(pitch - 21) as usize] as char, Mod::Ctrl)),
        97..=108 => Some(mk(HIGH_EXT.as_bytes()[(pitch - 97) as usize] as char, Mod::Ctrl)),
        36..=96 => {
            let black = is_black(pitch);
            let white = if black { pitch - 1 } else { pitch };
            // index of `white` among white keys starting at C2 (36)
            let mut idx = 0usize;
            for p in 36..white {
                if !is_black(p) {
                    idx += 1;
                }
            }
            let c = WHITE_KEYS.as_bytes()[idx] as char;
            Some(mk(c, if black { Mod::Shift } else { Mod::None }))
        }
        _ => None,
    }
}

/// Fold a pitch into the 88-key range by octaves.
pub fn fold(mut pitch: i32) -> u8 {
    while pitch < LOWEST as i32 {
        pitch += 12;
    }
    while pitch > HIGHEST as i32 {
        pitch -= 12;
    }
    pitch as u8
}

/// Human readable key label as shown in-game (e.g. "T", "Ctrl+1").
#[cfg_attr(not(test), allow(dead_code))]
pub fn label(k: &Key) -> String {
    match k.modifier {
        Mod::None => k.ch.to_string(),
        Mod::Ctrl => format!("Ctrl+{}", k.ch),
        Mod::Shift => match k.ch {
            '1' => "!".into(), '2' => "@".into(), '4' => "$".into(), '5' => "%".into(),
            '6' => "^".into(), '8' => "*".into(), '9' => "(".into(),
            c => c.to_ascii_uppercase().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_picture() {
        // 61-key part, C2..C7, exactly as in the reference image
        let expect_white = "1234567890qwertyuiopasdfghjklzxcvbnm";
        let expect_black = ["!", "@", "$", "%", "^", "*", "(", "Q", "W", "E", "T", "Y", "I", "O", "P",
            "S", "D", "G", "H", "J", "L", "Z", "C", "V", "B"];
        let mut whites = String::new();
        let mut blacks = vec![];
        for p in 36..=96u8 {
            let k = key_for(p).unwrap();
            if is_black(p) { blacks.push(label(&k)); } else { whites.push_str(&label(&k)); }
        }
        assert_eq!(whites, expect_white);
        assert_eq!(blacks, expect_black);
        assert_eq!(label(&key_for(60).unwrap()), "t"); // C4 = t
        assert_eq!(label(&key_for(61).unwrap()), "T"); // C#4 = T
    }

    #[test]
    fn ctrl_extension() {
        assert_eq!(label(&key_for(21).unwrap()), "Ctrl+1"); // A0
        assert_eq!(label(&key_for(35).unwrap()), "Ctrl+t"); // B1
        assert_eq!(label(&key_for(97).unwrap()), "Ctrl+y"); // C#7
        assert_eq!(label(&key_for(108).unwrap()), "Ctrl+j"); // C8
        assert!(key_for(20).is_none() && key_for(109).is_none());
        // every one of the 88 notes has a distinct (key, modifier) pair
        let mut seen = std::collections::HashSet::new();
        for p in 21..=108u8 {
            let k = key_for(p).unwrap();
            assert!(seen.insert((k.scan, k.modifier as u8)), "duplicate at {p}");
        }
        assert_eq!(seen.len(), 88);
    }
}
