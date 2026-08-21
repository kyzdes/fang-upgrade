#!/usr/bin/env python3
"""Самопроверка docs/subagent-report.schema.json.

Схема, которую не проверили на ОТКЛОНЕНИИ, — это тавтология в другой форме: она
принимает всё и потому ничего не доказывает. Ровно тот же дефект, что тест,
который не может упасть (за пять спринтов — 10 раз).

Прогон делает пять вещей:
  1. проверяет, что файл — валидная JSON Schema (Draft 2020-12);
  2. проводит через неё полный отчёт — он ОБЯЗАН быть принят;
  3. проводит набор заведомо неполных, противоречивых или отписочных отчётов —
     каждый ОБЯЗАН быть отклонён, и отклонён ИМЕННО ТЕМ правилом, против
     которого он написан: у каждого случая указан кусок ожидаемого сообщения,
     и «отклонён не тем правилом» считается провалом наравне с «принят».
     Иначе первый же промах в required прикрыл бы дыру в межполевых правилах;
  4. сверяет `required` схемы со списком обязательных полей в блоке D
     docs/subagent-task-template.md — расхождение в любую сторону есть дефект;
  5. сверяет docs/subagent-report.schema.compact.json с полной схемой: и по
     содержимому (компактная обязана быть полной со снятыми аннотациями), и по
     вердикту на всех фикстурах ниже.

    python3 docs/subagent-report.schema.check.py                # 0 = схема гейт
    python3 docs/subagent-report.schema.check.py --write-compact

ЧЕГО ЭТОТ ПРОГОН НЕ ДОКАЗЫВАЕТ. Он не доказывает, что попавший в отчёт блок
ofgate настоящий. Схема сверяет форму блока со строками, которые ofgate
действительно печатает, — и только. Блок нужного вида пишется руками за минуту:
ключа у схемы нет, sha256 из закрывающей строки ей не с чем сверить, тело блока
и есть весь её вход. Подлинность доказывается перезапуском гейта (ofverify), и
фикстура OFGATE_GREEN ниже — образец формы, а не удостоверение подлинности.

Числа в этом докстринге не проставлены руками: сколько случаев в наборе, столько
и печатает прогон.
"""
import copy
import io
import json
import os
import re
import sys

try:
    from jsonschema import Draft202012Validator
except ImportError:
    sys.exit("нужен python3-jsonschema (проверено на 4.10.3): pip install jsonschema")

HERE = os.path.dirname(os.path.abspath(__file__))
SCHEMA_PATH = os.path.join(HERE, "subagent-report.schema.json")
COMPACT_PATH = os.path.join(HERE, "subagent-report.schema.compact.json")
TEMPLATE_PATH = os.path.join(HERE, "subagent-task-template.md")

# Аннотации: ничего не утверждают о документе, только описывают схему.
# Компактная версия — это полная без них.
ANNOTATIONS = ("description", "$comment", "title", "examples")

OFGATE_GREEN = """=== OFGATE VERDICT ===
worktree   : /root/ofgate-canary
commit     : 6a2c37e600d2db41714acc923b641447ba286066 (dirty=no)
image      : ofgate-build:1.91 sha256:1dc3d2cca3f30bad2dcf80e3b4e8a21b0222128b35a68b4f49ca18b39dc77d03
toolchain  : rust-toolchain.toml=1.91 · в образе: rustc 1.91.1 (ed61e7d7e 2025-11-07)
target-vol : fang-target-ofgate-canary-
checks     : fmt,clippy,test  (порядок CI, останов на первой красной)
started    : 2026-08-21T10:52:27Z
logs       : /var/tmp/ofgate/ofgate-canary-/20260821T105225Z-986317
--- fmt : GREEN  exit=0  3s
      cmd : cargo fmt --all -- --check
      log : /var/tmp/ofgate/ofgate-canary-/20260821T105225Z-986317/fmt.log  (0 B, sha256:e3b0c44298fc1c14)
--- clippy : GREEN  exit=0  433s
      cmd : cargo clippy --workspace --all-targets -- -D warnings
      log : /var/tmp/ofgate/ofgate-canary-/20260821T105225Z-986317/clippy.log  (22172 B, sha256:7c0e254a7e66482c)
--- test : GREEN  exit=0  812s
      cmd : cargo test --workspace -- --test-threads=2
      log : /var/tmp/ofgate/ofgate-canary-/20260821T105225Z-986317/test.log  (202664 B, sha256:c5c16f376e82d3a0)
result     : GREEN — все запрошенные проверки прошли
elapsed    : 1248s
exit       : 0
=== END OFGATE VERDICT run=20260821T105225Z-986317 body-sha256=a98ee3e8eb6141e39b045d19a4ee5e77 ==="""

