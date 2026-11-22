# Dict preprocessing

## Why

Пользователю нужен способ исключать нежелательные записи из словарного автосвича через конфиг.

## What Changes

- Добавить общий для обоих словарей параметр `dict_skip_pattern = ''` в корень `config.toml`.
- При загрузке словарей пропускать записи, в которых regex совпал со словом целиком.
- Убрать встроенный пропуск слов с заглавными буквами. При отсутствующем или пустом `dict_skip_pattern` фильтрация слов отключена.

## Capabilities

### Modified Capabilities

- `auto-switch` получит настраиваемый фильтр слов при загрузке словарей.

## Impact

Изменение затрагивает `Configuration`, `DictionaryAutoSwitch::load`, `DictionaryTrie::read` и описание конфигурации в `README.md`.
