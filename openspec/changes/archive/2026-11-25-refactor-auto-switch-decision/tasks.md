# Tasks

## 1. Выбор и применение результата

- [x] 1.1 Вынести оценку в `AutoSwitch` и подключить её в `Input`. Запустить `cargo test` с фильтрами `switch_rules::`, `first_space` и `ambiguity`, чтобы проверить порядок pattern rules, обратную проверку и dictionary guards.
- [x] 1.2 Применять ambiguity и pending request в `Input` с учётом mouse pause. Запустить `cargo test` с фильтрами `mouse_motion`, `ambiguous_chain` и `manual`, включая сценарий `mouse_motion_pause_preserves_ambiguity_for_next_dictionary_switch`.