OFGATE_RED = (OFGATE_GREEN
              .replace("--- clippy : GREEN  exit=0  433s",
                       "--- clippy : RED  exit=101  433s")
              .replace("result     : GREEN — все запрошенные проверки прошли",
                       "result     : RED — упала проверка: clippy")
              .replace("exit       : 0", "exit       : 1"))

# Реальный отказ ofgate от переднего плана (прогон 2026-08-21, 882 знака, 11 строк).
OFGATE_REFUSAL = """ofgate: отказываюсь идти на переднем плане: test (измерено 812 с только на этом шаге).
  Потолок timeout у Bash — 600000 мс; запрос больше молча обрезается до десяти
  минут, и вызов вернёт "Command timed out after 10m 0s", хотя команда жива.

  Запусти фоном (Bash run_in_background: true), команда целиком:

    sh /root/.claude/skills/openfang/scripts/ofgate /root/src/openfang --only test --wait

  Короткие режимы, которым передний план разрешён:
    sh /root/.claude/skills/openfang/scripts/ofgate /root/src/openfang --only fmt"""

OFMUTATE_PROVEN = """# worktree /root/wt-fang-13
# HEAD 1a2b3c4d5  база ours  фильтр 'empty_response'
# ханков: 2 продакшн, 1 тестовых
    прод  crates/openfang-runtime/src/agent.rs  @@+118,9
    тест  crates/openfang-runtime/src/agent.rs  @@+402,24
# логи: /tmp/ofmutate-xxxx

[1/2] как есть — обязано быть зелёным и непустым
  как есть: GREEN  passed=1 failed=0  35s  → /tmp/ofmutate-xxxx/green.log

[2/2] продакшн-ханки откачены (2 шт.) — обязано покраснеть
  без патча: RED-ASSERT  passed=0 failed=1  41s  → /tmp/ofmutate-xxxx/red-all.log

=== вердикт ===
ДОКАЗАНО (RED-ASSERT): с патчем 1 passed; без него 1 failed. Тест смотрит на поведение патча.
логи прогонов: /tmp/ofmutate-xxxx"""

OFMUTATE_TAUTOLOGY = OFMUTATE_PROVEN.replace(
    "ДОКАЗАНО (RED-ASSERT): с патчем 1 passed; без него 1 failed. "
    "Тест смотрит на поведение патча.",
    "ТАВТОЛОГИЯ: без патча тест всё равно проходит (1 passed). Тест не проверяет патч.")

OFMUTATE_EMPTY = (OFMUTATE_PROVEN
                  .replace("  как есть: GREEN  passed=1 failed=0",
                           "  как есть: GREEN  passed=0 failed=0")
                  .split("[2/2]")[0]
                  + "ОТКАЗ: фильтр 'empty_response' не выбрал ни одного теста "
                    "(passed=0). Зелёный пустой прогон — это тавтология, "
                    "а не доказательство.")

