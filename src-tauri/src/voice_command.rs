//! Local, deterministic detection of explicit voice commands (AGENTS §10).
//!
//! "Låt oss se" / "Let's see" is also ordinary filler in speech, so a
//! candidate segment counts as the command only when the command is
//! essentially the whole utterance: optionally wrapped in a couple of filler
//! words ("Okej, låt oss se.", "Let's see here"). A sentence that merely
//! contains the phrase ("Låt oss se vad som händer om…") is normal dialogue.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceCommand {
    LookAtScreen,
}

const COMMANDS: [&[&str]; 4] = [&["låt", "oss", "se"], &["lått", "oss", "se"], &["let's", "see"], &["lets", "see"]];
const FILLERS: [&str; 18] = [
    "okej", "ok", "okay", "ja", "jaha", "mm", "hm", "eh", "öh", "så", "nu", "här", "då", "alright", "so", "now", "here", "um",
];
/// Words allowed around the command before it stops being "the whole utterance".
const MAX_EXTRA_WORDS: usize = 2;

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
                .replace('’', "'")
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

pub fn detect(text: &str) -> Option<VoiceCommand> {
    let words = words(text);
    for command in COMMANDS {
        let Some(start) = words.windows(command.len()).position(|window| window == command) else { continue };
        let extra: Vec<&String> = words[..start].iter().chain(&words[start + command.len()..]).collect();
        if extra.len() <= MAX_EXTRA_WORDS && extra.iter().all(|word| FILLERS.contains(&word.as_str())) {
            return Some(VoiceCommand::LookAtScreen);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_command_as_a_whole_utterance() {
        for text in ["Låt oss se.", "låt oss se", "Okej, låt oss se.", "Låt oss se här.", "Let's see.", "Let’s see", "OK, let's see now", "Lets see"] {
            assert_eq!(detect(text), Some(VoiceCommand::LookAtScreen), "{text}");
        }
    }

    #[test]
    fn ignores_the_phrase_inside_normal_dialogue() {
        for text in [
            "Låt oss se vad som händer om vi lägger till en cache.",
            "Let's see if I remember this correctly.",
            "Vi kan väl se på det.",
            "Låt oss",
            "Kan vi se koden?",
            "",
        ] {
            assert_eq!(detect(text), None, "{text}");
        }
    }
}
