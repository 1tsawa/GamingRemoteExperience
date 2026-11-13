#!/usr/bin/env python3
"""Faux serveur HTTP imitant l'API GitHub Releases pour tests locaux."""

import argparse
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, List, Optional

def parse_arguments() -> argparse.Namespace:
    """Analyse les arguments de la ligne de commande."""
    parser = argparse.ArgumentParser(description="Serveur fictif GitHub Releases.")
    parser.add_argument("--release-dir", required=True, type=Path, help="Dossier des releases.")
    parser.add_argument("--host", default="127.0.0.1", help="Adresse d'écoute.")
    parser.add_argument("--port", default=0, type=int, help="Port d'écoute (0 pour port libre).")
    parser.add_argument("--port-file", type=Path, default=None, help="Fichier pour écrire le port réel.")
    return parser.parse_args()


def validate_host(host: str) -> str:
    """Valide que l'hôte est 127.0.0.1 ou ::1."""
    if host not in ("127.0.0.1", "::1"):
        print(
            f"Erreur : l'hôte '{host}' n'est pas autorisé. "
            f"Seuls 127.0.0.1 et ::1 sont acceptés.",
            file=sys.stderr,
        )
        sys.exit(2)
    return host


class FakeGitHubHandler(BaseHTTPRequestHandler):
    """Gestionnaire de requêtes pour le serveur fictif GitHub."""

    def __init__(self, request: Any, client_address: tuple, server: Any) -> None:
        self.release_dir: Path = server.release_dir
        self.host: str = server.host
        self.port: int = server.port
        self._status_code: int = 200
        super().__init__(request, client_address, server)

    def log_message(self, format: str, *args: Any) -> None:
        """Journalise la requête sur stderr au format spécifié."""
        method = self.command
        path = self.path
        ua = self.headers.get("User-Agent") or ""
        print(f"{method} {path} UA={ua} -> {self._status_code}", file=sys.stderr)

    def _send_json_response(self, status: int, data: Any) -> None:
        """Envoie une réponse JSON avec les en-têtes requis."""
        self._status_code = status
        body = json.dumps(data, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _send_raw_json_response(self, status: int, raw_bytes: bytes) -> None:
        """Envoie du contenu JSON brut."""
        self._status_code = status
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw_bytes)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(raw_bytes)

    def _send_file_response(self, status: int, file_path: Path) -> None:
        """Envoie un fichier binaire par blocs de 64 Kio."""
        self._status_code = status
        file_size = file_path.stat().st_size
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(file_size))
        self.send_header("Connection", "close")
        self.end_headers()
        with open(file_path, "rb") as f:
            while True:
                chunk = f.read(64 * 1024)
                if not chunk:
                    break
                self.wfile.write(chunk)

    def _send_404(self) -> None:
        """Envoie une réponse 404 JSON."""
        self._send_json_response(404, {"message": "Not Found"})

    def _send_403(self) -> None:
        """Envoie une réponse 403 JSON."""
        self._send_json_response(403, {"message": "User-Agent required"})

    def _format_host(self) -> str:
        """Formate « hôte:port » (crochets si IPv6)."""
        host = f"[{self.host}]" if ":" in self.host else self.host
        return f"{host}:{self.server.server_address[1]}"

    def do_GET(self) -> None:
        """Gère les requêtes GET."""
        ua = self.headers.get("User-Agent") or ""
        if not ua:
            self._send_403()
            return

        path = self.path

        # Route 1 : /repos/<owner>/<repo>/releases/latest
        match = re.match(r"^/repos/([^/]+)/([^/]+)/releases/latest$", path)
        if match:
            self._handle_releases_latest()
            return

        # Route 2 : /download/<nom>
        match = re.match(r"^/download/(.+)$", path)
        if match:
            self._handle_download(match.group(1))
            return

        # Toute autre route
        self._send_404()

    def _handle_releases_latest(self) -> None:
        """Gère la route /repos/<owner>/<repo>/releases/latest."""
        override_path = self.release_dir / "latest_override.json"
        if override_path.is_file():
            try:
                raw = override_path.read_bytes()
                self._send_raw_json_response(200, raw)
                return
            except Exception:
                pass

        manifest_path = self.release_dir / "manifest.json"
        version = "0.0.0"
        if manifest_path.is_file():
            try:
                data = json.loads(manifest_path.read_text(encoding="utf-8"))
                version = str(data.get("version", "0.0.0"))
            except (json.JSONDecodeError, ValueError):
                version = "0.0.0"

        assets: List[Dict[str, Any]] = []
        if self.release_dir.is_dir():
            for f in sorted(self.release_dir.iterdir(), key=lambda p: p.name):
                if f.is_file():
                    assets.append({
                        "name": f.name,
                        "size": f.stat().st_size,
                        "content_type": "application/octet-stream",
                        "browser_download_url": f"http://{self._format_host()}/download/{f.name}"
                    })

        response_data = {
            "tag_name": f"v{version}",
            "name": f"GamingRemoteExperience {version}",
            "draft": False,
            "prerelease": False,
            "published_at": "2026-01-01T00:00:00Z",
            "html_url": f"http://{self._format_host()}/release",
            "assets": assets
        }
        self._send_json_response(200, response_data)

    def _handle_download(self, filename: str) -> None:
        """Gère la route /download/<nom>."""
        if not re.match(r"^[A-Za-z0-9._-]+$", filename) or filename.startswith("."):
            self._send_404()
            return

        file_path = self.release_dir / filename
        if not file_path.is_file():
            self._send_404()
            return

        self._send_file_response(200, file_path)


def main() -> None:
    """Point d'entrée principal."""
    args = parse_arguments()
    host = validate_host(args.host)

    server = ThreadingHTTPServer((host, args.port), FakeGitHubHandler)
    server.release_dir = args.release_dir
    server.host = host
    server.port = args.port

    actual_port = server.server_address[1]

    if args.port_file:
        with open(args.port_file, "w") as f:
            f.write(str(actual_port))
            f.flush()

    print(f"fake_github: http://{host}:{actual_port}", flush=True)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