GOOD = {
    "worktree": "/root/wt-fang-13",
    "branch": "fix/fang-13-empty-response",
    "head": "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
    "base": "ours",
    "model": "sonnet",
    "effort": "medium",
    "gate": OFGATE_GREEN,
    "mutate": OFMUTATE_PROVEN,
    "red_before_green": {
        "command": "ofmutate /root/wt-fang-13 --test empty_response -p openfang-runtime",
        "output": "test agent::tests::empty_response_is_retried ... FAILED\n"
                  "assertion `left == right` failed\n  left: 0\n right: 1",
        "exit_code": 101,
    },
    "claims": [{
        "claim": "пустой ответ провайдера повторяется один раз",
        "proof_command": "ofmutate /root/wt-fang-13 --test empty_response -p openfang-runtime",
        "proof_output": "ДОКАЗАНО (RED-ASSERT): с патчем 1 passed; без него 1 failed",
    }],
    "claims_without_proof": [],
    "findings": ["в задаче сказано routes.rs, метод висит цепочкой в server.rs"],
    "tried_and_rejected": ["ретрай в провайдере — задел бы все вызовы, не только пустые"],
    "cleanup": {
        "prefix": "fang13-",
        "containers": ["fang13-probe — удалён"],
        "volumes": ["fang-target-wt-fang-13- — оставлен, патч не влит"],
        "worktree_left": True,
    },
    "unverified": ["живой прогон против прода не делал — прод трогать нельзя"],
    "done": True,
    "summary": "пустой ответ провайдера повторяется, доказано RED-ASSERT",
}


def drop(key):
    """Убрать поле. Именно функцией, а не `r.pop(k) and r`: pop пустого списка
    возвращает ложь, и такой отчёт превращался бы в пустой список — отклонялся,
    но не тем правилом, которое проверяют."""
    def f(r):
        r.pop(key, None)
        return r
    return f


def drop_many(*keys):
    def f(r):
        for k in keys:
            r.pop(k, None)
        return r
    return f


def setk(**kw):
    def f(r):
        r.update(kw)
        return r
    return f


def compose(*fs):
    def f(r):
        for g in fs:
            r = g(r)
        return r
    return f


