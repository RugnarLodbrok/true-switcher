# Установка

## ADDED Requirements

### Requirement: Стартовый конфиг источников словарей

При установке True Switcher SHALL копировать `installer/dict-sources.example.toml` в `dict-sources.toml` рядом с пользовательским `config.toml`, если `dict-sources.toml` отсутствует.

#### Scenario: Конфиг источников отсутствует

- **WHEN** к началу установки `dict-sources.toml` отсутствует
- **THEN** установщик создаёт его из примера независимо от наличия `config.toml`

#### Scenario: Конфиг источников уже существует

- **WHEN** к началу установки у пользователя есть `dict-sources.toml`
- **THEN** установщик сохраняет существующий файл и его содержимое
