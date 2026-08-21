#!/usr/bin/env python3
"""Самопроверка docs/subagent-report.schema.json.

Схема, которую не проверили на ОТКЛОНЕНИИ, — это тавтология в другой форме: она
принимает всё и потому ничего не доказывает. Ровно тот же дефект, что тест,
который не может упасть (за пять спринтов — 10 раз).

Прогон делает три вещи:
  1. проверяет, что файл — валидная JSON Schema (Draft 2020-12);
  2. проводит через неё полный отчёт — он ОБЯЗАН быть принят;
  3. проводит десять заведомо неполных или подделанных отчётов — каждый ОБЯЗАН
     быть отклонён, и печатается сообщение, которым схема его отклонила.

    python3 docs/subagent-report.schema.check.py        # код выхода 0 = схема гейт

Блок ofgate в фикстуре GOOD — настоящий, не сочинённый: это прогон
/root/ofgate-canary от 2026-08-21, лежащий в
/var/tmp/ofgate/ofgate-canary-/20260821T105225Z-986317/verdict-body.txt, и его
sha256 (первые 32 знака) совпадает с тем, что стоит в закрывающей строке:

    $ sha256sum .../verdict-body.txt | cut -c1-32
    a98ee3e8eb6141e39b045d19a4ee5e77

Это и есть механика против пересказа: блок, написанный по памяти, такой строки
не воспроизводит, а процитированный перепроверяется одной командой.
"""
import copy
import json
import os
import sys

try:
    from jsonschema import Draft202012Validator
except ImportError:
    sys.exit("нужен python3-jsonschema (проверено на 4.10.3): pip install jsonschema")

SCHEMA_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                           "subagent-report.schema.json")

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
        "proof_output": "ДОКАЗАНО (RED-ASSERT)",
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
}

def drop(key):
    """Убрать поле. Именно функцией, а не `r.pop(k) and r`: pop пустого списка
    возвращает ложь, и такой отчёт превращался бы в пустой список — отклонялся,
    но не тем правилом, которое проверяют."""
    def f(r):
        r.pop(key, None)
        return r
    return f


def setk(**kw):
    def f(r):
        r.update(kw)
        return r
    return f


# (имя, мутация отчёта, что именно этот случай ловит)
BAD = [
    ("пустой отчёт",
     lambda r: {},
     "ни одного обязательного поля"),
    ("нет gate",
     drop("gate"),
     "ofgate не прогнан, и об этом молчат"),
    ("нет mutate",
     drop("mutate"),
     "молчание об ofmutate — ровно то, что случалось в спринтах 1-5"),
    ("нет red_before_green",
     drop("red_before_green"),
     "зелёный тест без записанного красного"),
    ("нет unverified",
     drop("unverified"),
     "самый часто пропускаемый пункт приёмки"),
    ("нет claims_without_proof",
     drop("claims_without_proof"),
     "фразы без прогона не перечислены"),
    ("gate пересказан по памяти",
     setk(gate="Прогнал ofgate, всё зелёное: fmt, clippy, test."),
     "нет закрывающей строки с body-sha256 — пересказ не проходит по форме"),
    ("gate обрезан до result",
     setk(gate=OFGATE_GREEN.split("=== END")[0]),
     "хвост со sha256 отрезан — блок больше не перепроверяем"),
    ("mutate прозой вместо вердикта",
     setk(mutate="ofmutate прогонял, тест краснеет без патча"),
     "нет ни '# HEAD', ни слова вердикта — доказательства нет"),
    ("mutate: not_applicable с опечаткой в ключе",
     setk(mutate={"not_aplicable": "правка только документации"}),
     "additionalProperties:false — опечатка не проскакивает как молчание"),
    ("unverified пуст без объяснения",
     setk(unverified=[]),
     "пустой список требует unverified_empty_because"),
    ("cleanup без личного префикса",
     setk(cleanup={"containers": [], "volumes": [], "worktree_left": False}),
     "чужие контейнеры дважды за период были прочитаны как находка"),
    ("модель не из таблицы",
     setk(model="opus-max"),
     "модель выбрана в моменте, а не по типу задачи"),
]


def first_error(validator, doc):
    errs = sorted(validator.iter_errors(doc), key=lambda e: list(e.absolute_path))
    if not errs:
        return None
    e = errs[0]
    where = "/".join(str(p) for p in e.absolute_path) or "<корень>"
    msg = e.message.replace("\n", " ")
    return f"{where}: {msg[:160]}"


def main():
    with open(SCHEMA_PATH, encoding="utf-8") as fh:
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

    print("[3] неполные отчёты — каждый обязан быть отклонён:")
    for name, mutate, why in BAD:
        doc = mutate(copy.deepcopy(GOOD))
        err = first_error(v, doc)
        if err is None:
            print(f"  ПРОПУЩЕН (это дефект схемы!)  {name}")
            failures += 1
        else:
            print(f"  отклонён  {name}")
            print(f"            → {err}")
            print(f"            ловит: {why}")

    print()
    if failures:
        print(f"ИТОГ: схема НЕ гейт — {failures} расхождений выше")
        return 1
    print(f"ИТОГ: схема гейт — принят 1 полный отчёт, отклонены все {len(BAD)} неполных")
    return 0


if __name__ == "__main__":
    sys.exit(main())