# (имя, мутация отчёта, что именно этот случай ловит, ожидаемое сообщение)
# Четвёртый элемент обязателен: он и делает набор доказательством. Без него
# случай «отклонён вообще» засчитывался бы за «отклонён этим правилом».
# Это кусок сообщения об ошибке; список кусков означает «сработали все
# перечисленные правила», а не «хотя бы одно».
BAD = [
    # --- полнота: отсутствующие поля -----------------------------------------
    ("пустой отчёт",
     lambda r: {},
     "ни одного обязательного поля",
     "'worktree' is a required property"),
    ("нет gate",
     drop("gate"),
     "ofgate не прогнан, и об этом молчат",
     "'gate' is a required property"),
    ("нет mutate",
     drop("mutate"),
     "молчание об ofmutate — ровно то, что случалось в спринтах 1-5",
     "'mutate' is a required property"),
    ("нет red_before_green",
     drop("red_before_green"),
     "зелёный тест без записанного красного",
     "'red_before_green' is a required property"),
    ("нет unverified",
     drop("unverified"),
     "самый часто пропускаемый пункт приёмки",
     "'unverified' is a required property"),
    ("нет claims_without_proof",
     drop("claims_without_proof"),
     "фразы без прогона не перечислены",
     "'claims_without_proof' is a required property"),
    ("нет done",
     drop("done"),
     "без done все межполевые правила обходятся молчанием",
     "'done' is a required property"),
    ("нет summary",
     drop("summary"),
     "итог первой строкой — пункт приёмки, а не любезность",
     "'summary' is a required property"),
    # --- ДЕФЕКТ 3: документ обещал эти поля, а required их не требовал --------
    ("АТАКА К1.5: нет effort, нет claims, нет tried_and_rejected",
     drop_many("effort", "claims", "tried_and_rejected"),
     "блок D называл их обязательными, а в required их не было",
     "'effort' is a required property"),
    ("нет claims (поле карты «утверждение → прогон»)",
     drop("claims"),
     "фразы добавлены, прогонов к ним нет",
     "'claims' is a required property"),
    ("нет tried_and_rejected",
     drop("tried_and_rejected"),
     "отвергнутый путь ведущий пройдёт заново",
     "'tried_and_rejected' is a required property"),
    # --- форма gate ----------------------------------------------------------
    ("gate пересказан по памяти",
     setk(gate="Прогнал ofgate, всё зелёное: fmt, clippy, test."),
     "нет ни одной строки, которую ofgate печатает",
     "is too short"),
    ("gate обрезан до result",
     setk(gate=OFGATE_GREEN.split("=== END")[0]),
     "нет закрывающей строки — блок неполон",
     "=== END OFGATE VERDICT run="),
    ("АТАКА К1.2: gate из четырёх строк, sha256 = тридцать два нуля",
     setk(gate="=== OFGATE VERDICT ===\nresult     : GREEN — ок\nexit       : 0\n"
                "=== END OFGATE VERDICT run=20260821T105225Z-986317 "
                "body-sha256=00000000000000000000000000000000 ==="),
     "нет строк worktree/commit/logs и ни одного шага с sha256 лога",
     "is too short"),
    ("gate: все строки на месте, но sha256 = тридцать два нуля",
     setk(gate=OFGATE_GREEN.replace("a98ee3e8eb6141e39b045d19a4ee5e77",
                                    "00000000000000000000000000000000")),
     "тридцать два одинаковых знака — заполнитель, а не sha256",
     "body-sha256=(.)"),
    ("gate: шаг без размера и sha256 лога",
     setk(gate=OFGATE_GREEN.replace("  (0 B, sha256:e3b0c44298fc1c14)", "")
                           .replace("  (22172 B, sha256:7c0e254a7e66482c)", "")
                           .replace("  (202664 B, sha256:c5c16f376e82d3a0)", "")),
     "строки шага печатает ofgate, их отсутствие — признак сочинения",
     "B, sha256:"),
    ("gate: маркеры переставлены (контроль — валидатор не сломан)",
     setk(gate=OFGATE_GREEN.replace("=== OFGATE VERDICT ===", "=== OFGATE TCIDREV ===")),
     "контрольный случай: испорченный маркер обязан отклоняться",
     "=== OFGATE VERDICT ==="),
    ("АТАКА К1.3: gate refused, output = 'ofgate:' — семь знаков",
     setk(gate={"status": "refused", "exit_code": 2, "output": "ofgate:"}),
     "семь знаков выводом отказа не являются: реальные отказы 882 и 464 знака",
     "is too short"),
    ("gate refused: вывод однострочный",
     setk(gate={"status": "refused", "exit_code": 3,
                "output": "ofgate: свободно 3G, этому прогону нужно 22G. "
                          "удали тома влитых патчей и повтори прогон, это отказ "
                          "инструмента, а не дефект дерева."}),
     "оба отказа ofgate многострочны",
     "does not match"),
    ("gate refused, а unverified пуст",
     compose(setk(gate={"status": "refused", "exit_code": 2, "output": OFGATE_REFUSAL}),
             setk(unverified=[],
                  unverified_empty_because="проверено всё, что вообще можно было проверить")),
     "отказ гейта означает непрогнанные проверки — это и есть непроверенное",
     "[] is too short"),
    # --- форма mutate --------------------------------------------------------
    ("mutate прозой вместо вердикта",
     setk(mutate="ofmutate прогонял, тест краснеет без патча"),
     "нет ни '# worktree', ни строки прогона — доказательства нет",
     "is too short"),
    ("mutate: одна строка '# HEAD ... ТАВТОЛОГИЯ'",
     setk(mutate="# HEAD abcdef01 ТАВТОЛОГИЯ"),
     "старая схема принимала это как дословный вывод инструмента",
     "is too short"),
    ("mutate: not_applicable с опечаткой в ключе",
     setk(mutate={"not_aplicable": "правка только документации"}),
     "additionalProperties:false — опечатка не проскакивает как молчание",
     "'not_applicable' is a required property"),
    ("АТАКА К1.4: mutate.not_applicable = двадцать одинаковых букв",
     setk(mutate={"not_applicable": "ааааааааааааааааааaa"},
          red_before_green={"not_applicable": "патч без тестов, правка только документации"}),
     "двадцать знаков есть, причины нет",
     "does not match"),
    ("mutate.not_applicable = 'не применимо'",
     setk(mutate={"not_applicable": "не применимо"},
          red_before_green={"not_applicable": "патч без тестов, правка только документации"}),
     "причина, а не отписка: минимум двадцать знаков и три слова",
     "is too short"),
    # --- форма red_before_green ---------------------------------------------
    ("red_before_green.output однострочный",
     setk(red_before_green={"command": "cargo test -p openfang-runtime empty_response",
                            "output": "тест падал, я это видел глазами"}),
     "вывод прогона многострочен; однострочная фраза — пересказ",
     "does not match"),
    ("red_before_green доказан командой `true`",
     setk(red_before_green={"command": "true",
                            "output": "FAILED\nassertion failed\n  left: 0\n right: 1"}),
     "команда, которая не может упасть, ничего не доказывает",
     "is too short"),
    # --- ДЕФЕКТ 1: отписки и сочинённые доказательства -----------------------
    ("АТАКА К1.1: отписка во всех полях, gate и mutate сочинены",
     lambda r: {
         "worktree": "/x", "branch": "x", "head": "abcdef0", "base": "x",
         "model": "opus", "effort": "high",
         "gate": "=== OFGATE VERDICT ===\nбла\nresult : GREEN\nexit : 0\n"
                 "=== END OFGATE VERDICT run=r body-sha256="
                 "00000000000000000000000000000000 ===",
         "mutate": "# HEAD abcdef01 ТАВТОЛОГИЯ",
         "red_before_green": {"command": "x", "output": "x" * 20},
         "claims": [], "claims_without_proof": [], "findings": ["x"],
         "tried_and_rejected": ["x"],
         "cleanup": {"prefix": "x", "containers": [], "volumes": [],
                     "worktree_left": False},
         "unverified": ["x"], "done": True, "summary": "x",
     },
     "отписка в каждом поле: однословные пункты, сочинённые блоки",
     "is too short"),
    ("АТАКА К1.6: claim доказан командой `true` и выводом 'ок'",
     setk(claims=[{"claim": "схема ловит подделку", "proof_command": "true",
                   "proof_output": "ок"}]),
     "команда, которая не может упасть, и вывод из двух знаков",
     "is too short"),
    ("claim доказан `echo`",
     setk(claims=[{"claim": "все три поверхности проверены",
                   "proof_command": "echo проверено на всех трёх поверхностях",
                   "proof_output": "проверено на всех трёх поверхностях"}]),
     "echo печатает то, что ему дали, и не читает систему",
     "(true|false|:|exit"),
    ("АТАКА К2.3: unverified = [' '] — один пробел",
     setk(unverified=[" "]),
     "пробел в списке — это пустой список, объявленный непустым",
     "is too short"),
    ("unverified = ['нет'] — одно слово",
     setk(unverified=["нет"]),
     "пункт списка обязан быть фразой, а не отпиской",
     "is too short"),
    ("unverified пуст без объяснения",
     setk(unverified=[]),
     "пустой список требует unverified_empty_because",
     "'unverified_empty_because' is a required property"),
    ("unverified_empty_because = отписка",
     setk(unverified=[], unverified_empty_because="всё проверено"),
     "объяснение из двух слов объяснением не является",
     "is too short"),
    ("cleanup без личного префикса",
     setk(cleanup={"containers": [], "volumes": [], "worktree_left": False}),
     "чужие контейнеры дважды за период были прочитаны как находка",
     "'prefix' is a required property"),
    ("модель не из таблицы",
     setk(model="opus-max"),
     "модель выбрана в моменте, а не по типу задачи",
     "is not one of ['haiku', 'sonnet', 'opus']"),
    ("effort не из таблицы",
     setk(effort="максимальный"),
     "уровень усилия задаётся ведущим по таблице",
     "is not one of ['low', 'medium', 'high']"),
    # --- ДЕФЕКТ 2: отчёт, который сам себя объявляет негодным ----------------
    ("АТАКА К2.1: gate RED + mutate ТАВТОЛОГИЯ + «красного не было вовсе» + done:true",
     setk(done=True, gate=OFGATE_RED, mutate=OFMUTATE_TAUTOLOGY,
          red_before_green={"not_applicable": "красного не было вовсе, тест сразу зелёный"}),
     "отчёт объявляет себя выполненным и негодным одновременно",
     ["result +: RED'}", "'pattern': 'ТАВТОЛОГИЯ'", "сразу"]),
    ("done:true при красном гейте",
     setk(done=True, gate=OFGATE_RED),
     "красный гейт — это невыполненное задание, а не деталь",
     "result +: RED'}"),
    ("done:true при вердикте ТАВТОЛОГИЯ",
     setk(done=True, mutate=OFMUTATE_TAUTOLOGY),
     "тавтология — самая частая ошибка периода, 10 раз за пять спринтов",
     "'pattern': 'ТАВТОЛОГИЯ'"),
    ("done:true при passed=0 (фильтр не выбрал теста)",
     setk(done=True, mutate=OFMUTATE_EMPTY),
     "зелёный пустой прогон — тавтология в чистом виде",
     "'pattern': 'passed=0"),
    ("АТАКА К2.2: claims пуст, claims_without_proof непуст, done:true",
     setk(done=True, claims=[],
          claims_without_proof=["схема ловит подделку блока ofgate",
                                "гейт подключён к воркфлоу-скрипту"]),
     "«нет прогона — нет фразы» либо действует, либо его нет",
     "is too long"),
    ("red_before_green.not_applicable отрицает сам себя",
     setk(red_before_green={"not_applicable": "красного не было, я его не записывал"},
          mutate={"not_applicable": "патч без тестов, правка только документации"}),
     "признание, а не причина неприменимости",
     "сразу"),
    ("red_before_green.not_applicable = 'не запускал до правки'",
     setk(red_before_green={"not_applicable": "не запускал тест до правки, было некогда"},
          mutate={"not_applicable": "патч без тестов, правка только документации"}),
     "то же самоотрицание другими словами",
     "сразу"),
    ("ofmutate прогнан, а red_before_green объявлен неприменимым",
     setk(red_before_green={"not_applicable": "патч без тестов, правка только документации"}),
     "прогон, доказавший красное, и есть эта запись — противоречие",
     "'command' is a required property"),
]


