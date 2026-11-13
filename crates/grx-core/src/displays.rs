//! Liste d'écrans et sélection d'écran, transportés comme octets binaires dans le canal Control fiable.
//!
//! Format `GRX-DSP` (host -> client) : `[magie 7 o][version u8 = 1][active u8][count u8]`
//! puis `count` fois : `[left i32][top i32][width u16][height u16][flags u8][name_len u8][name : name_len octets UTF-8]`.
//! Décodage strict, jamais de panique. L'encodage borne et tronque les valeurs pour garantir
//! `decode(encode(x))` réussi.
//!
//! Format `GRX-MON` (client -> host) : `[magie 7 o][version u8 = 1][index u8]`, longueur exacte 9.
//! Utilisé pour notifier le host de l'écran actif sélectionné par le client.

pub const DISPLAY_LIST_MAGIC: &[u8; 7] = b"GRX-DSP";
pub const DISPLAY_SELECT_MAGIC: &[u8; 7] = b"GRX-MON";
pub const VERSION: u8 = 1;
pub const MAX_DISPLAYS: usize = 16;
pub const MAX_NAME_LEN: usize = 48;
pub const MAX_DIM: u32 = 16_384;
pub const MAX_COORD: i32 = 1 << 24;
pub const NO_ACTIVE: u8 = 0xFF;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub is_virtual: bool,
}


#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DisplayList {
    pub active: Option<u8>,
    pub displays: Vec<DisplayInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplaySelect {
    pub index: u8,
}

/// Tronque une chaîne UTF-8 à `max` octets sur une frontière de caractère valide.
fn truncate_name(name: &str, max: usize) -> &str {
    let mut end = 0;
    for c in name.chars() {
        let l = c.len_utf8();
        if end + l > max {
            break;
        }
        end += l;
    }
    &name[..end]
}

impl DisplayList {
    /// Encode la liste d'écrans au format binaire `GRX-DSP`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10 + 12 + MAX_NAME_LEN);
        buf.extend_from_slice(DISPLAY_LIST_MAGIC);
        buf.push(VERSION);
        buf.push(NO_ACTIVE); // active, réécrit plus bas
        buf.push(0); // count, réécrit plus bas

        let mut written = 0;
        for info in self.displays.iter().take(MAX_DISPLAYS) {
            let left = info.left.clamp(-MAX_COORD, MAX_COORD);
            let top = info.top.clamp(-MAX_COORD, MAX_COORD);
            let width = info.width.clamp(1, MAX_DIM);
            let height = info.height.clamp(1, MAX_DIM);
            let flags = (if info.primary { 1 } else { 0 }) | (if info.is_virtual { 2 } else { 0 });
            let name = truncate_name(&info.name, MAX_NAME_LEN);
            let name_bytes = name.as_bytes();

            buf.extend_from_slice(&left.to_le_bytes());
            buf.extend_from_slice(&top.to_le_bytes());
            buf.extend_from_slice(&(width as u16).to_le_bytes());
            buf.extend_from_slice(&(height as u16).to_le_bytes());
            buf.push(flags);
            buf.push(name_bytes.len() as u8);
            buf.extend_from_slice(name_bytes);
            written += 1;
        }

