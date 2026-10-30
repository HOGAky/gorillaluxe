//! Bilingual UI strings: English (EN) and Russian (RU).
//! Hot-swappable: `tr()` is pure lookup, callers re-render on language change.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    pub fn from_code(s: &str) -> Option<Lang> {
        match s.to_ascii_lowercase().as_str() {
            "en" | "eng" | "english" => Some(Lang::En),
            "ru" | "rus" | "russian" => Some(Lang::Ru),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }
}

/// Translate `key` for `lang`. Falls back to English, then to the key itself.
pub fn tr<'a>(lang: Lang, key: &'a str) -> &'a str {
    let pair = STRINGS.iter().find(|(k, _, _)| *k == key);
    match (lang, pair) {
        (_, Some((_, en, _))) if lang == Lang::En => en,
        (_, Some((_, _, ru))) => ru,
        _ => key,
    }
}

/// (key, english, russian)
static STRINGS: &[(&str, &str, &str)] = &[
    ("app.tagline", "High-Performance Android Virtualization", "Высокопроизводительная виртуализация Android"),
    ("cli.help", "Usage: gorillaluxe-cli [--self-test] [--lang en|ru] [--allow-no-whpx] [--guest <path>] [--version]",
                 "Использование: gorillaluxe-cli [--self-test] [--lang en|ru] [--allow-no-whpx] [--guest <путь>] [--version]"),
    ("selftest.start", "Phase 1 self-test: WHPX vCPU smoke test", "Самотест фазы 1: проверка vCPU через WHPX"),
    ("selftest.whpx.found", "WHPX hypervisor detected", "Гипервизор WHPX обнаружен"),
    ("selftest.whpx.missing", "WHPX is not available on this host", "WHPX недоступен на этом узле"),
    ("selftest.partition.ok", "WHPX partition created and configured", "Раздел WHPX создан и настроен"),
    ("selftest.guest.ok", "Guest payload executed, magic sequence received", "Гостевой код выполнен, магическая последовательность получена"),
    ("selftest.guest.bad", "Guest finished but magic sequence was not received", "Гость завершил работу, но магическая последовательность не получена"),
    ("selftest.pass", "SELFTEST: PASS", "САМОТЕСТ: ПРОЙДЕН"),
    ("selftest.fail", "SELFTEST: FAIL", "САМОТЕСТ: ПРОВАЛЕН"),
    ("selftest.skip", "SELFTEST: SKIP", "САМОТЕСТ: ПРОПУЩЕН"),
    ("ui.welcome", "Welcome to GorillaLuxe", "Добро пожаловать в GorillaLuxe"),
    ("ui.phase1", "Phase 1 — WHPX foundation", "Фаза 1 — базис WHPX"),
    ("ui.phase2", "Phase 2 — DX12 zero-copy pipeline (next)", "Фаза 2 — конвейер DX12 zero-copy (далее)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_keys_have_both_languages() {
        for (k, en, ru) in STRINGS {
            assert!(!en.is_empty(), "empty EN for {}", k);
            assert!(!ru.is_empty(), "empty RU for {}", k);
        }
    }

    #[test]
    fn lang_codes() {
        assert_eq!(Lang::from_code("RU"), Some(Lang::Ru));
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(tr(Lang::Ru, "app.tagline"), "Высокопроизводительная виртуализация Android");
        assert_eq!(tr(Lang::En, "missing.key"), "missing.key");
    }
}
