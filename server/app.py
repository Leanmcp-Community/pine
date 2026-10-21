"""Single-node durable ingestion. Keys map SHA256(upload key) to organization."""
from contextlib import contextmanager
import hashlib
import hmac
import json
import os
import re
import sqlite3
import yaml
from pathlib import Path
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import Response

app = FastAPI()
DATA = Path(os.environ.get('PINE_DATA', '/data'))
KEY_FILE = os.environ.get('PINE_KEYS_FILE', '/run/secrets/keys.json')
CONFIG_FILE = os.environ.get('PINE_CONFIG', '/etc/pine/config.yaml')

def policy():
    try:
        raw = yaml.safe_load(Path(CONFIG_FILE).read_text())
        if not isinstance(raw, dict) or set(raw) != {'sync_enabled', 'max_session_mib', 'max_chunk_mib'}:
            raise ValueError('Expected sync_enabled, max_session_mib and max_chunk_mib')
        if type(raw['sync_enabled']) is not bool:
            raise ValueError('sync_enabled must be boolean')
        for key in ('max_session_mib', 'max_chunk_mib'):
            if type(raw[key]) is not int or not 1 <= raw[key] <= 1048576:
                raise ValueError('Size limits must be positive integers, at most 1048576 MiB')
        if raw['max_chunk_mib'] > 8:
            raise ValueError('max_chunk_mib cannot exceed the 8 MiB protocol ceiling')
        return {'sync_enabled':raw['sync_enabled'], 'max_session_bytes':raw['max_session_mib']*1048576, 'max_chunk_bytes':raw['max_chunk_mib']*1048576}
    except (OSError, ValueError, yaml.YAMLError):
        raise HTTPException(503, 'Server config.yaml is missing or invalid; contact the administrator')

@app.get('/v1/config')
def configuration(request: Request):
    organization(request)
    return policy()

@contextmanager
def db():
    DATA.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(DATA / 'capture.sqlite', timeout=30)
    conn.execute('PRAGMA journal_mode=WAL')
    conn.execute('PRAGMA synchronous=FULL')
    conn.execute('CREATE TABLE IF NOT EXISTS chunks (org TEXT, device TEXT, session TEXT, path TEXT, offset INTEGER, length INTEGER, checksum TEXT, hostname TEXT, data BLOB, PRIMARY KEY(org,device,session,path,offset,checksum))')
    try:
        with conn:
            yield conn
    finally:
        conn.close()

def organization(request):
    auth = request.headers.get('authorization', '')
    if not auth.startswith('Bearer '):
        raise HTTPException(401, 'Upload key required')
    supplied = hashlib.sha256(auth[7:].encode()).hexdigest()
    keys = json.loads(Path(KEY_FILE).read_text())
    for digest, org in keys.items():
        if hmac.compare_digest(supplied, digest):
            return org
    raise HTTPException(401, 'Invalid or revoked upload key')

def ident(value):
    if not re.fullmatch(r'[0-9A-HJKMNP-TV-Z]{26}', value):
        raise HTTPException(400, 'Invalid identifier')
    return value

def path_name(value):
    if not re.fullmatch(r'(meta\.json|traffic\.jsonl|(?:bodies|stream)/[A-Za-z0-9_-]+\.(?:req\.bin|res\.bin|sse\.jsonl|ws\.jsonl))', value):
        raise HTTPException(400, 'Invalid capture path')
    return value

@app.get('/health')
def health():
    with db() as conn:
        conn.execute('SELECT 1')
    return {'status': 'ok'}

