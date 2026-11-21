## ADDED Requirements

### Requirement: Установка KWin-скрипта

True Switcher SHALL устанавливать и включать true-switcher-focus для поддерживаемого окружения.

#### Scenario: Установка KWin-скрипта

- **WHEN** предварительная проверка подтвердила совместимость Plasma 6 и наличие инструментов
- **THEN** установщик устанавливает пакет через kpackagetool6, включает плагин и применяет конфигурацию KWin
