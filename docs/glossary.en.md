# Glossary

## General

### `teach` / shift switch

Manual layout correction using `Shift` and the configured `switch_key`. This adds a word to `learned-rules.toml` or increments its count if it already exists.

### `stroke`

A pair consisting of a physical key code (`evdev::KeyCode`) and a flag indicating whether Shift is pressed (`shift: bool`). For example, pressing the `,` key with or without Shift produces two distinct `Stroke` values. The layout is stored separately in `WordStroke` and determines which character a `Stroke` produces.

## Word scoring

The automatic layout switching heuristic uses word scores to decide whether to switch layouts.

### `trie`

- At startup, the application builds a `trie` from the available dictionaries and adds a `␣` node at the end of each word.
- Each node stores `log_2(P)` for its prefix. The terminal `␣` nodes store scores for complete words, excluding their occurrences as prefixes of longer words.
- `T[word␣] <= T[word]`
- `T[word] < 0` because `P < 1` for the prefix.

#### Notation

- `T[word␣]` is the dictionary score for a complete word.
- `T[word]` is the dictionary score for a prefix.

### `trie2`

The `trie2` stores weights derived by summing counts from `learned-rules.toml` on their original scale. As in the `trie`, `␣` marks a complete word. The value at that terminal node serves as a learned adjustment to the dictionary score.

#### Notation

- `T2[word␣]` is the count for a complete word in `learned-rules.toml`.
- `T2[word]` is the sum of counts for words that share the prefix in `learned-rules.toml`.

### word delta-score / prefix delta-score / dscore

A `delta-score` is the score of a word or prefix in the alternative layout minus its score in the current layout. The terms `delta-score` and `dscore` refer to `D[]` or `d[]`, depending on context.

For example, with `ru` as the current layout and `us` as the alternative layout, the same keystrokes produce `цщкв` and `word`, respectively.

- `D[цщкв] = d[цщкв] + d2[цщкв]` is the total delta-score used in the switching decision.
- `d[цщкв] = T[word] - T[цщкв]` is the dictionary delta-score.
- `d2[цщкв] = T2[word] - T2[цщкв]` is the learned adjustment, or the delta-score from `learned-rules.toml`.

In this example, both `word` and `цщкв` are in the dictionary, but `цщкв` has a much lower weight.

- `T[цщкв] << T[word]`
- `d[цщкв] > threshold > 0`

## Punctuation

The `punctuation` setting defines which characters count as punctuation.

```toml
punctuation = '[`~<>\p{P}]'
```

### strong punctuation / strong pnt

Depending on context, `strong punctuation` can refer to either of the following.

- A `Stroke` that produces punctuation in every active layout.
- The character that such a `Stroke` produces in a given layout.

### weak punctuation / weak pnt

Depending on context, `weak punctuation` can refer to either of the following.

- A `Stroke` that produces punctuation in at least one available layout and a character outside the punctuation set in another.
- A punctuation character for which every `Stroke` that produces it in a given layout is weak punctuation.

### semi-strong punctuation / semi-strong pnt

A punctuation character that can be typed in a given layout with either a strong punctuation `Stroke` or a weak punctuation `Stroke`. This distinction matters when indexing text keys in `trie2`, where the original `Stroke` is unknown.
