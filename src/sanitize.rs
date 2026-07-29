use std::sync::OnceLock;

use regex::Regex;

const ERROR_DETAIL_LIMIT: usize = 500;

pub fn clean_text(value: impl AsRef<str>, limit: usize) -> String {
    strip_ansi_sequences(value.as_ref())
        .chars()
        .filter(|character| !is_unsafe_control(*character))
        .take(limit)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn terminal_line(value: &str, limit: usize) -> String {
    clean_text(value.replace('\t', "    "), limit)
}

pub fn error_detail(value: &str) -> String {
    let value = strip_ansi_sequences(value);
    let value = url_regex().replace_all(&value, "<url>");
    let value = arm_resource_regex().replace_all(&value, "<resource>");
    let value = labeled_identifier_regex().replace_all(&value, "<identifier>");
    let value = quoted_client_regex().replace_all(&value, "client <identifier>");
    let value = email_regex().replace_all(&value, "<email>");
    let value = guid_regex().replace_all(&value, "<guid>");
    clean_text(value, ERROR_DETAIL_LIMIT)
}

pub(crate) fn is_unsafe_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{206f}'
        )
}

fn strip_ansi_sequences(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        EscapeIntermediate,
        Csi,
        Osc,
        OscEscape,
        String,
        StringEscape,
    }

    let mut output = String::with_capacity(value.len());
    let mut state = State::Text;
    for character in value.chars() {
        state = match state {
            State::Text => match character {
                '\u{001b}' => State::Escape,
                '\u{009b}' => State::Csi,
                '\u{009d}' => State::Osc,
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => State::String,
                _ => {
                    output.push(character);
                    State::Text
                }
            },
            State::Escape => match character {
                '[' => State::Csi,
                ']' => State::Osc,
                'P' | 'X' | '^' | '_' => State::String,
                ' '..='/' => State::EscapeIntermediate,
                '0'..='~' => State::Text,
                _ => State::Escape,
            },
            State::EscapeIntermediate => match character {
                ' '..='/' => State::EscapeIntermediate,
                '0'..='~' => State::Text,
                _ => State::EscapeIntermediate,
            },
            State::Csi => {
                if ('@'..='~').contains(&character) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::Osc => match character {
                '\u{0007}' | '\u{009c}' => State::Text,
                '\u{001b}' => State::OscEscape,
                _ => State::Osc,
            },
            State::OscEscape => {
                if character == '\\' {
                    State::Text
                } else if character == '\u{001b}' {
                    State::OscEscape
                } else {
                    State::Osc
                }
            }
            State::String => match character {
                '\u{009c}' => State::Text,
                '\u{001b}' => State::StringEscape,
                _ => State::String,
            },
            State::StringEscape => {
                if character == '\\' {
                    State::Text
                } else if character == '\u{001b}' {
                    State::StringEscape
                } else {
                    State::String
                }
            }
        };
    }
    output
}

fn url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:https?|ftp)://[^\s<>"']+|\bwww\.[^\s<>"']+"#)
            .expect("static URL regex")
    })
}

fn arm_resource_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?i)/(?:subscriptions|resourcegroups|providers|tenants|managementgroups)(?:/[^\s<>"'?,;)\]}]+)+"#,
        )
        .expect("static ARM resource regex")
    })
}

fn labeled_identifier_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            \b(?:
                (?:tenant|client|object|principal|application|app)[\s_-]*(?:id|identifier)
                | oid | tid | appid
            )\b
            \s*["']?\s*(?:(?:is)\b|[:=])?\s*["']?
            [[:alnum:]_@./:+%~-]+
            ["']?
            "#,
        )
        .expect("static labeled identifier regex")
    })
}

fn quoted_client_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)\bclient\s+['"][^'"\r\n]{1,1024}['"]"#)
            .expect("static quoted client regex")
    })
}

fn email_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)\b[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)+\b")
            .expect("static email regex")
    })
}

fn guid_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{32})",
        )
        .expect("static GUID regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminal_ansi_controls_and_bidi_controls() {
        let input = concat!(
            "\u{1b}[31mred\u{1b}[0m",
            "\u{1b}]8;;https://attacker.invalid\u{7}link\u{1b}]8;;\u{7}",
            "\u{1b}(B",
            "\u{202e}spoof\u{2066}",
            "\u{009b}32mgreen\u{009b}0m\n"
        );
        assert_eq!(terminal_line(input, 80), "redlinkspoofgreen");
    }

    #[test]
    fn authorization_failed_details_do_not_retain_private_identifiers() {
        let input = concat!(
            "\u{1b}[31mERROR:\u{1b}[0m (AuthorizationFailed) ",
            "The client 'alice.admin@contoso.example' with object id ",
            "'11111111-2222-3333-4444-555555555555' does not have authorization ",
            "to perform action 'Microsoft.Web/sites/read' over scope ",
            "'/subscriptions/AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE/",
            "resourceGroups/internal-secret/providers/Microsoft.Web/sites/private-api'. ",
            "tenantId='01234567-89ab-cdef-0123-456789abcdef'; ",
            "client_id: fedcba98-7654-3210-fedc-ba9876543210; ",
            "principal identifier=00112233445566778899aabbccddeeff. ",
            "oid='x'; appId: opaque-client; ",
            "See https://portal.azure.us/#view/Microsoft_Azure_Resources/",
            "ResourceMenuBlade/~/overview\u{202e}"
        );

        let result = error_detail(input);
        for secret in [
            "alice.admin",
            "contoso.example",
            "11111111",
            "AAAAAAAA",
            "internal-secret",
            "private-api",
            "01234567",
            "fedcba98",
            "00112233",
            "opaque-client",
            "portal.azure.us",
            "/subscriptions/",
        ] {
            assert!(
                !result.contains(secret),
                "sanitized detail retained {secret:?}: {result}"
            );
        }
        assert!(result.contains("AuthorizationFailed"));
        assert!(!result.chars().any(is_unsafe_control));
        assert!(!result.contains('\u{1b}'));
    }

    #[test]
    fn redacts_unlabeled_emails_guids_arm_paths_and_urls() {
        let result = error_detail(
            "owner@example.com {DEADBEEF-DEAD-BEEF-DEAD-BEEFDEADBEEF} \
             /providers/Microsoft.Authorization/roleAssignments/secret \
             ftp://files.example.invalid/private",
        );
        assert_eq!(result, "<email> {<guid>} <resource> <url>");
    }
}
