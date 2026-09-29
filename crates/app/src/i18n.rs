//! The interface language: English plus nine translations (`locales/app.yml`), following the
//! system unless the user picks one. GPUI Kit reads the same locale, so its components switch
//! along.

use std::fmt::Display;

pub struct Language {
    pub code: &'static str,
    /// The language's own name for itself.
    pub name: &'static str,
}

pub const LANGUAGES: &[Language] = &[
    Language { code: "en", name: "English" },
    Language { code: "zh-CN", name: "简体中文" },
    Language { code: "zh-TW", name: "繁體中文" },
    Language { code: "ja", name: "日本語" },
    Language { code: "ko", name: "한국어" },
    Language { code: "fr", name: "Français" },
    Language { code: "de", name: "Deutsch" },
    Language { code: "es", name: "Español" },
    Language { code: "pt-BR", name: "Português (Brasil)" },
    Language { code: "ru", name: "Русский" },
];

/// The preference that means "whatever the system uses".
pub const SYSTEM: &str = "system";

/// The best match for the system's preferred languages, or English.
pub fn system_language() -> &'static str {
    sys_locale::get_locales().find_map(|tag| matching(&tag)).unwrap_or("en")
}

/// Map a BCP 47 tag (`de-AT`, `zh-Hant-HK`, `pt_PT`) to a language we have.
fn matching(tag: &str) -> Option<&'static str> {
    let tag = tag.replace('_', "-").to_ascii_lowercase();
    let mut parts = tag.split('-');
    let language = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    match language {
        "zh" => {
            let traditional = rest.iter().any(|p| matches!(*p, "hant" | "tw" | "hk" | "mo"));
            Some(if traditional { "zh-TW" } else { "zh-CN" })
        }
        "pt" => Some("pt-BR"),
        _ => LANGUAGES.iter().find(|l| l.code == language).map(|l| l.code),
    }
}

/// Use `preference` (a language code, or [`SYSTEM`]) from now on; returns the language chosen.
pub fn apply(preference: &str) -> &'static str {
    let code = LANGUAGES.iter().find(|l| l.code == preference).map(|l| l.code).unwrap_or_else(system_language);
    rust_i18n::set_locale(code);
    code
}

pub fn current() -> String {
    rust_i18n::locale().to_string()
}

pub fn language_name(code: &str) -> &'static str {
    LANGUAGES.iter().find(|l| l.code == code).map(|l| l.name).unwrap_or("English")
}

/// A message.
pub fn t(key: &str) -> String {
    lookup(key)
}

/// A message with `%{name}` placeholders filled in.
pub fn tf(key: &str, args: &[(&str, &dyn Display)]) -> String {
    fill(lookup(key), args)
}

/// A message that depends on a number: picks the language's plural form for `count` and fills
/// in `%{count}` as well as `args`.
pub fn tn(key: &str, count: usize, args: &[(&str, &dyn Display)]) -> String {
    let form = plural_form(&current(), count);
    let text = lookup(&format!("{key}.{form}"));
    let mut all: Vec<(&str, &dyn Display)> = vec![("count", &count)];
    all.extend_from_slice(args);
    fill(text, &all)
}

fn lookup(key: &str) -> String {
    let locale = current();
    crate::_rust_i18n_try_translate(&locale, key).map(|m| m.into_owned()).unwrap_or_else(|| key.to_string())
}

fn fill(mut text: String, args: &[(&str, &dyn Display)]) -> String {
    for (name, value) in args {
        text = text.replace(&format!("%{{{name}}}"), &value.to_string());
    }
    text
}

/// The CLDR plural category of `n` in `locale`, among the forms `locales/app.yml` provides.
pub fn plural_form(locale: &str, n: usize) -> &'static str {
    match locale.split('-').next().unwrap_or(locale) {
        "zh" | "ja" | "ko" => "other",
        // French and Portuguese treat zero as singular.
        "fr" | "pt" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "ru" => {
            let (tens, hundreds) = (n % 10, n % 100);
            if tens == 1 && hundreds != 11 {
                "one"
            } else if (2..=4).contains(&tens) && !(12..=14).contains(&hundreds) {
                "few"
            } else {
                "many"
            }
        }
        _ => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// "just now", "3 min ago", … for a Unix timestamp in seconds.
pub fn ago(at: f64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(at);
    let seconds = (now - at).max(0.) as usize;
    match seconds {
        0..=9 => t("time.just_now"),
        10..=59 => tf("time.seconds", &[("count", &seconds)]),
        60..=3599 => tf("time.minutes", &[("count", &(seconds / 60))]),
        3600..=86_399 => tf("time.hours", &[("count", &(seconds / 3600))]),
        _ => tn("time.days", seconds / 86_400, &[]),
    }
}

/// A short date and time, like `Sep 28, 22:50` or `9月28日 22:50`.
pub fn short_date(month: usize, day: u32, time: &str) -> String {
    let months = t("date.months");
    let name = months.split(',').nth(month.saturating_sub(1)).unwrap_or_default().to_string();
    tf("date.short", &[("month", &name), ("day", &day), ("time", &time)])
}

/// Tests that read or set the global locale take this first.
#[cfg(test)]
pub(crate) static TEST_LOCALE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_tags_match_our_languages() {
        assert_eq!(matching("en-US"), Some("en"));
        assert_eq!(matching("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(matching("zh_TW"), Some("zh-TW"));
        assert_eq!(matching("zh-Hant-HK"), Some("zh-TW"));
        assert_eq!(matching("pt-PT"), Some("pt-BR"));
        assert_eq!(matching("de-AT"), Some("de"));
        assert_eq!(matching("sv-SE"), None);
    }

    #[test]
    fn plural_forms_follow_cldr() {
        assert_eq!(plural_form("en", 1), "one");
        assert_eq!(plural_form("en", 0), "other");
        assert_eq!(plural_form("fr", 0), "one");
        assert_eq!(plural_form("ja", 1), "other");
        assert_eq!(
            [1, 2, 5, 11, 21, 22, 25].map(|n| plural_form("ru", n)),
            ["one", "few", "many", "many", "one", "few", "many"]
        );
    }

    #[test]
    fn every_language_has_every_message() {
        let _locale = TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        for language in LANGUAGES {
            rust_i18n::set_locale(language.code);
            for count in [1, 2, 5] {
                let text = tn("headline.in_sync.detail", count, &[("accounts", &3)]);
                assert!(text.contains(&count.to_string()) && text.contains('3'), "{}: {text}", language.code);
            }
            assert!(!t("background.title").starts_with("background."), "{}", language.code);
        }
        rust_i18n::set_locale("en");
    }
}