def all_messages(validator, doc):
    """Все сообщения об ошибках, включая вложенные в anyOf/oneOf/if-then."""
    out = []

    def walk(errs):
        for e in errs:
            where = "/".join(str(p) for p in e.absolute_path) or "<корень>"
            out.append((where, e.message.replace("\n", " ")))
            walk(e.context or [])

    walk(sorted(validator.iter_errors(doc), key=lambda e: list(e.absolute_path)))
    return out


def first_error(validator, doc):
    msgs = all_messages(validator, doc)
    if not msgs:
        return None
    where, msg = msgs[0]
    return f"{where}: {msg[:160]}"


def strip_annotations(node):
    """Компактная схема = полная без аннотаций. description/$comment/title/
    examples ничего не утверждают о документе, поэтому снятие их вердикта
    менять не может; что не меняет — проверяется шагом [5], а не обещается."""
    if isinstance(node, dict):
        return {k: strip_annotations(v) for k, v in node.items()
                if k not in ANNOTATIONS}
    if isinstance(node, list):
        return [strip_annotations(v) for v in node]
    return node


def compact_text(schema):
    return json.dumps(strip_annotations(schema), ensure_ascii=False,
                      separators=(",", ":")) + "\n"


def required_from_template():
    """Список обязательных полей из блока D шаблона: строки вида
    '  <имя>  <описание>' между заголовком «Обязательные поля» и «Условные поля»."""
    text = io.open(TEMPLATE_PATH, encoding="utf-8").read()
    lines = text.splitlines()
    start = end = None
    for i, ln in enumerate(lines):
        if start is None and ln.startswith("Обязательные поля"):
            start = i
        elif start is not None and ln.startswith("Условные поля"):
            end = i
            break
    if start is None or end is None:
        return None
    names = []
    for ln in lines[start + 1:end]:
        m = re.match(r"^  ([a-z_0-9]+) ", ln)
        if m:
            names.append(m.group(1))
    return names


