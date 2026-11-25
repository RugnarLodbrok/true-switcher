# Proposal

## Why

Mouse pause разнёс решение об автосвиче по двум методам `Input`. Оценка первого пробела во время паузы нужна для сохранения ambiguity, которую использует multiword correction.

## What Changes

- Вынести выбор результата по pattern rules и словарям в `AutoSwitch`. Применять результат к истории и `PendingSwitch` в `Input`.

## Capabilities

### New Capabilities

### Modified Capabilities

## Impact

`src/input.rs`, `src/switch_rules.rs` и сценарии автосвича.
