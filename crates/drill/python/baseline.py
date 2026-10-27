"""Offline external-tool driver: a reviewed adapter implements observe(requests).

Each conversation runs in a fresh process with private scratch and outbound I/O
blocked. This guards trusted tests, not hostile Python or native extensions.
"""
import contextlib
import json
import os
from pathlib import Path
import subprocess
import sys


def isolate(root):
    """Deny network/process activity and writes outside the owned scratch root.

    SQLite writes bypass Python open, so its connect audit is checked as well.
    This guards a trusted, separately reviewed reference, not hostile native code.
    """
    root = Path(root).resolve()
    violations = []

    def reject(event):
        violations.append(event)
        raise PermissionError('baseline isolation violation')

    def inside(path):
        return isinstance(path, (str, bytes, os.PathLike)) and Path(os.fsdecode(path)).resolve().is_relative_to(root)

    def audit(event, args):
        if event.startswith('socket.') or event in (
                'subprocess.Popen', 'os.system', 'os.exec', 'os.posix_spawn',
                'os.fork', 'os.forkpty', 'ctypes.dlopen', 'ctypes.dlsym'):
            reject(event)
        if event == 'open':
            path, mode, flags = args
            writing = (isinstance(mode, str) and any(c in mode for c in 'wax+')) or (flags & (os.O_WRONLY | os.O_RDWR | os.O_CREAT))
            if writing and not inside(path):
                reject(event)
        if event == 'sqlite3.connect' and not inside(args[0]):
            reject(event)
        if event in ('os.symlink', 'os.link', 'os.chdir'):
            reject(event)
        if event in ('os.remove', 'os.rmdir', 'os.mkdir', 'os.rename', 'os.chmod', 'os.chown', 'os.truncate', 'os.utime'):
            paths = args[:2] if event == 'os.rename' else args[:1]
            if not all(inside(path) for path in paths):
                reject(event)
    # Importing urllib may call getfqdn at module scope; supply a local constant,
    # while every actual DNS/socket audit event still fails.
    import socket
    socket.getfqdn = lambda _name='': 'drill.invalid'
    sys.addaudithook(audit)
    return violations


def child(reference, root):
    import runpy
    requests = json.load(sys.stdin)
    violations = isolate(root)
    with contextlib.redirect_stdout(sys.stderr):
        namespace = runpy.run_path(str(Path(reference) / 'adapter.py'))
        observe = namespace.get('observe')
        if not callable(observe):
            raise ValueError('external adapter must define observe(requests)')
        rows = observe(requests)
    if violations:
        raise RuntimeError('external adapter attempted forbidden activity')
    json.dump(rows, sys.stdout)


def run(reference, root):
    requests = json.load(sys.stdin)
    cases = {}
    for request in requests:
        cases.setdefault(request['case_id'], []).append(request)
    rows = []
    for index, case in enumerate(cases.values()):
        directory = Path(root) / f'case-{index}'
        directory.mkdir()
        env = {'PATH': os.defpath, 'LANG': 'C.UTF-8', 'HOME': str(directory),
               'TMPDIR': str(directory), 'PYTHONDONTWRITEBYTECODE': '1'}
        result = subprocess.run([sys.executable, '-I', '-B', __file__, '--child', reference, str(directory)],
            input=json.dumps(case), text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            cwd=directory, env=env, timeout=30, check=True)
        rows.extend(json.loads(result.stdout))
    json.dump(rows, sys.stdout)


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[1] == '--child':
        child(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 3:
        run(sys.argv[1], sys.argv[2])
    else:
        raise SystemExit('usage: baseline.py ADAPTER_DIRECTORY SCRATCH')
