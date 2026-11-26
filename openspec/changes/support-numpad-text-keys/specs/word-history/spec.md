# Spec Delta

## MODIFIED Requirements

### Requirement: История слова

True Switcher SHALL сохранять физические клавиши, Shift, раскладку и завершающие пробелы для коррекции. Нажатия `KEY_KPSLASH`, `KEY_KPASTERISK`, `KEY_KPMINUS`, `KEY_KPPLUS`, `KEY_KPEQUAL`, `KEY_KPPLUSMINUS`, `KEY_KPCOMMA`, `KEY_KPJPCOMMA`, `KEY_KPLEFTPAREN` и `KEY_KPRIGHTPAREN` SHALL входить в слово при обоих состояниях Num Lock и участвовать в проверке автосвича по XKB. `KEY_KP0`–`KEY_KP9` и `KEY_KPDOT` SHALL оставаться за пределами истории слова.

#### Scenario: История слова

- **WHEN** пользователь вводит слово, затем несколько пробелов
- **THEN** коррекция воспроизводит регистр и пробелы

#### Scenario: Numpad Divide внутри слова

- **WHEN** пользователь вводит `гы/кг` в RU с `/` на клавише Numpad Divide
- **THEN** проверка автосвича получает `гы/кг` в RU и `us/ru` в US
- **AND** словарная запись `usrunaway` сама по себе не даёт оснований для автосвича

#### Scenario: Клавиши с режимом навигации

- **WHEN** пользователь нажимает одну из клавиш `KEY_KP0`–`KEY_KP9` или `KEY_KPDOT`
- **THEN** нажатие не добавляется в слово при любом состоянии Num Lock
