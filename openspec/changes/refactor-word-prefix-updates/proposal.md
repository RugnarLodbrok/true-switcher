# Proposal

## Why

При добавлении символа `History` записывает stroke, а `Input` отдельным вызовом обновляет стеки словарных префиксов. Backspace обновляет обе части в `History`. Согласованность зависит от двух разных путей редактирования.

## What Changes

- Дать истории единый путь добавления и удаления stroke, который обновляет и `entries`, и состояние префиксов.

## Capabilities

### New Capabilities

### Modified Capabilities

## Impact

`src/history.rs`, `src/word.rs`, `src/input.rs` и `src/dictionary.rs`. Выполнять третьим, после `refactor-dictionary-comparison`, чтобы отдельно проверить изменения пути ввода.
