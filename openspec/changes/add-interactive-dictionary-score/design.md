# Устройство команды `score`

## Context

В существующем коде `Input` собирает `Word` через `History`. `DictionaryAutoSwitch::comparison` вычисляет сравнение, которое используют словарное решение и debug-лог. `main` сейчас запускает lifecycle службы. Мотивация и входной формат описаны в [proposal](proposal.md).

## Goals / Non-Goals

**Goals**

- Переиспользовать словарный расчёт и конфигурацию, чтобы трассы можно было сверять с работающим свитчером.
- Хранить словари и раскладки в памяти на протяжении сессии, а `History` создавать для каждого запроса.

**Non-Goals**

- Симуляция итогового автозапроса с `switch_rules`, `ambiguous word chain` и состоянием ввода. В таблице проверяется собственный `dscore > threshold`.
- Поддержка `dead key` и композиции в `score` отложена. Поддержку `dead_acute` для команды добавит change [support-dead-acute](../support-dead-acute/proposal.md).

## Decisions

1. **Точка входа.** В `main` разобрать аргументы `score` через `clap` до вызова `run` и чтения файлов. `clap` генерирует `Usage` и `Options` из описания аргументов и обрабатывает справку и ошибки запуска с кодами `0` и `2`. Формат строк stdin, примеры, описание вывода и коды выхода добавить через `after_help`.
   Для скоринга использовать штатную загрузку `config.toml` и словарей. Пара словарей обязательна независимо от значения `auto_switch`. При `--learned-rules false` пропустить чтение `learned-rules.toml` и использовать расчёт без `trie2`, включая случай базового скора из сохранённого примера. `--threshold` задаёт эффективный порог целевой раскладки для всей сессии и применяется при сравнении.
2. **Раскладки.** Один раз прочитать `kxkbrc` и переиспользовать разбор XKB layout, variant, model и options из KDE-модуля. Сопоставлять имена с целями словарей. Если файл отсутствует или в нём нет одной из требуемых раскладок, завершить запуск с кодом `1` и указать путь к файлу или имя раскладки в stderr. Тот же набор настроенных раскладок передать в `Punctuation` и при включённых сохранённых примерах обновить индекс `trie2`. Чтение файла даёт параметры раскладок для воспроизводимой трассы вне запущенной службы. Направление берётся из входной строки.
3. **Восстановление нажатий.** При загрузке построить обратную таблицу символов для поддерживаемых `Stroke` с Shift и без него через `xkb::stroke_text`. Для каждого символа найти `Stroke`, которые печатают его в исходной раскладке. Затем преобразовать найденные `Stroke` в целевой раскладке и объединить нажатия с одинаковым результатом и одинаковой классификацией `strong pnt`. Нумеровать группы с 0 по коду клавиши и Shift первого `Stroke` каждой группы. Собрать все проблемные позиции до скоринга. Позиции считать с 0 по символам Unicode, каждое вхождение указывать отдельно. Одна диагностика показывает символ, позицию и доступные варианты с клавишей, Shift и результатом в целевой раскладке. Если варианты отличаются границей слова, указать `strong pnt` у соответствующего варианта. Сопоставлять индексы с неоднозначными позициями слева направо заново для каждой строки.
   Исключать из вариантов `Stroke`, для которых целевое преобразование возвращает `ConversionError::DeadKey`. Если после исключения таких `Stroke` у символа не осталось вариантов, указать в общей диагностике его позицию и причину: `dead key` в целевой раскладке пока не поддерживается.