@app.post('/v1/chunks')
async def upload(request: Request):
    org = organization(request)
    limits = policy()
    if not limits['sync_enabled']:
        raise HTTPException(403, 'Uploads paused by administrator')
    h = request.headers
    device, session = ident(h.get('x-device', '')), ident(h.get('x-session', ''))
    path = path_name(h.get('x-path', ''))
    try:
        offset, length = int(h['x-offset']), int(h['x-source-length'])
        if not 0 <= offset < 2**60 or not 0 < length <= limits['max_chunk_bytes']:
            raise ValueError()
    except (KeyError, ValueError):
        raise HTTPException(400, 'Invalid offset or length')
    data = bytearray()
    async for part in request.stream():
        data.extend(part)
        if len(data) > limits['max_chunk_bytes']:
            raise HTTPException(413, 'Chunk too large')
    checksum = hashlib.sha256(data).hexdigest()
    if checksum != h.get('x-sha256'):
        raise HTTPException(400, 'Checksum mismatch')
    with db() as conn:
        conn.execute('BEGIN IMMEDIATE')
        args = (org, device, session, path)
        existing = conn.execute('SELECT length, checksum FROM chunks WHERE org=? AND device=? AND session=? AND path=? AND offset=?', (*args, offset)).fetchall()
        if (length, checksum) in existing:
            return {'stored': True, 'duplicate': True}
        if path != 'meta.json':
            end = conn.execute('SELECT COALESCE(MAX(offset+length),0) FROM chunks WHERE org=? AND device=? AND session=? AND path=?', args).fetchone()[0]
            if offset != end:
                raise HTTPException(409, 'Unexpected source offset')
        elif offset != 0:
            raise HTTPException(400, 'Metadata offset must be zero')
        sizes = dict(conn.execute('SELECT path, MAX(offset+length) FROM chunks WHERE org=? AND device=? AND session=? GROUP BY path', (org, device, session)).fetchall())
        sizes[path] = max(sizes.get(path, 0), offset + length)
        if sum(sizes.values()) >= limits['max_session_bytes']:
            raise HTTPException(413, 'Session exceeds administrator size limit')
        conn.execute('INSERT INTO chunks VALUES (?,?,?,?,?,?,?,?,?)', (*args, offset, length, checksum, h.get('x-hostname', '')[:255], bytes(data)))
    return {'stored': True}

@app.get('/v1/sessions')
def sessions(request: Request, limit: int = 100, offset: int = 0):
    org = organization(request)
    if not 1 <= limit <= 1000 or offset < 0:
        raise HTTPException(400, 'Invalid pagination')
    with db() as conn:
        rows = conn.execute('SELECT device,session,MAX(hostname),SUM(LENGTH(data)) FROM chunks WHERE org=? GROUP BY device,session ORDER BY session DESC LIMIT ? OFFSET ?', (org, limit, offset)).fetchall()
    return [{'device': d, 'session': s, 'hostname': h, 'stored_bytes': n} for d,s,h,n in rows]

@app.get('/v1/files/{device}/{session}')
def files(device: str, session: str, request: Request):
    with db() as conn:
        rows = conn.execute('SELECT DISTINCT path FROM chunks WHERE org=? AND device=? AND session=? ORDER BY path', (organization(request), ident(device), ident(session))).fetchall()
    return [r[0] for r in rows]

@app.get('/v1/chunks/{device}/{session}')
def download(device: str, session: str, path: str, request: Request, offset: int = 0):
    # Return one bounded chunk at a time; source lengths differ after filtering.
    with db() as conn:
        row = conn.execute('SELECT data,length,checksum FROM chunks WHERE org=? AND device=? AND session=? AND path=? AND offset=? ORDER BY rowid DESC LIMIT 1', (organization(request), ident(device), ident(session), path_name(path), offset)).fetchone()
    if row is None:
        raise HTTPException(404, 'Chunk not found')
    return Response(row[0], media_type='application/octet-stream', headers={'X-Source-Length': str(row[1]), 'X-SHA256': row[2]})

@app.get('/')
def dashboard():
    from fastapi.responses import FileResponse
    return FileResponse(Path(__file__).with_name('index.html'), headers={'Cache-Control':'no-store'})

@app.get('/v1/summary')
def summary(request: Request):
    with db() as conn:
        org = organization(request)
        row = conn.execute('SELECT COUNT(DISTINCT device), COUNT(DISTINCT device || session), COUNT(*), COALESCE(SUM(LENGTH(data)),0) FROM chunks WHERE org=?', (org,)).fetchone()
    return dict(zip(['devices','sessions','chunks','stored_bytes'],row))