        let active_byte = match self.active {
            Some(idx) if idx < written => idx,
            _ => NO_ACTIVE,
        };
        buf[8] = active_byte;
        buf[9] = written;
        buf
    }

    /// Décodage strict du message `GRX-DSP`. Retourne `None` en cas de format invalide.
    pub fn decode(b: &[u8]) -> Option<DisplayList> {
        if b.get(..7)? != DISPLAY_LIST_MAGIC {
            return None;
        }
        if *b.get(7)? != VERSION {
            return None;
        }
        let active = *b.get(8)?;
        let count = *b.get(9)? as usize;
        if count > MAX_DISPLAYS {
            return None;
        }
        if active != NO_ACTIVE && active as usize >= count {
            return None;
        }

        let mut displays = Vec::with_capacity(count);
        let mut offset = 10;
        for _ in 0..count {
            let left = i32::from_le_bytes(b.get(offset..offset + 4)?.try_into().ok()?);
            let top = i32::from_le_bytes(b.get(offset + 4..offset + 8)?.try_into().ok()?);
            let width = u16::from_le_bytes(b.get(offset + 8..offset + 10)?.try_into().ok()?);
            let height = u16::from_le_bytes(b.get(offset + 10..offset + 12)?.try_into().ok()?);
            let flags = *b.get(offset + 12)?;
            let name_len = *b.get(offset + 13)? as usize;
            let name_bytes = b.get(offset + 14..offset + 14 + name_len)?;
            let name = std::str::from_utf8(name_bytes).ok()?;

            if width == 0 || width as u32 > MAX_DIM {
                return None;
            }
            if height == 0 || height as u32 > MAX_DIM {
                return None;
            }
            if !(-MAX_COORD..=MAX_COORD).contains(&left) {
                return None;
            }
            if !(-MAX_COORD..=MAX_COORD).contains(&top) {
                return None;
            }
            if name_len > MAX_NAME_LEN {
                return None;
            }

            let primary = (flags & 1) != 0;
            let is_virtual = (flags & 2) != 0;

            displays.push(DisplayInfo {
                name: name.to_string(),
                left,
                top,
                width: width as u32,
                height: height as u32,
                primary,
                is_virtual,
            });
            offset += 14 + name_len;
        }
        if offset != b.len() {
            return None;
        }

        Some(DisplayList {
            active: if active == NO_ACTIVE { None } else { Some(active) },
            displays,
        })
    }
}

impl DisplaySelect {
    /// Encode la sélection d'écran au format binaire `GRX-MON`.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(9);
        b.extend_from_slice(DISPLAY_SELECT_MAGIC);
        b.push(VERSION);
        b.push(self.index);
        b
    }

    /// Décodage strict du message `GRX-MON`. Retourne `None` si longueur != 9 ou champs invalides.
    pub fn decode(b: &[u8]) -> Option<DisplaySelect> {
        if b.len() != 9 {
            return None;
        }
        if b.get(..7)? != DISPLAY_SELECT_MAGIC {
            return None;
        }
        if *b.get(7)? != VERSION {
            return None;
        }
        let index = *b.get(8)?;
        if index as usize >= MAX_DISPLAYS {
            return None;
        }
        Some(DisplaySelect { index })
    }
}

/// Le message est-il une liste d'écrans (`GRX-DSP`) ?
pub fn is_display_list(b: &[u8]) -> bool {
    b.starts_with(DISPLAY_LIST_MAGIC)
}

/// Le message est-il une sélection d'écran (`GRX-MON`) ?
pub fn is_display_select(b: &[u8]) -> bool {
    b.starts_with(DISPLAY_SELECT_MAGIC)
}

impl DisplayInfo {
    /// Retourne une étiquette lisible : `"NOM · LxH"` avec suffixes optionnels.
    pub fn label(&self) -> String {
        let name = if self.name.is_empty() { "Écran" } else { &self.name };
        let mut s = format!("{} · {}x{}", name, self.width, self.height);
        if self.primary {
            s.push_str(" (principal)");
        }
        if self.is_virtual {
            s.push_str(" (virtuel)");
        }
        s
    }
}

impl DisplayList {
    /// Retourne l'info de l'écran actif si l'index est valide.
    pub fn active_info(&self) -> Option<&DisplayInfo> {
        self.active.and_then(|i| self.displays.get(i as usize))
    }

