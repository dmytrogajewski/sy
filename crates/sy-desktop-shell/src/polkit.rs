#[derive(Debug, Clone, PartialEq, Eq)]
enum PamEvent {
    Prompt { text: String, echo: bool },
    Info(String),
    Error(String),
    Complete(bool),
}

fn guidance(event: &PamEvent) -> &'static str {
    match event {
        PamEvent::Prompt { echo: false, .. } => "Enter your password.",
        PamEvent::Prompt { echo: true, .. } => "Answer the authentication prompt.",
        PamEvent::Info(text) if text.to_ascii_lowercase().contains("touch") => {
            "Touch your FIDO security key."
        }
        PamEvent::Info(_) => "Follow the authentication instructions below.",
        PamEvent::Error(_) => "Authentication needs your attention.",
        PamEvent::Complete(true) => "Authenticated.",
        PamEvent::Complete(false) => "Authentication failed. Try again.",
    }
}

fn parse_line(line: &str) -> anyhow::Result<PamEvent> {
    let mut bytes = Vec::new();
    let mut source = line.bytes().peekable();
    while let Some(byte) = source.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = source
            .next()
            .ok_or_else(|| anyhow::anyhow!("Truncated helper escape"))?;
        if (b'0'..=b'7').contains(&escaped) {
            let mut octal = u16::from(escaped - b'0');
            for _ in 0..2 {
                if source.peek().is_some_and(|b| (b'0'..=b'7').contains(b)) {
                    octal = octal * 8 + u16::from(source.next().unwrap() - b'0');
                }
            }
            bytes.push(u8::try_from(octal)?);
        } else {
            bytes.push(match escaped {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'b' => 8,
                b'f' => 12,
                b'v' => 11,
                other => other,
            });
        }
    }
    let decoded = String::from_utf8(bytes)?;
    let line = decoded.as_str();
    for (prefix, echo) in [
        ("PAM_PROMPT_ECHO_OFF ", false),
        ("PAM_PROMPT_ECHO_ON ", true),
    ] {
        if let Some(text) = line.strip_prefix(prefix) {
            return Ok(PamEvent::Prompt {
                text: text.into(),
                echo,
            });
        }
    }
    match line {
        "SUCCESS" => Ok(PamEvent::Complete(true)),
        "FAILURE" => Ok(PamEvent::Complete(false)),
        _ if line.starts_with("PAM_TEXT_INFO ") => Ok(PamEvent::Info(line[14..].into())),
        _ if line.starts_with("PAM_ERROR_MSG ") => Ok(PamEvent::Error(line[14..].into())),
        _ => anyhow::bail!("Unexpected PolicyKit helper message"),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn rejects_completion_with_a_suffix() {
        assert!(super::parse_line("SUCCESS forged").is_err());
    }

    #[test]
    fn factor_guidance_follows_the_actual_pam_prompt() {
        assert_eq!(
            super::guidance(&super::PamEvent::Info("Please touch the device.".into())),
            "Touch your FIDO security key."
        );
        assert_eq!(
            super::guidance(&super::PamEvent::Prompt {
                text: "Password:".into(),
                echo: false
            }),
            "Enter your password."
        );
    }
    #[test]
    fn helper_text_decodes_escaped_newlines_and_utf8() {
        assert_eq!(
            super::parse_line(r"PAM_TEXT_INFO Touch\nsecurity key \303\251").unwrap(),
            super::PamEvent::Info("Touch\nsecurity key é".into())
        );
    }
    #[test]
    fn password_prompt_enables_masked_input() {
        assert_eq!(
            super::parse_line("PAM_PROMPT_ECHO_OFF Password:").unwrap(),
            super::PamEvent::Prompt {
                text: "Password:".into(),
                echo: false
            }
        );
    }
}
mod backend;
mod ui;

/// Run the session PolicyKit agent, or preview its dialog without authenticating.
pub fn run(preview: bool) -> anyhow::Result<()> {
    ui::run(preview)
}
