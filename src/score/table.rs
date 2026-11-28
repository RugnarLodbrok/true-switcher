use std::fmt::Write as _;

use crate::dictionary::DictionaryComparison;

pub(super) const UNAVAILABLE: &str = "n/a";
const VISIBLE_SPACE: &str = "␣";
const COLUMNS: usize = 7;
const LEARNED_COLUMN: usize = 4;

pub(super) struct ScoreTable<'a> {
    from: &'a str,
    to: &'a str,
    threshold: f64,
    rows: Vec<[String; COLUMNS]>,
}

impl<'a> ScoreTable<'a> {
    pub(super) fn new(from: &'a str, to: &'a str, threshold: f64) -> Self {
        let headers = [
            format!("Text ({from})"),
            format!("Text ({to})"),
            format!("Score ({from})"),
            format!("Score ({to})"),
            "Learned".into(),
            "Delta".into(),
            "Above threshold".into(),
        ];
        Self {
            from,
            to,
            threshold,
            rows: vec![headers],
        }
    }

    pub(super) fn push(
        &mut self,
        from_text: &str,
        to_text: Option<&str>,
        is_word_end: bool,
        comparison: Option<DictionaryComparison>,
    ) {
        let [from_score, to_score, learned, delta, above_threshold] = comparison.map_or_else(
            || std::array::from_fn(|_| UNAVAILABLE.into()),
            |comparison| score_cells(&comparison, self.threshold),
        );
        self.rows.push([
            format_text(Some(from_text), is_word_end),
            format_text(to_text, is_word_end),
            from_score,
            to_score,
            learned,
            delta,
            above_threshold,
        ]);
    }

    pub(super) fn render(&self, is_learned_enabled: bool) -> String {
        let threshold = format_score(self.threshold);
        let threshold = if threshold.parse::<f64>() == Ok(self.threshold) {
            threshold
        } else {
            self.threshold.to_string()
        };
        let mut output = format!("{} → {} | threshold: {threshold}\n\n", self.from, self.to);
        let mut widths = [0; COLUMNS];
        for row in &self.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        for row in &self.rows {
            for (index, (cell, width)) in row.iter().zip(widths).enumerate() {
                if index == LEARNED_COLUMN && !is_learned_enabled {
                    continue;
                }
                match index {
                    0 | 1 => write!(output, "{cell:<width$}  ").unwrap(),
                    index if index == COLUMNS - 1 => writeln!(output, "{cell}").unwrap(),
                    _ => write!(output, "{cell:>width$}  ").unwrap(),
                }
            }
        }
        output
    }
}

fn score_cells(comparison: &DictionaryComparison, threshold: f64) -> [String; 5] {
    let (ordering, learned) = comparison.learned_difference();
    let sign = match ordering {
        std::cmp::Ordering::Less => "-",
        std::cmp::Ordering::Equal => "",
        std::cmp::Ordering::Greater => "+",
    };
    let is_above_threshold = if comparison.delta_score > threshold {
        "yes"
    } else {
        "no"
    };
    [
        format_score(comparison.current_base_score),
        format_score(comparison.alternative_base_score),
        format!("{sign}{learned}"),
        format_score(comparison.delta_score),
        is_above_threshold.into(),
    ]
}

fn format_text(text: Option<&str>, is_word_end: bool) -> String {
    match text {
        Some(text) if is_word_end => format!("{text}{VISIBLE_SPACE}"),
        Some(text) => text.to_owned(),
        None => UNAVAILABLE.into(),
    }
}

fn format_score(score: f64) -> String {
    if score.is_infinite() {
        format!("{score:+}")
    } else {
        format!("{score:.2}")
    }
}