    /// Retourne l'index de la première entrée dont le nom correspond, ou `None`.
    pub fn index_of(&self, name: &str) -> Option<u8> {
        self.displays.iter().position(|d| d.name == name).map(|i| i as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let empty = DisplayList::default();
        assert_eq!(DisplayList::decode(&empty.encode()), Some(empty));

        let one = DisplayList {
            active: Some(0),
            displays: vec![DisplayInfo {
                name: "Écran 1".into(),
                left: -100,
                top: 200,
                width: 1920,
                height: 1080,
                primary: true,
                is_virtual: false,
            }],
        };
        assert_eq!(DisplayList::decode(&one.encode()), Some(one));

        let two = DisplayList {
            active: None,
            displays: vec![
                DisplayInfo {
                    name: "A".into(),
                    left: 0,
                    top: 0,
                    width: 800,
                    height: 600,
                    primary: false,
                    is_virtual: true,
                },
                DisplayInfo {
                    name: "B".into(),
                    left: 800,
                    top: 0,
                    width: 1024,
                    height: 768,
                    primary: true,
                    is_virtual: false,
                },
            ],
        };
        assert_eq!(DisplayList::decode(&two.encode()), Some(two));
    }

    #[test]
    fn bounds_and_clamping() {
        let mut displays = Vec::with_capacity(20);
        for i in 0..20 {
            displays.push(DisplayInfo {
                name: format!("D{}", i),
                left: 0,
                top: 0,
                width: 100,
                height: 100,
                primary: false,
                is_virtual: false,
            });
        }
        let list = DisplayList { active: Some(0), displays };
        let decoded = DisplayList::decode(&list.encode()).unwrap();
        assert_eq!(decoded.displays.len(), 16);

        let long_name: String = "é".repeat(100);
        let list = DisplayList {
            active: None,
            displays: vec![DisplayInfo {
                name: long_name,
                left: 0,
                top: 0,
                width: 100,
                height: 100,
                primary: false,
                is_virtual: false,
            }],
        };
        let decoded = DisplayList::decode(&list.encode()).unwrap();
        assert_eq!(decoded.displays[0].name.len(), 48);
        assert!(std::str::from_utf8(decoded.displays[0].name.as_bytes()).is_ok());

        let list = DisplayList {
            active: None,
            displays: vec![
                DisplayInfo {
                    name: "W0".into(),
                    left: 0,
                    top: 0,
                    width: 0,
                    height: 100,
                    primary: false,
                    is_virtual: false,
                },
                DisplayInfo {
                    name: "W99".into(),
                    left: 0,
                    top: 0,
                    width: 99_999,
                    height: 100,
                    primary: false,
                    is_virtual: false,
                },
            ],
        };
        let decoded = DisplayList::decode(&list.encode()).unwrap();
        assert_eq!(decoded.displays[0].width, 1);
        assert_eq!(decoded.displays[1].width, MAX_DIM);

        let list = DisplayList {
            active: Some(5),
            displays: vec![DisplayInfo::default(), DisplayInfo::default()],
        };
        let decoded = DisplayList::decode(&list.encode()).unwrap();
        assert_eq!(decoded.active, None);
    }

    #[test]
    fn malformed_messages_refused() {
        let good = DisplayList {
            active: Some(0),
            displays: vec![DisplayInfo {
                name: "X".into(),
                left: 0,
                top: 0,
                width: 100,
                height: 100,
                primary: false,
                is_virtual: false,
            }],
        };
        let b = good.encode();
        assert_eq!(DisplayList::decode(&[]), None);
        for n in 0..b.len() {
            assert_eq!(DisplayList::decode(&b[..n]), None, "préfixe {}", n);
        }
        let mut extra = b.clone();
        extra.push(0);
        assert_eq!(DisplayList::decode(&extra), None);

        let mut bad_magic = b.clone();
        bad_magic[0] = b'X';
        assert_eq!(DisplayList::decode(&bad_magic), None);

        let mut v2 = b.clone();
        v2[7] = 2;
        assert_eq!(DisplayList::decode(&v2), None);

        let mut bad_count = b.clone();
        bad_count[9] = 17;
        assert_eq!(DisplayList::decode(&bad_count), None);

        let mut bad_active = b.clone();
        bad_active[8] = 1;
        assert_eq!(DisplayList::decode(&bad_active), None);

        let mut bad_width = b.clone();
        bad_width[18] = 0;
        bad_width[19] = 0;
        assert_eq!(DisplayList::decode(&bad_width), None);

        let mut bad_name_len = b.clone();
        bad_name_len[23] = 49;
        assert_eq!(DisplayList::decode(&bad_name_len), None);

        let mut bad_utf8 = b.clone();
        bad_utf8[24] = 0xFF;
        assert_eq!(DisplayList::decode(&bad_utf8), None);
    }

    #[test]
    fn display_select_roundtrips_and_refusals() {
        for idx in [0, 15] {
            let sel = DisplaySelect { index: idx };
            assert_eq!(DisplaySelect::decode(&sel.encode()), Some(sel));
        }
        assert_eq!(DisplaySelect::decode(&DisplaySelect { index: 16 }.encode()), None);
        assert_eq!(DisplaySelect::decode(&[0; 8]), None);
        assert_eq!(DisplaySelect::decode(&[0; 10]), None);
        assert_eq!(DisplaySelect::decode(DISPLAY_LIST_MAGIC), None);
        assert_eq!(DisplaySelect::decode(&DisplayList::default().encode()), None);
        assert_eq!(DisplayList::decode(&DisplaySelect { index: 0 }.encode()), None);
        assert!(is_display_select(&DisplaySelect { index: 0 }.encode()) && !is_display_list(&DisplaySelect { index: 0 }.encode()));
        assert!(!is_display_list(b"GRX-SET...") && !is_display_select(b"GRX-SET..."));
    }

    #[test]
    fn utility_methods() {
        let info = DisplayInfo {
            name: "DELL".into(),
            left: 0,
            top: 0,
            width: 2560,
            height: 1440,
            primary: true,
            is_virtual: false,
        };
        assert_eq!(info.label(), "DELL · 2560x1440 (principal)");

        let empty = DisplayInfo {
            name: "".into(),
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            primary: false,
            is_virtual: true,
        };
        assert_eq!(empty.label(), "Écran · 1920x1080 (virtuel)");

        let list = DisplayList {
            active: Some(1),
            displays: vec![
                DisplayInfo { name: "A".into(), ..Default::default() },
                DisplayInfo { name: "B".into(), ..Default::default() },
            ],
        };
        assert_eq!(list.index_of("B"), Some(1));
        assert_eq!(list.index_of("C"), None);
        assert_eq!(list.active_info().map(|d| &d.name), Some(&"B".to_string()));
    }

    #[test]
    fn fuzz_never_panics_and_respects_bounds() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut accepted = 0;
        for i in 0..200_000 {
            let len = (next() % 200) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if i % 2 == 0 && b.len() >= 10 {
                let m = if i % 4 == 0 { DISPLAY_LIST_MAGIC } else { DISPLAY_SELECT_MAGIC };
                b[..7].copy_from_slice(m);
                b[7] = VERSION;
                if m == DISPLAY_LIST_MAGIC {
                    let c = (next() % 17) as usize;
                    b[8] = if c == 0 { NO_ACTIVE } else { (next() % c as u64) as u8 };
                    b[9] = c as u8;
                    if i % 8 == 0 {
                        b.truncate(10);
                        for _ in 0..c {
                            b.extend_from_slice(&0i32.to_le_bytes());
                            b.extend_from_slice(&0i32.to_le_bytes());
                            b.extend_from_slice(&1u16.to_le_bytes());
                            b.extend_from_slice(&1u16.to_le_bytes());
                            b.push(0);
                            b.push(0);
                        }
                    }
                } else {
                    b.truncate(9);
                    b[8] = (next() % MAX_DISPLAYS as u64) as u8;
                }
            }
            if let Some(d) = DisplayList::decode(&b) {
                accepted += 1;
                assert!(d.displays.len() <= MAX_DISPLAYS);
                if let Some(a) = d.active {
                    assert!(a < d.displays.len() as u8);
                }
                for info in &d.displays {
                    assert!((1..=MAX_DIM).contains(&info.width));
                    assert!((1..=MAX_DIM).contains(&info.height));
                    assert!(info.name.len() <= MAX_NAME_LEN);
                    assert!(std::str::from_utf8(info.name.as_bytes()).is_ok());
                }
                assert_eq!(DisplayList::decode(&d.encode()).as_ref(), Some(&d));
            }
            if DisplaySelect::decode(&b).is_some() {
                accepted += 1;
            }
        }
        assert!(accepted >= 1000, "au moins 1000 messages GRX-MON ou GRX-DSP acceptés : {}", accepted);
    }
}