def main():
    write_compact = "--write-compact" in sys.argv
    with io.open(SCHEMA_PATH, encoding="utf-8") as fh:
        schema = json.load(fh)

    print(f"схема: {SCHEMA_PATH}")
    Draft202012Validator.check_schema(schema)
    print("[1] check_schema: валидная JSON Schema (Draft 2020-12)\n")

    v = Draft202012Validator(schema)
    failures = 0

    err = first_error(v, GOOD)
    if err is None:
        print("[2] полный отчёт: ПРИНЯТ\n")
    else:
        print(f"[2] полный отчёт: ОТКЛОНЁН, а не должен быть — {err}\n")
        failures += 1

    print(f"[3] {len(BAD)} негодных отчётов — каждый обязан быть отклонён "
          f"ИМЕННО своим правилом:")
    for name, mutate, why, expect in BAD:
        doc = mutate(copy.deepcopy(GOOD))
        msgs = all_messages(v, doc)
        if not msgs:
            print(f"  ПРОПУЩЕН (это дефект схемы!)  {name}")
            failures += 1
            continue
        wanted = expect if isinstance(expect, list) else [expect]
        hits, missing = [], []
        for frag in wanted:
            found = [m for _, m in msgs if frag in m]
            (hits.append(found[0]) if found else missing.append(frag))
        if missing:
            print(f"  НЕ ТЕМ ПРАВИЛОМ (дефект набора или схемы!)  {name}")
            print(f"            не нашли в сообщениях: {missing!r}")
            print(f"            получили: {first_error(v, doc)}")
            failures += 1
            continue
        print(f"  отклонён  {name}  (правил сработало: {len(hits)})")
        print(f"            → {hits[0][:150]}")
        print(f"            ловит: {why}")

    print(f"\n[4] required схемы против блока D в {os.path.basename(TEMPLATE_PATH)}:")
    tmpl = required_from_template()
    req = list(schema["required"])
    if tmpl is None:
        print("  блок «Обязательные поля ... Условные поля» в шаблоне не найден")
        failures += 1
    elif tmpl != req:
        print(f"  РАСХОЖДЕНИЕ — в схеме: {req}")
        print(f"              в шаблоне: {tmpl}")
        print(f"  только в схеме: {sorted(set(req) - set(tmpl))}")
        print(f"  только в шаблоне: {sorted(set(tmpl) - set(req))}")
        failures += 1
    else:
        print(f"  совпадают знак в знак, {len(req)} полей: {' '.join(req)}")

    print(f"\n[5] компактная схема {os.path.basename(COMPACT_PATH)}:")
    want = compact_text(schema)
    if write_compact:
        io.open(COMPACT_PATH, "w", encoding="utf-8").write(want)
        print(f"  записана заново ({len(want)} знаков)")
    if not os.path.exists(COMPACT_PATH):
        print("  файла нет — собери его: --write-compact")
        failures += 1
    else:
        have = io.open(COMPACT_PATH, encoding="utf-8").read()
        if have != want:
            print("  РАСХОЖДЕНИЕ с полной схемой — пересобери: --write-compact")
            failures += 1
        else:
            print(f"  содержимое: полная со снятыми аннотациями, знак в знак "
                  f"({len(have)} знаков против {os.path.getsize(SCHEMA_PATH)} у полной)")
        cs = json.loads(have)
        Draft202012Validator.check_schema(cs)
        cv = Draft202012Validator(cs)
        diff = []
        fixtures = [("полный отчёт", GOOD)] + [
            (n, m(copy.deepcopy(GOOD))) for n, m, _, _ in BAD]
        for n, doc in fixtures:
            a = first_error(v, doc) is None
            b = first_error(cv, doc) is None
            if a != b:
                diff.append((n, a, b))
        if diff:
            for n, a, b in diff:
                print(f"  ВЕРДИКТЫ РАЗОШЛИСЬ  {n}: полная={'принят' if a else 'отклонён'}, "
                      f"компактная={'принят' if b else 'отклонён'}")
            failures += 1
        else:
            print(f"  вердикты совпали на всех {len(fixtures)} фикстурах "
                  f"(1 принят, {len(BAD)} отклонены)")

    print()
    if failures:
        print(f"ИТОГ: схема НЕ гейт — {failures} расхождений выше")
        return 1
    print(f"ИТОГ: схема гейт — принят 1 полный отчёт, отклонены все {len(BAD)} "
          f"негодных, каждый своим правилом; required сверен с блоком D; "
          f"компактная эквивалентна полной")
    return 0


if __name__ == "__main__":
    sys.exit(main())
