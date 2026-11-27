use evdev::KeyCode;

const TEXT_KEY_RANGES: [(KeyCode, KeyCode); 4] = [
    (KeyCode::KEY_1, KeyCode::KEY_EQUAL),
    (KeyCode::KEY_Q, KeyCode::KEY_RIGHTBRACE),
    (KeyCode::KEY_A, KeyCode::KEY_GRAVE),
    (KeyCode::KEY_BACKSLASH, KeyCode::KEY_SLASH),
];

const TEXT_KEYS: [KeyCode; 11] = [
    KeyCode::KEY_102ND,
    KeyCode::KEY_KPSLASH,
    KeyCode::KEY_KPASTERISK,
    KeyCode::KEY_KPMINUS,
    KeyCode::KEY_KPPLUS,
    KeyCode::KEY_KPEQUAL,
    KeyCode::KEY_KPPLUSMINUS,
    KeyCode::KEY_KPCOMMA,
    KeyCode::KEY_KPJPCOMMA,
    KeyCode::KEY_KPLEFTPAREN,
    KeyCode::KEY_KPRIGHTPAREN,
];

pub(crate) fn is_text_key(key: KeyCode) -> bool {
    TEXT_KEY_RANGES
        .iter()
        .any(|(start, end)| (start.0..=end.0).contains(&key.0))
        || TEXT_KEYS.contains(&key)
}

pub(crate) fn text_keys() -> impl Iterator<Item = KeyCode> {
    TEXT_KEY_RANGES
        .into_iter()
        .flat_map(|(start, end)| (start.0..=end.0).map(KeyCode::new))
        .chain(TEXT_KEYS)
}
