## ADDED Requirements

### Requirement: Запуск установщика через sudo

True Switcher SHALL разрешить запуск через sudo при TRUE_SWITCHER_INSTALL_ALLOW_SUDO=1 с сохранением контекста исходного пользователя.

#### Scenario: Запуск установщика через sudo

- **WHEN** установщик вызван через sudo с opt-in
- **THEN** пользовательские файлы и операции systemctl --user относятся к исходному пользователю
