#!/usr/bin/env python3
"""Seed the dev-local Paperless-ngx with a varied sample of real upstream docs.

Read-only against the upstream Paperless. NEVER writes back. The upstream API
token is read from the environment only — never written to disk, never logged.

Usage:
    cp deploy/compose/.env.dev-local.example deploy/compose/.env.dev-local
    docker compose -f deploy/compose/docker-compose.yml \
        -f deploy/compose/dev-local-docker-compose.yml \
        --env-file deploy/compose/.env.dev-local up -d
    ./scripts/dev-local-bootstrap.sh
    export SOURCE_PAPERLESS_URL=https://paperless.example.com
    export SOURCE_PAPERLESS_TOKEN=xxxxxxxxxxxxxxxxx
    python3 scripts/seed-from-prod.py --count 20

Selection strategy: query the upstream for the newest 200 documents, then pick
`count` documents striped across a varied bucket selection (different
correspondents, document types, page counts, and dates) so we don't accidentally
get 20 copies of the same template.

Each pulled document is downloaded into `dev-samples/` (gitignored) and
re-uploaded into the local Paperless via the upload endpoint with its original
title, correspondent name, document type name, tags and document date carried
over so the local archivist sees a realistic-looking inventory.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import pathlib
import re
import sys
import time
from typing import Any, Dict, List, Optional

import requests

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
DEV_SAMPLES = REPO_ROOT / "dev-samples"
STATE_FILE = REPO_ROOT / "scripts" / ".dev-local-state.json"
ENV_FILE = REPO_ROOT / "deploy" / "compose" / ".env.dev-local"


def log(msg: str) -> None:
    print(f"[seed-from-prod] {msg}", file=sys.stderr, flush=True)


def load_env() -> Dict[str, str]:
    env: Dict[str, str] = {}
    if ENV_FILE.exists():
        for line in ENV_FILE.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            v = v.strip().strip('"').strip("'")
            env[k.strip()] = v
    return env


@dataclasses.dataclass
class ProdDoc:
    id: int
    title: str
    correspondent: Optional[str]
    document_type: Optional[str]
    tags: List[str]
    created: Optional[str]
    page_count: Optional[int]
    original_file_name: Optional[str]


def list_prod_docs(base_url: str, token: str, max_docs: int) -> List[ProdDoc]:
    """Walk a few pages of the upstream inventory and return up to max_docs items."""
    headers = {"Authorization": f"Token {token}"}
    items: List[ProdDoc] = []
    correspondents: Dict[int, str] = {}
    document_types: Dict[int, str] = {}
    tags: Dict[int, str] = {}

    def fetch_paged(endpoint: str) -> Dict[int, str]:
        out: Dict[int, str] = {}
        url = f"{base_url}/api/{endpoint}/?page_size=200"
        while url:
            r = requests.get(url, headers=headers, timeout=30)
            r.raise_for_status()
            data = r.json()
            for row in data["results"]:
                out[row["id"]] = row["name"]
            url = data.get("next")
        return out

    correspondents = fetch_paged("correspondents")
    document_types = fetch_paged("document_types")
    tags = fetch_paged("tags")

    url = f"{base_url}/api/documents/?ordering=-id&page_size=100"
    while url and len(items) < max_docs:
        r = requests.get(url, headers=headers, timeout=30)
        r.raise_for_status()
        data = r.json()
        for row in data["results"]:
            items.append(
                ProdDoc(
                    id=row["id"],
                    title=row.get("title") or f"Document {row['id']}",
                    correspondent=correspondents.get(row.get("correspondent")),
                    document_type=document_types.get(row.get("document_type")),
                    tags=[tags[t] for t in row.get("tags", []) if t in tags],
                    created=row.get("created"),
                    page_count=row.get("page_count"),
                    original_file_name=row.get("original_file_name"),
                )
            )
        url = data.get("next")
        if len(items) >= max_docs:
            break
    return items


def varied_pick(docs: List[ProdDoc], count: int) -> List[ProdDoc]:
    """Pick `count` documents distributed across correspondent + type + date."""
    if len(docs) <= count:
        return docs
    by_correspondent: Dict[str, List[ProdDoc]] = {}
    for d in docs:
        key = (d.correspondent or "(none)") + "::" + (d.document_type or "(none)")
        by_correspondent.setdefault(key, []).append(d)
    picked: List[ProdDoc] = []
    keys = list(by_correspondent.keys())
    while len(picked) < count and any(by_correspondent[k] for k in keys):
        for k in keys:
            if not by_correspondent[k]:
                continue
            picked.append(by_correspondent[k].pop(0))
            if len(picked) >= count:
                break
    return picked


def slugify(value: str) -> str:
    value = re.sub(r"[^a-zA-Z0-9-]+", "-", value).strip("-").lower()
    return value[:60] or "untitled"


def download(base_url: str, token: str, doc: ProdDoc, dst_dir: pathlib.Path) -> pathlib.Path:
    dst_dir.mkdir(parents=True, exist_ok=True)
    headers = {"Authorization": f"Token {token}"}
    r = requests.get(
        f"{base_url}/api/documents/{doc.id}/download/",
        headers=headers,
        stream=True,
        timeout=120,
    )
    r.raise_for_status()
    suffix = ".pdf"
    if doc.original_file_name:
        ext = pathlib.Path(doc.original_file_name).suffix
        if ext:
            suffix = ext
    path = dst_dir / f"{doc.id}-{slugify(doc.title)}{suffix}"
    with path.open("wb") as fh:
        for chunk in r.iter_content(chunk_size=64 * 1024):
            fh.write(chunk)
    return path


def upload_to_local(local_url: str, local_token: str, doc: ProdDoc, path: pathlib.Path) -> str:
    headers = {"Authorization": f"Token {local_token}"}
    data: Dict[str, Any] = {"title": doc.title}
    if doc.created:
        data["created"] = doc.created
    files = {"document": (path.name, path.open("rb"), "application/octet-stream")}
    r = requests.post(
        f"{local_url}/api/documents/post_document/",
        headers=headers,
        data=data,
        files=files,
        timeout=300,
    )
    r.raise_for_status()
    return r.text.strip().strip('"')


def ensure_lookup(local_url: str, local_token: str, kind: str, name: str) -> Optional[int]:
    """Return the local id for a (correspondent|document_type|tag) name, creating it if needed."""
    headers = {"Authorization": f"Token {local_token}"}
    url = f"{local_url}/api/{kind}/?name__iexact={requests.utils.quote(name)}"
    r = requests.get(url, headers=headers, timeout=30)
    r.raise_for_status()
    data = r.json()
    if data.get("count"):
        return data["results"][0]["id"]
    r = requests.post(f"{local_url}/api/{kind}/", headers=headers, json={"name": name}, timeout=30)
    if r.status_code in (200, 201):
        return r.json()["id"]
    return None


def patch_local_metadata(
    local_url: str,
    local_token: str,
    title_or_id: str | int,
    doc: ProdDoc,
) -> None:
    """After ingest completes, patch the freshly-ingested local doc with original metadata."""
    headers = {"Authorization": f"Token {local_token}"}
    if isinstance(title_or_id, int):
        local_id = title_or_id
    else:
        return
    payload: Dict[str, Any] = {}
    if doc.correspondent:
        cid = ensure_lookup(local_url, local_token, "correspondents", doc.correspondent)
        if cid:
            payload["correspondent"] = cid
    if doc.document_type:
        dtid = ensure_lookup(local_url, local_token, "document_types", doc.document_type)
        if dtid:
            payload["document_type"] = dtid
    tag_ids = []
    for t in doc.tags:
        tid = ensure_lookup(local_url, local_token, "tags", t)
        if tid:
            tag_ids.append(tid)
    if tag_ids:
        payload["tags"] = tag_ids
    if not payload:
        return
    r = requests.patch(
        f"{local_url}/api/documents/{local_id}/",
        headers=headers,
        json=payload,
        timeout=30,
    )
    if not r.ok:
        log(f"warn: patch metadata for local doc {local_id} failed: {r.status_code} {r.text[:120]}")


def wait_for_local_ingest(
    local_url: str,
    local_token: str,
    task_uuids: List[str],
    timeout: int = 600,
) -> Dict[str, Optional[int]]:
    """Block until all upload tasks succeed; return task_uuid -> local document_id."""
    headers = {"Authorization": f"Token {local_token}"}
    pending = set(task_uuids)
    out: Dict[str, Optional[int]] = {}
    start = time.time()
    while pending and time.time() - start < timeout:
        r = requests.get(
            f"{local_url}/api/tasks/",
            headers=headers,
            timeout=30,
        )
        r.raise_for_status()
        for row in r.json():
            uid = row.get("task_id")
            if uid in pending and row.get("status") in ("SUCCESS", "FAILURE"):
                pending.discard(uid)
                if row.get("status") == "SUCCESS":
                    rd = row.get("related_document")
                    out[uid] = int(rd) if rd is not None else None
                else:
                    log(f"ingest task {uid} FAILED: {row.get('result')[:200] if row.get('result') else ''}")
                    out[uid] = None
        if pending:
            time.sleep(3)
    if pending:
        log(f"timeout: {len(pending)} ingest tasks did not complete in {timeout}s")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--count", type=int, default=20, help="number of documents to import")
    ap.add_argument(
        "--source-token",
        default=os.environ.get("SOURCE_PAPERLESS_TOKEN") or os.environ.get("PROD_PAPERLESS_TOKEN"),
        help="upstream Paperless API token (env SOURCE_PAPERLESS_TOKEN)",
    )
    args = ap.parse_args()

    env = load_env()
    source_url = (
        os.environ.get("SOURCE_PAPERLESS_URL")
        or env.get("SOURCE_PAPERLESS_URL")
        or env.get("PROD_PAPERLESS_URL")
    )
    if not source_url:
        log("SOURCE_PAPERLESS_URL not set — export it or put it in .env.dev-local")
        return 2
    if not args.source_token:
        log("SOURCE_PAPERLESS_TOKEN not set — export it in your shell, never commit it")
        return 2

    if not STATE_FILE.exists():
        log("scripts/.dev-local-state.json missing — run ./scripts/dev-local-bootstrap.sh first")
        return 2
    state = json.loads(STATE_FILE.read_text())
    local_url = state["paperless_url"]
    local_token = state["paperless_token"]

    source_token = args.source_token
    masked = source_token[:6] + "…"
    log(f"using upstream Paperless token (masked={masked})")

    # --- list & pick ---
    log("listing upstream documents ...")
    pool = list_prod_docs(source_url, source_token, max_docs=200)
    log(f"got {len(pool)} upstream documents from inventory")
    if not pool:
        log("no documents found in upstream Paperless")
        return 1
    picked = varied_pick(pool, args.count)
    log(f"selected {len(picked)} varied documents")
    for d in picked:
        log(f"  - #{d.id} '{d.title[:40]}' corr={d.correspondent} type={d.document_type} pages={d.page_count}")

    # --- download + upload ---
    task_uuid_to_doc: Dict[str, ProdDoc] = {}
    for doc in picked:
        local_path = download(source_url, source_token, doc, DEV_SAMPLES)
        log(f"downloaded #{doc.id} -> {local_path.relative_to(REPO_ROOT)} ({local_path.stat().st_size} bytes)")
        try:
            task_uuid = upload_to_local(local_url, local_token, doc, local_path)
        except requests.HTTPError as e:
            log(f"upload failed for #{doc.id}: {e.response.text[:200]}")
            continue
        log(f"  -> local Paperless task {task_uuid}")
        task_uuid_to_doc[task_uuid] = doc

    log("waiting for local Paperless to ingest all uploads ...")
    mapping = wait_for_local_ingest(local_url, local_token, list(task_uuid_to_doc.keys()))
    succeeded = [uuid for uuid, did in mapping.items() if did]
    log(f"ingest complete: {len(succeeded)}/{len(task_uuid_to_doc)} succeeded")

    log("patching metadata (correspondent / document_type / tags) for ingested docs ...")
    for uuid, local_id in mapping.items():
        if not local_id:
            continue
        doc = task_uuid_to_doc[uuid]
        patch_local_metadata(local_url, local_token, local_id, doc)

    log("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
