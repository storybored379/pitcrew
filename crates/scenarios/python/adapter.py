"""Export a reviewed external tool revision without touching its working tree."""
import subprocess
from pathlib import Path


def export_reference(repository, revision, destination):
    """Export committed source without checkout, hooks, or writes to the reference.

    Resolve to a commit first so a caller's revision cannot become an archive option.
    Extraction rejects absolute paths, traversal, and links escaping the scratch root.
    """
    import io
    import tarfile
    import shutil
    from pathlib import PurePosixPath
    commit = subprocess.run(
        ['git', '-C', str(repository), 'rev-parse', '--verify', '--end-of-options',
         revision + '^{commit}'], check=True, capture_output=True, text=True,
    ).stdout.strip()
    archive = subprocess.run(
        ['git', '-C', str(repository), 'archive', '--format=tar', commit],
        check=True, capture_output=True,
    ).stdout
    root = Path(destination).resolve()
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as source:
        members = source.getmembers()
        # Accept source files/directories only. Reject links/devices even on older
        # Python versions without tarfile's extraction-filter API.
        for member in members:
            path = PurePosixPath(member.name)
            if path.is_absolute() or '..' in path.parts or not (member.isfile() or member.isdir()):
                raise ValueError('reference archive must contain confined regular files/directories')
        for member in members:
            target = root / member.name
            if not target.resolve().is_relative_to(root):
                raise ValueError('reference archive escaped scratch')
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with source.extractfile(member) as incoming, target.open('xb') as outgoing:
                    shutil.copyfileobj(incoming, outgoing)
    return commit