4. **Построение трассы.** Передавать нажатия в отдельный `History` в исходной раскладке. Вход должен образовывать одно слово, открытое до добавления первого пробела. Для каждого префикса и добавленного пробела использовать `DictionaryAutoSwitch::comparison`. Предоставить команде доступ к результату сравнения и переиспользовать поля из существующего debug-лога. Значения для сравнения с порогом брать до округления при печати.
5. **Таблица.** Показать направление и эффективный порог над колонками `Text (FROM)`, `Text (TO)`, `Score (FROM)`, `Score (TO)`, `Learned`, `Delta` и `Above threshold`. Колонку `Learned` показывать только в режиме `--learned-rules true`. Пробел отобразить как `␣` в обоих текстовых столбцах. Для бесконечностей использовать `-inf` и `+inf`, для недоступных значений — `n/a`.
6. **Режим сессии.** Определять интерактивность один раз по `stdin.is_terminal()` через `std::io::IsTerminal`. Печатать таблицу в stdout, статус загрузки и ошибки в stderr. При терминальном stdin выводить приглашение `> ` в stderr. Использовать редактор строки, когда stdin и stderr подключены к терминалу. При перенаправленном stderr сохранять приглашение и читать строки напрямую из stdin. В неинтерактивной сессии читать stdin построчно, печатать `END OK` или `END ERROR` в stdout после каждого непустого запроса, включая ошибочный, и сразу делать flush. Все тексты интерфейса писать по-английски.
7. **Код выхода при EOF.** При чтении из файла или pipe запоминать ошибки запросов и после обработки всех строк вернуть `1`, если были ошибки, или `0`, если все запросы выполнены. При чтении с терминала штатный EOF возвращает `0`.
8. **Прогресс загрузки.** До чтения словарей проверить `stdin.is_terminal()` и `stderr.is_terminal()`. Если оба подключены к терминалу, обновлять одну строку в stderr. Показывать имя текущего файла, прогресс-бар и процент прочитанных байтов от размера файла. Учитывать байты, которые потребил парсер, включая разделители строк и записи, пропущенные по `dict_skip_pattern`. На этапе сортировки и построения trie заменить процент на spinner с `Building index for <filename>…`. Ограничить частоту перерисовки, чтобы она не зависела от числа прочитанных строк. При успешной загрузке очистить строку перед `Dictionaries loaded.` и приглашением `> `. При ошибке очистить строку перед диагностикой. Общая загрузка словарей передаёт данные о ходе работы, а отрисовка принадлежит CLI `score`.
9. **История запросов.** Использовать `rustline` с явными файловыми дескрипторами stdin и stderr. Включить однострочный ввод и хранить все непустые отправленные строки в памяти текущей сессии, включая повторы и ошибочные запросы. Редактор поддерживает `↑` и `↓`, позволяет редактировать извлечённый запрос и восстанавливает черновик при возврате за самую новую запись. Приглашение и перерисовка строки идут в stderr, поэтому `score > scores.txt` записывает в файл только таблицы.

Последовательные состояния одной строки прогресса

```text
Reading en_full.txt  [████████░░░░░░░░░░░░]  40%
Reading en_full.txt  [████████████████████] 100%
⠋ Building index for en_full.txt…
Reading ru_full.txt  [█████████████░░░░░░░]  65%
⠹ Building index for ru_full.txt…
```

```console
$ true-switcher score
Dictionaries loaded.
> us ru //tmp
Ambiguous characters in source layout us (target: ru):
  Character index 0, '/':
    0. KEY_SLASH    us '/' → ru '.'
    1. KEY_KPSLASH  us '/' → ru '/'
  Character index 1, '/':
    0. KEY_SLASH    us '/' → ru '.'
    1. KEY_KPSLASH  us '/' → ru '/'
Append one choice index per ambiguous character, from left to right.
> us ru //tmp 1 0
```

Пример stdout при `score -l false -t 2.5` и запросе `us ru bkb` из файла или pipe. Словарь US содержит только `bkbfoo`, словарь RU — только `или`.

```text
us → ru | threshold: 2.50

Text (us)  Text (ru)  Score (us)  Score (ru)  Delta  Above threshold
b          и               0.00        0.00   0.00  no
bk         ил              0.00        0.00   0.00  no
bkb        или             0.00        0.00   0.00  no
bkb␣       или␣            -inf        0.00   +inf  yes
END OK
```

## Пример справки

```text
Score words against the configured dictionaries in a single session.
Read requests from stdin until EOF.

Usage: true-switcher score [OPTIONS]

Options:
  -l, --learned-rules <true|false>  Use learned rules and show the Learned column
                                   [default: true]
  -t, --threshold <NUMBER>         Override the target dictionary's threshold
                                   for this session. Must be finite and positive.
  -h, --help                       Show this help and exit

Request format:
  FROM TO WORD [INDEX ...]
  Use the configured layout names for FROM and TO.
  Append one zero-based choice index per ambiguous character, from left to right.

Examples:
  ru us куищще
  us ru bkb
  us ru //tmp 1 0

Output:
  One score row per prefix, then one for the first space (shown as ␣).
  With terminal input, the prompt is "> ".
  With input from a file or pipe, each response ends with END OK or END ERROR.
  Error details go to stderr.

Exit status:
  0  Help shown or normal completion.
  1  Startup failure or a request error when reading from a file or pipe.
  2  Invalid command-line arguments.

  Request errors are reported immediately. Processing continues until EOF.
```

## Risks / Trade-offs

- Один символ может соответствовать разным физическим клавишам. Нумерация воспроизводима для того же набора XKB-раскладок.
- `Above threshold` показывает сравнение собственного `dscore` с эффективным порогом для отдельного слова.
