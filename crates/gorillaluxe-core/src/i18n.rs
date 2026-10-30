//! Bilingual UI strings: English (EN) and Russian (RU).
//! Hot-swappable: `tr()` is pure lookup, callers re-render on language change.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

/// Global UI language — atomic so the `[ RU | EN ]` pill can hot-swap
/// mid-frame without restart (spec §3.5).
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn current_lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 {
        Lang::Ru
    } else {
        Lang::En
    }
}

/// Switch the active language instantly; next lookup sees the new value.
/// Returns the switch cost — must stay << 1 ms (Phase 2 check).
pub fn set_lang(l: Lang) {
    CURRENT.store(if l == Lang::Ru { 1 } else { 0 }, Ordering::Relaxed);
}

/// Lookup in the currently active language.
pub fn trc(key: &str) -> &str {
    tr(current_lang(), key)
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
pub fn tr(lang: Lang, key: &str) -> &str {
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
    ("ui.keymapper", "Keymapper", "Раскладка"),
    ("ui.quick_settings", "Quick Settings", "Быстрые настройки"),
    ("ui.install_apk", "Install APK", "Установить APK"),
    ("ui.screenshot", "Screenshot", "Скриншот"),
    ("ui.multi_instance", "Multi-Instance Manager", "Менеджер окон"),
    ("ui.preparing", "Preparing Android 13 Environment...", "Подготовка среды Android 13..."),
    ("hud.zero_copy", "DX12 Zero-Copy", "DX12 Zero-Copy"),
    ("gfx.init", "Initializing DirectX 12", "Инициализация DirectX 12"),
    ("gfx.adapter", "D3D12 adapter", "Адаптер D3D12"),
    ("gfx.zerocopy.ok", "Zero-copy shared texture import verified", "Импорт общей текстуры без копирования проверен"),
    ("gfx.cas.ok", "CAS compute pass verified (SSIM >= 0.99)", "Проход CAS проверен (SSIM >= 0.99)"),
    ("gfx.atlas.ok", "Cyrillic glyph coverage verified", "Покрытие кириллических глифов проверено"),
    ("gfx.hotswap.ok", "RU/EN hot-swap under 1 ms", "Переключение RU/EN быстрее 1 мс"),
    ("gfx.steady.zero", "steady_state_cpu_copy_bytes == 0", "steady_state_cpu_copy_bytes == 0"),
    // Phase 3 — interactive UI, keymapper, WHPX auto-setup.
    ("ui.resolution", "Resolution", "Разрешение"),
    ("ui.fps_cap", "FPS cap", "Лимит FPS"),
    ("ui.fps_aim", "FPS AIM", "FPS ПРИЦЕЛ"),
    ("ui.keymapper.hint", "Drag widgets onto the screen, click to bind keys",
     "Перетащите виджеты на экран, нажмите для привязки клавиш"),
    ("ui.apk.dropped", "Installing", "Установка"),
    ("ui.apk.hint", "Drop an .apk/.xapk onto the window", "Перетащите .apk/.xapk на окно"),
    ("ui.shot.saved", "Screenshot saved", "Скриншот сохранён"),
    ("ui.whpx.missing", "WHPX acceleration unavailable", "Ускорение WHPX недоступно"),
    ("ui.whpx.enable", "Enable WHPX Acceleration in 1 Click",
     "Включить ускорение WHPX в 1 клик"),
    ("ui.whpx.restart", "[ Restart PC ]", "[ Перезагрузить ПК ]"),
    ("ui.whpx.done", "WHPX enabled — restart to apply", "WHPX включён — требуется перезагрузка"),
    ("ui.whpx.uac_denied", "Elevation declined", "Права администратора отклонены"),
    ("input.selftest", "Phase 3 self-test: infinite swipe + WASAPI + keymap",
     "Самотест фазы 3: infinite swipe + WASAPI + keymap"),
    ("cli.auto_setup", "Launching elevated WHPX setup...", "Запуск настройки WHPX с правами администратора..."),
    ("cli.setup_done", "Windows features enabled — please restart your PC",
     "Компоненты Windows включены — перезагрузите компьютер"),
    // Phase 4 — Zone D first-boot onboarding / Android runtime.
    ("ui.onboard.title", "GORILLALUXE", "GORILLALUXE"),
    ("ui.onboard.missing", "Android 13 runtime required",
     "Требуется среда Android 13"),
    ("ui.onboard.download", "Download Android Runtime",
     "Скачать среду Android"),
    ("ui.onboard.launch", "Launch Android",
     "Запустить Android"),
    ("ui.onboard.booting", "Booting Android 13...", "Загрузка Android 13..."),
    ("ui.onboard.ready", "Android environment ready", "Среда Android готова"),
    ("ui.onboard.downloading", "Downloading Android 13 Environment...",
     "Загрузка среды Android 13..."),
    ("ui.onboard.extracting", "Extracting archives...",
     "Распаковка архивов..."),
    ("ui.onboard.verifying", "Verifying SHA-256 checksums...",
     "Проверка контрольных сумм SHA-256..."),
    ("ui.onboard.avd", "Creating GorillaLuxe AVD...",
     "Создание AVD GorillaLuxe..."),
    ("ui.onboard.starting", "Starting headless engine...",
     "Запуск фонового движка..."),
    ("ui.onboard.failed", "Setup failed", "Ошибка установки"),
    ("ui.onboard.retry", "Retry Download", "Повторить загрузку"),
    ("cli.setup_android", "Setting up Android runtime (emulator + system image)...",
     "Установка среды Android (эмулятор + образ системы)..."),
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
        assert_eq!(
            tr(Lang::Ru, "app.tagline"),
            "Высокопроизводительная виртуализация Android"
        );
        assert_eq!(tr(Lang::En, "missing.key"), "missing.key");
    }
}
