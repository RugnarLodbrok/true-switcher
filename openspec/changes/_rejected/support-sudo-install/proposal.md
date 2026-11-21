# Запуск установщика через sudo

## Why

Пользователю нужен явный способ запустить install и uninstall через sudo.

## What Changes

Разрешить запуск через sudo при TRUE_SWITCHER_INSTALL_ALLOW_SUDO=1 с сохранением контекста исходного пользователя.

## Capabilities

### Modified Capabilities

- `installation` — Запуск установщика через sudo.

## Impact

Исходная [задача №10](../../../doc/tasks/10.md).
