# Роли `score` и `score_separated`

## Why

Сейчас автосвич использует `score_separated` при оценке кандидатов и повторно при расчёте `dscore`. Этот change закрепляет `score_separated` за выводом команды `score` и debug-логом словарного скоринга.

## What Changes

- `score_separated` разрешено использовать только для команды `score` и debug-лога. Любое другое использование запрещено, включая косвенный вызов из автосвича.
- `score` возвращает итоговый скор ключа. Для выбора варианта используются эти итоговые скоры, а `dscore` вычисляется из итоговых скоров двух победителей.

## Capabilities

### New Capabilities

### Modified Capabilities

- `auto-switch` — закрепляет роли `score` и `score_separated`.

## Impact

Словарный скоринг в `src/dictionary.rs`, debug-лог и вывод команды `score`.
