# Защита при dead key

## Why

Текущая защита отменяет коррекцию. Требуется корректно обрабатывать составной ввод с dead_acute.

## What Changes

Учитывать композицию dead_acute при ручной и автоматической коррекции, сохраняя защиту для неподдержанных dead keys.

Поддержать композицию `dead_acute` в трассах словарного скоринга команды `true-switcher score` из change [add-interactive-dictionary-score](../add-interactive-dictionary-score/proposal.md).

## Capabilities

### Modified Capabilities

- `layout-correction` — Защита при dead key.

## Impact

Исходная [задача №30](../../../doc/tasks/30.md).
