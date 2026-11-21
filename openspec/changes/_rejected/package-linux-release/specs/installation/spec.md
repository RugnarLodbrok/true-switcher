## ADDED Requirements

### Requirement: Linux release-архив

Release-скрипт SHALL собирать `.tar.gz` с файлом версии, `target/release/true-switcher`, `installer/installer.sh`, `installer/installer.py`, `icon/icon.svg` и `icon/icon_error.svg`.

#### Scenario: Linux release-архив

- **WHEN** пользователь распаковывает Linux release
- **THEN** готовый бинарник и обе части установщика доступны по ожидаемым путям
