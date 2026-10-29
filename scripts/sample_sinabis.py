#!/usr/bin/env python3
"""Fetch a reproducible, local-only sample of synthetic Sinabis invoice PDFs."""

import argparse
import hashlib
import json
import random
import shutil
import urllib.request
import zipfile
from pathlib import Path


REPO = "sinabis-group/sinabis-1m-synthetic-pdf"
REVISION = "92c46bb9a008cfb77cbfc9382fbf2ecfd434ded8"
BASE = f"https://huggingface.co/datasets/{REPO}/resolve/{REVISION}"
FIRST_ARCHIVE_LIMIT = 150_500


def request(url, start=None, end=None):
    headers = {"User-Agent": "docpipe-sinabis-sampler/1"}
    if start is not None:
        headers["Range"] = f"bytes={start}-{end}"
    return urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=120)


class RemoteZip:
    """A seekable ZIP reader backed by HTTP byte ranges."""

    def __init__(self, url):
        self.url = url
        with urllib.request.urlopen(urllib.request.Request(url, method="HEAD"), timeout=120) as response:
            self.size = int(response.headers["Content-Length"])
        self.position = 0
        self.cache_start = 0
        self.cache = b""

    def seekable(self):
        return True

    def readable(self):
        return True

    def tell(self):
        return self.position

    def seek(self, offset, whence=0):
        if whence == 1:
            offset += self.position
        elif whence == 2:
            offset += self.size
        if not 0 <= offset <= self.size:
            raise ValueError("seek outside ZIP")
        self.position = offset
        return offset

    def read(self, length=-1):
        if length < 0:
            length = self.size - self.position
        length = min(length, self.size - self.position)
        if not length:
            return b""
        if not self.cache_start <= self.position or self.position + length > self.cache_start + len(self.cache):
            # A small cache saves separate range requests for each member's header and body.
            fetch_length = max(length, 256 * 1024)
            end = min(self.position + fetch_length, self.size) - 1
            with request(self.url, self.position, end) as response:
                if response.status != 206:
                    raise RuntimeError("ZIP server did not honor HTTP Range")
                self.cache = response.read()
            if len(self.cache) != end - self.position + 1:
                raise RuntimeError("incomplete ZIP range")
            self.cache_start = self.position
        start = self.position - self.cache_start
        result = self.cache[start : start + length]
        self.position += len(result)
        return result


def download_manifest(path, url=None):
    if path.exists():
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(".download")
    try:
        with request(url or f"{BASE}/Sinabis_1M_pdfs_manifest.zip") as source, temp.open("wb") as target:
            shutil.copyfileobj(source, target)
        with zipfile.ZipFile(temp) as archive:
            if archive.testzip() is not None:
                raise RuntimeError("corrupt manifest ZIP")
        temp.replace(path)
    finally:
        temp.unlink(missing_ok=True)


def sample(manifest_path, count, seed, language):
    rng = random.Random(seed)
    buckets = {False: [], True: []}
    seen = {False: 0, True: 0}
    targets = {False: (count + 1) // 2, True: count // 2}
    with zipfile.ZipFile(manifest_path) as archive:
        for name in sorted(archive.namelist()):
            if not name.endswith(".jsonl"):
                continue
            with archive.open(name) as lines:
                for line in lines:
                    row = json.loads(line)
                    if row["idx"] >= FIRST_ARCHIVE_LIMIT:
                        break
                    if row["theme"] != "invoice" or row["lang"] != language:
                        continue
                    scanned = row["scanned"]
                    seen[scanned] += 1
                    bucket = buckets[scanned]
                    if len(bucket) < targets[scanned]:
                        bucket.append(row)
                    else:
                        slot = rng.randrange(seen[scanned])
                        if slot < targets[scanned]:
                            bucket[slot] = row
    if any(len(buckets[flag]) != targets[flag] for flag in (False, True)):
        raise RuntimeError(f"insufficient {language} invoices in first archive: {seen}")
    return sorted(buckets[False] + buckets[True], key=lambda row: row["idx"]), seen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--count", type=int, default=100)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--lang", default="de", help="ISO 639-1 language (default: de)")
    parser.add_argument("--output", type=Path, default=Path("benchmark-data/sinabis"))
    parser.add_argument("--manifest", type=Path, help="cache the large manifest outside --output")
    args = parser.parse_args()
    if args.count < 1:
        parser.error("--count must be positive")
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = args.manifest or args.output / "Sinabis_1M_pdfs_manifest.zip"
    download_manifest(manifest)
    rows, seen = sample(manifest, args.count, args.seed, args.lang)
    print(f"Eligible in part 01: {seen[False]} vector, {seen[True]} scanned")
    zip_url = f"{BASE}/Sinabis_1M_pdfs_part_01.zip"
    with zipfile.ZipFile(RemoteZip(zip_url)) as archive:
        names = set(archive.namelist())
        for row in rows:
            if row["file"] not in names:
                raise RuntimeError(f"missing ZIP entry: {row['file']}")
        index_path = args.output / "selection.jsonl"
        with index_path.open("w") as index:
            for number, row in enumerate(rows, 1):
                destination = args.output / row["file"]
                destination.parent.mkdir(parents=True, exist_ok=True)
                if not destination.exists() or destination.stat().st_size != row["bytes"]:
                    with archive.open(row["file"]) as source, destination.open("wb") as target:
                        shutil.copyfileobj(source, target)
                if destination.stat().st_size != row["bytes"]:
                    raise RuntimeError(f"size mismatch: {row['file']}")
                digest = hashlib.sha256(destination.read_bytes()).hexdigest()
                record = {key: row[key] for key in ("idx", "file", "lang", "scanned", "number", "date", "currency", "total", "bytes")}
                record["sha256"] = digest
                index.write(json.dumps(record, ensure_ascii=False) + "\n")
                print(f"{number}/{len(rows)} {row['file']}", end="\r", flush=True)
    print(f"\nSaved {len(rows)} PDFs and selection.jsonl in {args.output}")


if __name__ == "__main__":
    main()
