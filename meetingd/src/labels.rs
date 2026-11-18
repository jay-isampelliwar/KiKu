//! After a meeting: name the remote speakers, and remove words that leaked from the speakers
//! into the mic. Pure logic, no I/O.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A leaked mic word must match a remote word within this many seconds.
const LEAK_WINDOW_SECONDS: f64 = 1.0;

/// One spoken word with times on the meeting clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub word: String,
}

/// One saved transcript line.
#[derive(Debug, Clone)]
pub struct Line {
    pub id: i64,
    /// From the mic (me) or from system audio (others).
    pub is_me: bool,
    pub start: f64,
    pub end: f64,
    pub words: Vec<Word>,
}

/// One pyannote turn on the meeting clock.
#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    pub speaker: String,
}

/// An edit to apply to a saved line.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Speaker { id: i64, speaker: String },
    Text { id: i64, text: String, words: Vec<Word> },
    Delete { id: i64 },
}

/// Work out every edit for one meeting.
pub fn plan(lines: &[Line], turns: &[Turn]) -> Vec<Change> {
    let names = person_names(turns);
    let mut changes = Vec::new();

    for line in lines.iter().filter(|l| !l.is_me) {
        if let Some(raw) = best_turn(line, turns) {
            changes.push(Change::Speaker { id: line.id, speaker: names[raw].clone() });
        }
    }

    let remote_words: Vec<(f64, String)> = lines
        .iter()
        .filter(|l| !l.is_me)
        .flat_map(|l| &l.words)
        .flat_map(|w| tokens(&w.word).into_iter().map(move |t| (w.start, t)))
        .collect();

    for line in lines.iter().filter(|l| l.is_me && !l.words.is_empty()) {
        let kept: Vec<Word> = line
            .words
            .iter()
            .filter(|w| !is_leak(w, &remote_words))
            .cloned()
            .collect();

        if kept.is_empty() {
            changes.push(Change::Delete { id: line.id });
        } else if kept.len() < line.words.len() {
            let text = kept.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ");
            changes.push(Change::Text { id: line.id, text, words: kept });
        }
    }

    changes
}

/// "SPEAKER_01" → "Person 1", "Person 2", … in the order they first speak.
fn person_names(turns: &[Turn]) -> HashMap<&str, String> {
    let mut sorted: Vec<&Turn> = turns.iter().collect();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut names = HashMap::new();
    for turn in sorted {
        let next = names.len() + 1;
        names.entry(turn.speaker.as_str()).or_insert_with(|| format!("Person {next}"));
    }

    names
}

/// The speaker whose turns overlap this line the most, if any overlap at all.
fn best_turn<'a>(line: &Line, turns: &'a [Turn]) -> Option<&'a str> {
    let mut overlap: HashMap<&str, f64> = HashMap::new();

    for turn in turns {
        let shared = line.end.min(turn.end) - line.start.max(turn.start);
        if shared > 0.0 {
            *overlap.entry(turn.speaker.as_str()).or_default() += shared;
        }
    }

    overlap
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(speaker, _)| speaker)
}

/// A mic word is a leak if every part of it was said remotely at about the same time.
fn is_leak(word: &Word, remote: &[(f64, String)]) -> bool {
    let parts = tokens(&word.word);

    !parts.is_empty()
        && parts.iter().all(|part| {
            remote
                .iter()
                .any(|(start, token)| token == part && (start - word.start).abs() <= LEAK_WINDOW_SECONDS)
        })
}

/// Lowercase, drop punctuation, and split contractions: "I'm," → ["i", "am"].
fn tokens(word: &str) -> Vec<String> {
    let clean: String = word
        .to_lowercase()
        .replace('’', "'")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .collect();

    let expanded = [
        ("n't", " not"),
        ("'m", " am"),
        ("'re", " are"),
        ("'s", " is"),
        ("'ve", " have"),
        ("'ll", " will"),
        ("'d", " would"),
    ]
    .iter()
    .fold(clean, |text, (short, long)| text.replace(short, long));

    expanded
        .split_whitespace()
        .map(|t| t.trim_matches('\'').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(start: f64, text: &str) -> Vec<Word> {
        text.split(' ')
            .enumerate()
            .map(|(i, w)| Word { start: start + i as f64 * 0.3, end: start + i as f64 * 0.3 + 0.2, word: w.into() })
            .collect()
    }

    fn line(id: i64, is_me: bool, start: f64, text: &str) -> Line {
        let words = words(start, text);
        let end = words.last().map_or(start, |w| w.end);
        Line { id, is_me, start, end, words }
    }

    fn turn(start: f64, end: f64, speaker: &str) -> Turn {
        Turn { start, end, speaker: speaker.into() }
    }

    #[test]
    fn speakers_named_in_order_of_first_turn() {
        let lines = [line(1, false, 0.0, "hello all"), line(2, false, 10.0, "hi there")];
        let turns = [turn(9.0, 12.0, "SPEAKER_00"), turn(0.0, 2.0, "SPEAKER_01")];

        let changes = plan(&lines, &turns);

        assert!(changes.contains(&Change::Speaker { id: 1, speaker: "Person 1".into() }));
        assert!(changes.contains(&Change::Speaker { id: 2, speaker: "Person 2".into() }));
    }

    #[test]
    fn line_without_any_turn_keeps_its_speaker() {
        let lines = [line(1, false, 50.0, "hello")];
        assert!(plan(&lines, &[turn(0.0, 2.0, "SPEAKER_00")]).is_empty());
    }

    #[test]
    fn leaked_words_are_removed_from_my_line() {
        let lines = [
            line(1, true, 0.0, "how are you? I am good thank you"),
            line(2, false, 0.9, "I'm good, thank you."),
        ];

        let changes = plan(&lines, &[]);

        let Some(Change::Text { text, .. }) = changes.first() else { panic!("{changes:?}") };
        assert_eq!(text, "how are you?");
    }

    #[test]
    fn line_that_is_all_leak_is_deleted() {
        let lines = [line(1, true, 22.0, "thank you"), line(2, false, 22.3, "Thank you.")];
        assert_eq!(plan(&lines, &[]), vec![Change::Delete { id: 1 }]);
    }

    #[test]
    fn same_words_far_apart_are_not_a_leak() {
        let lines = [line(1, true, 0.0, "thank you"), line(2, false, 30.0, "thank you")];
        assert!(plan(&lines, &[]).is_empty());
    }

    #[test]
    fn my_own_words_stay() {
        let lines = [line(1, true, 0.0, "did you fix the bugs"), line(2, false, 0.5, "yes all done")];
        assert!(plan(&lines, &[]).is_empty());
    }
}
