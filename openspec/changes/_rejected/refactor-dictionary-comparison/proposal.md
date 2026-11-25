# Proposal

## Why

При включённом debug logging один и тот же `DictionaryComparison` вычисляется отдельно для лога и для dictionary decision. Это повторяет scoring обеих раскладок, включая обработку начальной пунктуации.

## What Changes

- Переиспользовать один результат сравнения в пределах события ввода. Выполнять расчёт по требованию от logging или dictionary decision.

## Capabilities

### New Capabilities

### Modified Capabilities

## Impact

`src/input.rs`, `src/dictionary.rs` и тесты словарного scoring. Выполнять после `refactor-auto-switch-decision`.
