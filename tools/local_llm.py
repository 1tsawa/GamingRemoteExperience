#!/usr/bin/env python3
"""Délègue une tâche de code au LLM local (llama.cpp, API compatible OpenAI).

Usage :
    python tools/local_llm.py --task "spec précise" [--context f1 f2 ...] [--out chemin]

- Sans --out : affiche le code extrait de la réponse.
- Avec --out : écrit le fichier (uniquement dans le dépôt) et affiche une ligne de résumé.

Variables d'environnement :
    LOCAL_LLM_KEY   jeton envoyé en "Authorization: Bearer" s'il est défini
    LOCAL_LLM_URL   URL de base (défaut : serveur du projet)
    LOCAL_LLM_MODEL nom du modèle (défaut : Qwen3.6-35B-A3B)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

DEFAULT_BASE_URL = "https://cnas-energia2.pro.dns-orange.fr:8443"
DEFAULT_MODEL = "Qwen3.6-35B-A3B"
TIMEOUT_SECONDS = 600
TEMPERATURE = 0.2
# Garde-fou : ~4 caractères par token, contexte de 64k tokens, on laisse de la place pour la réponse.
MAX_PROMPT_CHARS = 160_000

SYSTEM_PROMPT = (
    "Tu es un développeur senior. Code propre, typé, lisible, noms explicites.\n"
    "Respecte strictement le style et les conventions du contexte fourni.\n"
    "Réponds uniquement avec le code final dans un seul bloc de code."
)

REPO_ROOT = Path(__file__).resolve().parent.parent

CODE_BLOCK_RE = re.compile(r"```[^\n`]*\n(.*?)```", re.DOTALL)
THINK_RE = re.compile(r"<think>.*?</think>", re.DOTALL)


class LocalLlmError(Exception):
    """Erreur lisible destinée à l'utilisateur (pas de trace Python)."""


def build_user_prompt(task: str, context_files: list[Path]) -> str:
    parts = [f"# Tâche\n{task.strip()}\n"]
    for path in context_files:
        try:
            content = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            raise LocalLlmError(f"contexte illisible : {path} ({exc})") from exc
        try:
            shown = path.resolve().relative_to(REPO_ROOT).as_posix()
        except ValueError:
            shown = path.name
        parts.append(f"# Fichier de contexte : {shown}\n```\n{content}\n```\n")
    prompt = "\n".join(parts)
    if len(prompt) > MAX_PROMPT_CHARS:
        raise LocalLlmError(
            f"prompt trop long ({len(prompt)} caractères > {MAX_PROMPT_CHARS}) : "
            "réduisez les fichiers de contexte"
        )
    return prompt


def call_llm(user_prompt: str) -> str:
    base_url = os.environ.get("LOCAL_LLM_URL", DEFAULT_BASE_URL).rstrip("/")
    body = {
        "model": os.environ.get("LOCAL_LLM_MODEL", DEFAULT_MODEL),
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_prompt},
        ],
        "temperature": TEMPERATURE,
        "stream": False,
    }
    headers = {"Content-Type": "application/json"}
    api_key = os.environ.get("LOCAL_LLM_KEY")
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    request = urllib.request.Request(
        f"{base_url}/v1/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:300]
        raise LocalLlmError(f"le serveur a répondu HTTP {exc.code} : {detail}") from exc
    except urllib.error.URLError as exc:
        raise LocalLlmError(f"serveur injoignable ({base_url}) : {exc.reason}") from exc
    except TimeoutError as exc:
        raise LocalLlmError(f"pas de réponse après {TIMEOUT_SECONDS} s") from exc
    except json.JSONDecodeError as exc:
        raise LocalLlmError("réponse du serveur qui n'est pas du JSON") from exc
    try:
        return payload["choices"][0]["message"]["content"] or ""
    except (KeyError, IndexError, TypeError) as exc:
        raise LocalLlmError(f"réponse inattendue : {str(payload)[:300]}") from exc


def extract_code(answer: str) -> str:
    """Premier bloc de code ; si le modèle n'en a pas mis, la réponse entière (sans <think>)."""
    cleaned = THINK_RE.sub("", answer)
    match = CODE_BLOCK_RE.search(cleaned)
    code = match.group(1) if match else cleaned
    code = code.strip("\n")
    if not code.strip():
        raise LocalLlmError("réponse vide : aucun code produit")
    return code + "\n"


def resolve_output_path(raw: str) -> Path:
    target = (Path.cwd() / raw).resolve()
    try:
        target.relative_to(REPO_ROOT)
    except ValueError as exc:
        raise LocalLlmError(f"chemin refusé (hors du dépôt {REPO_ROOT}) : {target}") from exc
    return target


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Délègue une tâche de code au LLM local.")
    parser.add_argument("--task", required=True, help="spécification précise de la tâche")
    parser.add_argument("--context", nargs="*", default=[], type=Path, help="fichiers de contexte")
    parser.add_argument("--out", help="fichier à écrire (dans le dépôt uniquement)")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    # La console Windows n'est pas en UTF-8 par défaut : messages accentués lisibles partout.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    args = parse_args(argv)
    try:
        output_path = resolve_output_path(args.out) if args.out else None
        code = extract_code(call_llm(build_user_prompt(args.task, args.context)))
        if output_path is None:
            sys.stdout.write(code)
            return 0
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(code, encoding="utf-8", newline="\n")
        line_count = code.count("\n")
        print(f"{output_path.relative_to(REPO_ROOT).as_posix()} : {line_count} lignes")
        return 0
    except LocalLlmError as exc:
        print(f"local_llm : {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
